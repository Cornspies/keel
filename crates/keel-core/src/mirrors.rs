//! Pull mirrors: repositories kept in sync with one hosted elsewhere, by a
//! background worker. Mirrors can't be pushed to, merged into or edited.
//!
//! Fetching from a URL that people enter is a way into the server's network,
//! so it gets the same protection as webhooks (see `webhooks.rs`): only
//! http(s), and unless private addresses are allowed, the host must resolve
//! to public addresses only. git is then pinned to the checked address and
//! may not follow redirects.

use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use crate::{Core, Error, Repo, Result, User, invalid, is_private_ip, now};

/// How often a mirror can sync: (seconds, label).
pub const MIRROR_INTERVALS: &[(i64, &str)] =
    &[(3600, "every hour"), (4 * 3600, "every 4 hours"), (8 * 3600, "every 8 hours"), (86_400, "every day")];
pub(crate) const FETCH_LIMIT: Duration = Duration::from_secs(600);
const MAX_PUSH_MIRRORS: i64 = 10;
/// Delays before retrying a failed push.
const PUSH_RETRY_SECS: &[i64] = &[60, 300, 1800, 7200, 21_600];
/// After a failure, try again within this long at the latest.
const RETRY_SECS: i64 = 3600;

/// Where a mirror fetches from or pushes to, with credentials.
#[derive(Clone)]
pub struct MirrorRemote {
    pub url: String,
    pub username: String,
    pub password: String,
}

/// A remote that gets every change pushed.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct PushMirror {
    pub id: i64,
    pub repo_id: i64,
    pub url: String,
    pub username: String,
    pub has_password: bool,
    /// Changes are waiting to be pushed.
    pub pending: bool,
    pub next_attempt_at: i64,
    pub attempts: i64,
    pub pushed_at: Option<i64>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct Mirror {
    pub repo_id: i64,
    /// Without credentials.
    pub url: String,
    pub username: String,
    pub has_password: bool,
    pub interval_secs: i64,
    pub synced_at: Option<i64>,
    pub next_sync_at: i64,
    pub last_error: Option<String>,
}

/// An http(s) URL taken apart.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct MirrorUrl {
    /// The URL without credentials.
    pub url: String,
    pub host: String,
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
}

pub(crate) fn parse_mirror_url(url: &str) -> Result<MirrorUrl> {
    let url = url.trim();
    let bad = || invalid("Enter an http:// or https:// URL, like https://example.com/team/project.git.");
    let (scheme, rest) = match url.split_once("://") {
        Some((s, r)) if s.eq_ignore_ascii_case("https") || s.eq_ignore_ascii_case("http") => (s.to_ascii_lowercase(), r),
        _ => return bad(),
    };
    if url.len() > 2000 || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return bad();
    }
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let (userinfo, hostport) = match authority.rsplit_once('@') {
        Some((u, h)) => (Some(u), h),
        None => (None, authority),
    };
    let decode = |s: &str| form_urlencoded::parse(format!("x={s}").as_bytes()).next().map(|(_, v)| v.into_owned());
    let (username, password) = match userinfo {
        Some(u) => match u.split_once(':') {
            Some((name, pass)) => (decode(name), decode(pass)),
            None => (decode(u), None),
        },
        None => (None, None),
    };
    let default_port = if scheme == "https" { 443 } else { 80 };
    let (host, port) = if let Some(v6) = hostport.strip_prefix('[') {
        let (h, after) = v6.split_once(']').ok_or(Error::Invalid("Invalid address.".into()))?;
        let port = match after.strip_prefix(':') {
            Some(p) => p.parse().map_err(|_| Error::Invalid("Invalid port.".into()))?,
            None => default_port,
        };
        (h.to_owned(), port)
    } else {
        match hostport.rsplit_once(':') {
            Some((h, p)) => (h.to_owned(), p.parse().map_err(|_| Error::Invalid("Invalid port.".into()))?),
            None => (hostport.to_owned(), default_port),
        }
    };
    if host.is_empty() {
        return bad();
    }
    let shown_host = if host.contains(':') { format!("[{host}]") } else { host.clone() };
    let port_part = if port == default_port { String::new() } else { format!(":{port}") };
    Ok(MirrorUrl { url: format!("{scheme}://{shown_host}{port_part}{path}"), host, port, username, password })
}

