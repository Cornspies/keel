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
const FETCH_LIMIT: Duration = Duration::from_secs(600);
/// After a failure, try again within this long at the latest.
const RETRY_SECS: i64 = 3600;

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
    async fn check_mirror_host(&self, url: &MirrorUrl) -> Result<Vec<IpAddr>> {
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
             WHERE repo_id = (SELECT repo_id FROM mirrors WHERE next_sync_at <= ?1 ORDER BY next_sync_at LIMIT 1)
             RETURNING repo_id",
        )
        .bind(now)
        .fetch_optional(&self.db)
        .await?;
        Ok(row.map(|(id,)| id))
    }

    /// When the next mirror is due.
    pub async fn next_mirror_due(&self) -> Result<Option<i64>> {
        let (t,): (Option<i64>,) = sqlx::query_as("SELECT MIN(next_sync_at) FROM mirrors").fetch_one(&self.db).await?;
        Ok(t)
    }

    /// Fetches a mirror's branches and tags from its remote, then handles
    /// what changed like a push (webhooks, search, caches).
    pub async fn sync_mirror(&self, repo_id: i64) -> Result<()> {
        let row: Option<(String, String, String, i64)> =
            sqlx::query_as("SELECT url, username, password, interval_secs FROM mirrors WHERE repo_id = ?1")
                .bind(repo_id)
                .fetch_optional(&self.db)
                .await?;
        let Some((url, username, password, interval)) = row else { return Ok(()) };
        let result = self.fetch_mirror(repo_id, &url, &username, &password).await;
        let now = now();
        let (next, error) = match &result {
            Ok(()) => (now + interval, None),
            Err(e) => (now + interval.min(RETRY_SECS), Some(e.to_string())),
        };
        sqlx::query(
            "UPDATE mirrors SET next_sync_at = ?2, last_error = ?3,
                synced_at = CASE WHEN ?3 IS NULL THEN ?4 ELSE synced_at END
             WHERE repo_id = ?1",
        )
        .bind(repo_id)
        .bind(next)
        .bind(&error)
        .bind(now)
        .execute(&self.db)
        .await?;
        if let Some(e) = error {
            tracing::info!(repo = repo_id, "mirror sync failed: {e}");
        }
        Ok(())
    }

    async fn fetch_mirror(&self, repo_id: i64, url: &str, username: &str, password: &str) -> Result<()> {
        let parsed = parse_mirror_url(url)?;
        let addrs = self.check_mirror_host(&parsed).await?;
        let before = self.ref_snapshot(repo_id).await?;
        let path = self.config.repo_path(repo_id);
        let (url, user, pass) = (parsed.url.clone(), username.to_owned(), password.to_owned());
        let (host, port, ip) = (parsed.host.clone(), parsed.port, addrs[0]);
        let fetched = tokio::task::spawn_blocking(move || {
            let remote = keel_git::Remote {
                url: &url,
                auth: (!user.is_empty() || !pass.is_empty()).then_some((user.as_str(), pass.as_str())),
                pin: Some((host.as_str(), port, ip)),
            };
            keel_git::mirror_fetch(&path, &remote, FETCH_LIMIT)
        })
        .await
        .map_err(anyhow::Error::from)?;
        if let Err(e) = fetched {
            return Err(Error::Invalid(e.to_string()));
        }
        if self.ref_snapshot(repo_id).await? != before {
            self.after_push(repo_id, None, Some(before)).await?;
        }
        Ok(())
    }
}

/// Keeps mirrors in sync: syncs whichever is due, one at a time, and sleeps
/// until the next is due (or one is asked to sync now).
pub fn start_mirror_worker(core: Core) {
    tokio::spawn(async move {
        let wakeup = core.mirror_wakeup();
        loop {
            match core.claim_mirror().await {
                Ok(Some(repo_id)) => {
                    if let Err(e) = core.sync_mirror(repo_id).await {
                        tracing::warn!(repo = repo_id, "mirror sync: {e}");
                    }
                    continue;
                }
                Ok(None) => {}
                Err(e) => tracing::warn!("mirror queue: {e}"),
            }
            let wait = match core.next_mirror_due().await {
                Ok(Some(t)) => (t - now()).clamp(1, 300) as u64,
                _ => 300,
            };
            tokio::select! {
                _ = wakeup.notified() => {}
                _ = tokio::time::sleep(Duration::from_secs(wait)) => {}
            }
        }
    });
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