impl Core {
    pub fn mirror_wakeup(&self) -> Arc<tokio::sync::Notify> {
        self.mirror_wakeup.clone()
    }

    /// Creates a repository that mirrors `url`. Credentials can be in the
    /// URL or given separately; either way they're stored apart from it.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_mirror(
        &self,
        owner: &User,
        name: &str,
        description: &str,
        is_private: bool,
        url: &str,
        username: &str,
        password: &str,
        interval_secs: i64,
    ) -> Result<Repo> {
        let parsed = parse_mirror_url(url)?;
        self.check_interval(interval_secs)?;
        self.check_mirror_host(&parsed).await?;
        let repo = self.create_repo(owner, name, description, is_private).await?;
        let username = parsed.username.clone().unwrap_or_else(|| username.trim().to_owned());
        let password = parsed.password.clone().unwrap_or_else(|| password.to_owned());
        sqlx::query(
            "INSERT INTO mirrors (repo_id, url, username, password, interval_secs, next_sync_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
        )
        .bind(repo.id)
        .bind(&parsed.url)
        .bind(username)
        .bind(password)
        .bind(interval_secs)
        .bind(now())
        .execute(&self.db)
        .await?;
        self.mirror_wakeup.notify_one();
        Ok(repo)
    }

    fn check_interval(&self, secs: i64) -> Result<()> {
        if MIRROR_INTERVALS.iter().any(|(s, _)| *s == secs) { Ok(()) } else { invalid("Choose how often to sync.") }
    }

    /// The addresses `url` may be fetched from (all of its host's, which
    /// must be public unless private addresses are allowed).
    pub(crate) async fn check_mirror_host(&self, url: &MirrorUrl) -> Result<Vec<IpAddr>> {
        let addrs: Vec<IpAddr> = match url.host.parse::<IpAddr>() {
            Ok(ip) => vec![ip],
            Err(_) => tokio::net::lookup_host((url.host.as_str(), url.port))
                .await
                .map_err(|_| Error::Invalid(format!("{} can't be found.", url.host)))?
                .map(|a| a.ip())
                .collect(),
        };
        if addrs.is_empty() {
            return invalid(format!("{} can't be found.", url.host));
        }
        if !self.config.webhook_allow_private && addrs.iter().any(|ip| is_private_ip(*ip)) {
            return invalid("Mirrors can't fetch from private network addresses on this server.");
        }
        Ok(addrs)
    }

    pub async fn mirror(&self, repo_id: i64) -> Result<Option<Mirror>> {
        Ok(sqlx::query_as(
            "SELECT repo_id, url, username, password != '' AS has_password, interval_secs, synced_at, next_sync_at,
                last_error
             FROM mirrors WHERE repo_id = ?1",
        )
        .bind(repo_id)
        .fetch_optional(&self.db)
        .await?)
    }

    pub async fn is_mirror(&self, repo_id: i64) -> Result<bool> {
        Ok(self.mirror(repo_id).await?.is_some())
    }

    /// Changes where and how often a mirror syncs. `password` `None` keeps
    /// the stored one.
    pub async fn update_mirror(&self, repo_id: i64, url: &str, username: &str, password: Option<&str>, interval_secs: i64) -> Result<()> {
        let parsed = parse_mirror_url(url)?;
        self.check_interval(interval_secs)?;
        self.check_mirror_host(&parsed).await?;
        let username = parsed.username.clone().unwrap_or_else(|| username.trim().to_owned());
        let password = parsed.password.as_deref().or(password);
        sqlx::query(
            "UPDATE mirrors SET url = ?2, username = ?3, password = COALESCE(?4, password), interval_secs = ?5,
                next_sync_at = MIN(next_sync_at, ?6 + ?5)
             WHERE repo_id = ?1",
        )
        .bind(repo_id)
        .bind(&parsed.url)
        .bind(username)
        .bind(password)
        .bind(interval_secs)
        .bind(now())
        .execute(&self.db)
        .await?;
        Ok(())
    }

    /// Turns a mirror into an ordinary repository.
    pub async fn stop_mirroring(&self, repo_id: i64) -> Result<()> {
        sqlx::query("DELETE FROM mirrors WHERE repo_id = ?1").bind(repo_id).execute(&self.db).await?;
        Ok(())
    }

    /// Asks the worker to sync a mirror now.
    pub async fn request_mirror_sync(&self, repo_id: i64) -> Result<()> {
        sqlx::query("UPDATE mirrors SET next_sync_at = ?2 WHERE repo_id = ?1")
            .bind(repo_id)
            .bind(now())
            .execute(&self.db)
            .await?;
        self.mirror_wakeup.notify_one();
        Ok(())
    }

    /// Claims the mirror most overdue for a sync, if any.
    pub async fn claim_mirror(&self) -> Result<Option<i64>> {
        let now = now();
        let row: Option<(i64,)> = sqlx::query_as(
            "UPDATE mirrors SET next_sync_at = ?1 + 3600
             WHERE repo_id = (SELECT repo_id FROM mirrors WHERE next_sync_at <= ?1
                                AND repo_id IN (SELECT id FROM repositories WHERE archived_at IS NULL)
                              ORDER BY next_sync_at LIMIT 1)
             RETURNING repo_id",
        )
        .bind(now)
        .fetch_optional(&self.db)
        .await?;
        Ok(row.map(|(id,)| id))
    }

    /// When the next mirror is due.
    pub async fn next_mirror_due(&self) -> Result<Option<i64>> {
        let (t,): (Option<i64>,) = sqlx::query_as(
            "SELECT MIN(next_sync_at) FROM mirrors WHERE repo_id IN (SELECT id FROM repositories WHERE archived_at IS NULL)",
        )
        .fetch_one(&self.db)
        .await?;
        Ok(t)
    }

    /// Where a mirror fetches from, if it's still a mirror.
    pub async fn mirror_remote(&self, repo_id: i64) -> Result<Option<MirrorRemote>> {
        let row: Option<(String, String, String)> =
            sqlx::query_as("SELECT url, username, password FROM mirrors WHERE repo_id = ?1")
                .bind(repo_id)
                .fetch_optional(&self.db)
                .await?;
        Ok(row.map(|(url, username, password)| MirrorRemote { url, username, password }))
    }

    /// Fetches a mirror's branches and tags, then handles what changed
    /// like a push (webhooks, search, caches). Returns where it fetched from
    /// and the LFS files it needs (those the new commits point to, and
    /// earlier ones still missing); `None` if it isn't a mirror (any more).
    /// Errors are for the mirror's page.
    pub async fn fetch_mirror(&self, repo_id: i64) -> Result<Option<(MirrorRemote, Vec<(String, u64)>)>> {
        let Some(remote) = self.mirror_remote(repo_id).await? else { return Ok(None) };
        let before = self.ref_snapshot(repo_id).await?;
        let path = self.config.repo_path(repo_id);
        self.with_remote(&remote, move |r| keel_git::mirror_fetch(&path, r, FETCH_LIMIT)).await?;
        let after = self.ref_snapshot(repo_id).await?;
        let changes: Vec<_> = keel_git::ref_changes(&before, &after)
            .into_iter()
            .filter_map(|c| Some((c.old, c.new?)))
            .collect();
        let mut wanted = self.lfs_pending(repo_id).await?;
        if !changes.is_empty() {
            wanted.extend(self.git(repo_id, move |g| g.lfs_pointers(&changes)).await?);
            self.after_push(repo_id, None, Some(before)).await?;
        }
        wanted.sort();
        wanted.dedup();
        let mut missing = Vec::new();
        for (oid, size) in wanted {
            if self.lfs_object(repo_id, &oid).await?.is_none() {
                missing.push((oid, size));
            }
        }
        Ok(Some((remote, missing)))
    }

    async fn lfs_pending(&self, repo_id: i64) -> Result<Vec<(String, u64)>> {
        let (json,): (String,) = sqlx::query_as("SELECT lfs_pending FROM mirrors WHERE repo_id = ?1")
            .bind(repo_id)
            .fetch_one(&self.db)
            .await?;
        Ok(serde_json::from_str(&json).unwrap_or_default())
    }

    /// Records a sync's outcome: LFS files still missing (`None`: as
    /// before), and an error.
    pub async fn finish_mirror_sync(&self, repo_id: i64, lfs_missing: Option<&[(String, u64)]>, error: Option<String>) -> Result<()> {
        let (interval,): (i64,) = match sqlx::query_as("SELECT interval_secs FROM mirrors WHERE repo_id = ?1")
            .bind(repo_id)
            .fetch_optional(&self.db)
            .await?
        {
            Some(row) => row,
            None => return Ok(()),
        };
        let now = now();
        let next = if error.is_some() { now + interval.min(RETRY_SECS) } else { now + interval };
        sqlx::query(
            "UPDATE mirrors SET next_sync_at = ?2, last_error = ?3, lfs_pending = COALESCE(?5, lfs_pending),
                synced_at = CASE WHEN ?3 IS NULL THEN ?4 ELSE synced_at END
             WHERE repo_id = ?1",
        )
        .bind(repo_id)
        .bind(next)
        .bind(&error)
        .bind(now)
        .bind(match lfs_missing {
            Some(m) => Some(serde_json::to_string(m).map_err(anyhow::Error::from)?),
            None => None,
        })
        .execute(&self.db)
        .await?;
        self.metrics.mirror_run("pull", error.is_none());
        if let Some(e) = error {
            tracing::info!(repo = repo_id, "mirror sync failed: {e}");
        }
        Ok(())
    }

    /// Fetches and records the outcome, without LFS files (for tests and
    /// callers without an HTTP client).
    pub async fn sync_mirror(&self, repo_id: i64) -> Result<()> {
        match self.fetch_mirror(repo_id).await {
            Ok(Some((_, missing))) => self.finish_mirror_sync(repo_id, Some(&missing), None).await,
            Ok(None) => Ok(()),
            Err(Error::Invalid(e)) => self.finish_mirror_sync(repo_id, None, Some(e)).await,
            Err(e) => self.finish_mirror_sync(repo_id, None, Some(e.to_string())).await,
        }
    }

    /// Runs `f` (git, blocking) against a remote after checking its host,
    /// pinned to a checked address. Errors are for people to read.
    pub(crate) async fn with_remote<T: Send + 'static>(
        &self,
        remote: &MirrorRemote,
        f: impl FnOnce(&keel_git::Remote<'_>) -> keel_git::Result<T> + Send + 'static,
    ) -> Result<T> {
        let parsed = parse_mirror_url(&remote.url)?;
        let addrs = self.check_mirror_host(&parsed).await?;
        let (user, pass) = (remote.username.clone(), remote.password.clone());
        let (host, port, ip) = (parsed.host.clone(), parsed.port, addrs[0]);
        let url = parsed.url.clone();
        let result = tokio::task::spawn_blocking(move || {
            let r = keel_git::Remote {
                url: &url,
                auth: (!user.is_empty() || !pass.is_empty()).then_some((user.as_str(), pass.as_str())),
                pin: Some((host.as_str(), port, ip)),
            };
            f(&r)
        })
        .await
        .map_err(anyhow::Error::from)?;
        result.map_err(|e| Error::Invalid(e.to_string()))
    }

    // -- Push mirrors -------------------------------------------------------

    pub async fn push_mirrors(&self, repo_id: i64) -> Result<Vec<PushMirror>> {
        Ok(sqlx::query_as(
            "SELECT id, repo_id, url, username, password != '' AS has_password, pending, next_attempt_at, attempts,
                pushed_at, last_error
             FROM push_mirrors WHERE repo_id = ?1 ORDER BY id",
        )
        .bind(repo_id)
        .fetch_all(&self.db)
        .await?)
    }

    /// Adds a remote that every change is pushed to (starting now).
    pub async fn add_push_mirror(&self, repo_id: i64, url: &str, username: &str, password: &str) -> Result<i64> {
        let parsed = parse_mirror_url(url)?;
        self.check_mirror_host(&parsed).await?;
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM push_mirrors WHERE repo_id = ?1")
            .bind(repo_id)
            .fetch_one(&self.db)
            .await?;
        if count >= MAX_PUSH_MIRRORS {
            return invalid(format!("A repository can push to at most {MAX_PUSH_MIRRORS} mirrors."));
        }
        let username = parsed.username.clone().unwrap_or_else(|| username.trim().to_owned());
        let password = parsed.password.clone().unwrap_or_else(|| password.to_owned());
        let (id,): (i64,) = sqlx::query_as(
            "INSERT INTO push_mirrors (repo_id, url, username, password, next_attempt_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5) RETURNING id",
        )
        .bind(repo_id)
        .bind(&parsed.url)
        .bind(username)
        .bind(password)
        .bind(now())
        .fetch_one(&self.db)
        .await?;
        self.mirror_wakeup.notify_one();
        Ok(id)
    }

    pub async fn delete_push_mirror(&self, repo_id: i64, id: i64) -> Result<()> {
        sqlx::query("DELETE FROM push_mirrors WHERE repo_id = ?1 AND id = ?2").bind(repo_id).bind(id).execute(&self.db).await?;
        Ok(())
    }

    /// Asks for a push now (also resetting the backoff after failures).
    pub async fn request_push(&self, repo_id: i64, id: i64) -> Result<()> {
        sqlx::query("UPDATE push_mirrors SET pending = 1, next_attempt_at = ?3 WHERE repo_id = ?1 AND id = ?2")
            .bind(repo_id)
            .bind(id)
            .bind(now())
            .execute(&self.db)
            .await?;
        self.mirror_wakeup.notify_one();
        Ok(())
    }

    /// After refs changed: the repository's push mirrors have something to
    /// push. Ones backing off after failures keep their schedule.
    pub(crate) async fn push_mirrors_pending(&self, repo_id: i64) -> Result<()> {
        let done = sqlx::query("UPDATE push_mirrors SET pending = 1, next_attempt_at = ?2 WHERE repo_id = ?1 AND pending = 0")
            .bind(repo_id)
            .bind(now())
            .execute(&self.db)
            .await?;
        if done.rows_affected() > 0 {
            self.mirror_wakeup.notify_one();
        }
        Ok(())
    }

    /// Claims the push mirror most overdue, if any: (id, repository).
    pub async fn claim_push_mirror(&self) -> Result<Option<(i64, i64)>> {
        let now = now();
        // Not pending while it runs, so changes meanwhile make it pending again.
        Ok(sqlx::query_as(
            "UPDATE push_mirrors SET pending = 0, next_attempt_at = ?1 + 1800
             WHERE id = (SELECT id FROM push_mirrors WHERE pending = 1 AND next_attempt_at <= ?1
                         ORDER BY next_attempt_at LIMIT 1)
             RETURNING id, repo_id",
        )
        .bind(now)
        .fetch_optional(&self.db)
        .await?)
    }

    pub async fn push_mirror_remote(&self, id: i64) -> Result<Option<MirrorRemote>> {
        let row: Option<(String, String, String)> = sqlx::query_as("SELECT url, username, password FROM push_mirrors WHERE id = ?1")
            .bind(id)
            .fetch_optional(&self.db)
            .await?;
        Ok(row.map(|(url, username, password)| MirrorRemote { url, username, password }))
    }

    /// The LFS files to upload before pushing: those that commits the
    /// remote doesn't have point to, and that are stored here.
    pub async fn lfs_files_to_push(&self, repo_id: i64, remote: &MirrorRemote) -> Result<Vec<(String, u64)>> {
        let path = self.config.repo_path(repo_id);
        let theirs = self.with_remote(remote, move |r| keel_git::ls_remote(&path, r)).await?;
        let ours = self.ref_snapshot(repo_id).await?;
        let changes: Vec<_> = ours
            .iter()
            .filter(|(name, id)| theirs.get(*name) != Some(id))
            .map(|(name, id)| (theirs.get(name).copied(), *id))
            .collect();
        if changes.is_empty() {
            return Ok(Vec::new());
        }
        let pointers = self.git(repo_id, move |g| g.lfs_pointers(&changes)).await?;
        let mut out = Vec::new();
        for (oid, size) in pointers {
            if self.lfs_object(repo_id, &oid).await?.is_some() {
                out.push((oid, size));
            }
        }
        Ok(out)
    }

    /// Pushes branches and tags to a push mirror.
    pub async fn push_to_mirror(&self, repo_id: i64, remote: &MirrorRemote) -> Result<()> {
        let path = self.config.repo_path(repo_id);
        self.with_remote(remote, move |r| keel_git::mirror_push(&path, r, FETCH_LIMIT)).await
    }

    /// Records a push's outcome; failures are retried with growing delays.
    pub async fn finish_push(&self, id: i64, error: Option<String>) -> Result<()> {
        let now = now();
        match &error {
            None => {
                sqlx::query("UPDATE push_mirrors SET attempts = 0, pushed_at = ?2, last_error = NULL WHERE id = ?1")
                    .bind(id)
                    .bind(now)
                    .execute(&self.db)
                    .await?;
            }
            Some(e) => {
                let (attempts,): (i64,) = sqlx::query_as("SELECT attempts FROM push_mirrors WHERE id = ?1")
                    .bind(id)
                    .fetch_optional(&self.db)
                    .await?
                    .unwrap_or((0,));
                let delay = PUSH_RETRY_SECS[(attempts as usize).min(PUSH_RETRY_SECS.len() - 1)];
                sqlx::query(
                    "UPDATE push_mirrors SET pending = 1, attempts = attempts + 1, next_attempt_at = ?2, last_error = ?3
                     WHERE id = ?1",
                )
                .bind(id)
                .bind(now + delay)
                .bind(e)
                .execute(&self.db)
                .await?;
                tracing::info!(push_mirror = id, "push mirror failed: {e}");
            }
        }
        self.metrics.mirror_run("push", error.is_none());
        Ok(())
    }

    /// When the next pull or push mirror is due.
    pub async fn next_mirror_work(&self) -> Result<Option<i64>> {
        let (t,): (Option<i64>,) = sqlx::query_as(
            "SELECT MIN(t) FROM (SELECT MIN(next_sync_at) AS t FROM mirrors
                                   WHERE repo_id IN (SELECT id FROM repositories WHERE archived_at IS NULL)
                                 UNION ALL SELECT MIN(next_attempt_at) FROM push_mirrors WHERE pending = 1)",
        )
        .fetch_one(&self.db)
        .await?;
        Ok(t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        let u = parse_mirror_url("https://bob:s%40cret@git.example.com/team/x.git").unwrap();
        assert_eq!(u.url, "https://git.example.com/team/x.git");
        assert_eq!((u.username.as_deref(), u.password.as_deref()), (Some("bob"), Some("s@cret")));
        assert_eq!((u.host.as_str(), u.port), ("git.example.com", 443));
        let u = parse_mirror_url("http://[::1]:8080/x").unwrap();
        assert_eq!((u.host.as_str(), u.port, u.url.as_str()), ("::1", 8080, "http://[::1]:8080/x"));
        assert_eq!(parse_mirror_url("HTTPS://h:443/a").unwrap().url, "https://h/a");
        for bad in ["git@github.com:a/b.git", "file:///etc", "/srv/repo.git", "ssh://h/x", "https://", "https://h/a b"] {
            assert!(parse_mirror_url(bad).is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn mirrors_and_private_addresses() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let err = core.create_mirror(&ada, "m", "", false, "http://127.0.0.1:9/x.git", "", "", 3600).await.unwrap_err();
        assert!(err.to_string().contains("private network"), "{err}");
        assert!(core.create_mirror(&ada, "m", "", false, "http://10.1.2.3/x", "", "", 3600).await.is_err());
        assert!(core.create_mirror(&ada, "m", "", false, "https://192.0.2.1/x", "", "", 5).await.is_err(), "interval");
        assert!(core.repo("ada", "m").await.unwrap().is_none(), "nothing created");
    }
}
