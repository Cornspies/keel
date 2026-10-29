//! Git LFS storage: large files kept outside the repositories, under
//! `data/lfs/<aa>/<bb>/<sha256>`, once however many repositories use them.
//! Which repository may read which object is recorded in `lfs_objects`.
//!
//! LFS clients get short-lived tokens (for object transfers, and over SSH
//! from `git-lfs-authenticate`), signed with a server secret rather than
//! stored.

use std::path::PathBuf;

use crate::{Core, Repo, Result, User, invalid, now, sign};

/// A file locked with `git lfs lock`.
#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct LfsLock {
    pub id: i64,
    pub path: String,
    pub ref_name: String,
    pub owner_id: i64,
    pub owner: String,
    pub created_at: i64,
}

macro_rules! lock_select {
    () => {
        "SELECT l.id, l.path, l.ref_name, l.owner_id, u.username AS owner, l.created_at
         FROM lfs_locks l JOIN users u ON u.id = l.owner_id "
    };
}

/// How long LFS tokens are valid.
pub const LFS_TOKEN_SECS: i64 = 3600;

/// Whether `oid` is a SHA-256 in lowercase hex, as LFS uses.
pub fn valid_oid(oid: &str) -> bool {
    oid.len() == 64 && oid.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The object a Git LFS pointer file refers to: (oid, size).
pub fn parse_pointer(data: &[u8]) -> Option<(String, u64)> {
    keel_git::parse_lfs_pointer(data)
}

impl Core {
    /// Where an object is stored.
    pub fn lfs_path(&self, oid: &str) -> PathBuf {
        // Callers check (`valid_oid`, `parse_lfs_pointer`).
        debug_assert!(valid_oid(oid), "{oid}");
        self.config.data_dir.join("lfs").join(&oid[..2]).join(&oid[2..4]).join(oid)
    }

    /// Where uploads are written before they're checked.
    pub fn lfs_tmp_dir(&self) -> PathBuf {
        self.config.data_dir.join("lfs").join("tmp")
    }

    /// The size of an object `repo` may read, if it may and it's stored.
    pub async fn lfs_object(&self, repo_id: i64, oid: &str) -> Result<Option<u64>> {
        let row: Option<(i64,)> = sqlx::query_as("SELECT size FROM lfs_objects WHERE repo_id = ?1 AND oid = ?2")
            .bind(repo_id)
            .bind(oid)
            .fetch_optional(&self.db)
            .await?;
        Ok(row.map(|(s,)| s as u64).filter(|_| self.lfs_path(oid).is_file()))
    }

    /// Moves a downloaded file, whose SHA-256 the caller checked is `oid`,
    /// into the store and lets `repo_id` read it.
    pub async fn lfs_store(&self, repo_id: i64, tmp: &std::path::Path, oid: &str, size: u64) -> Result<()> {
        let path = self.lfs_path(oid);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await.map_err(anyhow::Error::from)?;
        }
        tokio::fs::rename(tmp, &path).await.map_err(anyhow::Error::from)?;
        self.lfs_link(repo_id, oid, size).await
    }

    /// Records that `repo_id` has an object (after it was uploaded and its
    /// content checked).
    pub async fn lfs_link(&self, repo_id: i64, oid: &str, size: u64) -> Result<()> {
        sqlx::query("INSERT OR IGNORE INTO lfs_objects (repo_id, oid, size, created_at) VALUES (?1, ?2, ?3, ?4)")
            .bind(repo_id)
            .bind(oid)
            .bind(size as i64)
            .bind(now())
            .execute(&self.db)
            .await?;
        Ok(())
    }

    /// After commits from repository `from` (e.g. a fork) were merged into
    /// `to`: lets `to` read the objects that the files changed between
    /// `base` and `head` point to, if `from` has them.
    pub(crate) async fn share_lfs_objects(&self, from: i64, to: i64, base: Option<keel_git::Oid>, head: keel_git::Oid) -> Result<()> {
        if from == to {
            return Ok(());
        }
        let oids: Vec<String> = self
            .git(to, move |g| g.lfs_pointers(&[(base, head)]))
            .await?
            .into_iter()
            .map(|(oid, _)| oid)
            .collect();
        if oids.is_empty() {
            return Ok(());
        }
        sqlx::query(
            "INSERT OR IGNORE INTO lfs_objects (repo_id, oid, size, created_at)
             SELECT ?2, oid, size, ?4 FROM lfs_objects
             WHERE repo_id = ?1 AND oid IN (SELECT value FROM json_each(?3))",
        )
        .bind(from)
        .bind(to)
        .bind(serde_json::to_string(&oids).map_err(anyhow::Error::from)?)
        .bind(now())
        .execute(&self.db)
        .await?;
        Ok(())
    }

    // -- Locks ----------------------------------------------------------------

    /// Locks of a repository, oldest first, after lock id `after` (for
    /// paging), optionally only for `path` or lock `id`.
    pub async fn lfs_locks(&self, repo_id: i64, path: Option<&str>, id: Option<i64>, after: i64, limit: i64) -> Result<Vec<LfsLock>> {
        Ok(sqlx::query_as(concat!(
            lock_select!(),
            "WHERE l.repo_id = ?1 AND (?2 IS NULL OR l.path = ?2) AND (?3 IS NULL OR l.id = ?3) AND l.id > ?4
             ORDER BY l.id LIMIT ?5"
        ))
        .bind(repo_id)
        .bind(path)
        .bind(id)
        .bind(after)
        .bind(limit)
        .fetch_all(&self.db)
        .await?)
    }

    /// Locks `path` for `user`. If someone (maybe the user) holds it
    /// already, returns that lock as the error value.
    pub async fn create_lfs_lock(&self, repo_id: i64, user: &User, path: &str, ref_name: &str) -> Result<Result<LfsLock, LfsLock>> {
        let path = path.trim().trim_start_matches('/');
        if path.is_empty() || path.len() > 1024 || path.chars().any(char::is_control) {
            return invalid("Invalid path.");
        }
        let inserted: Option<(i64,)> = sqlx::query_as(
            "INSERT INTO lfs_locks (repo_id, path, ref_name, owner_id, created_at) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (repo_id, path) DO NOTHING RETURNING id",
        )
        .bind(repo_id)
        .bind(path)
        .bind(ref_name)
        .bind(user.id)
        .bind(now())
        .fetch_optional(&self.db)
        .await?;
        let lock = match inserted {
            Some((id,)) => self.lfs_locks(repo_id, None, Some(id), 0, 1).await?,
            None => self.lfs_locks(repo_id, Some(path), None, 0, 1).await?,
        };
        let lock = lock.into_iter().next().ok_or(crate::Error::NotFound)?;
        Ok(if inserted.is_some() { Ok(lock) } else { Err(lock) })
    }

    pub async fn delete_lfs_lock(&self, repo_id: i64, id: i64) -> Result<()> {
        sqlx::query("DELETE FROM lfs_locks WHERE repo_id = ?1 AND id = ?2").bind(repo_id).bind(id).execute(&self.db).await?;
        Ok(())
    }

    /// How many objects a repository has, and their total size.
    pub async fn lfs_usage(&self, repo_id: i64) -> Result<(i64, i64)> {
        Ok(sqlx::query_as("SELECT COUNT(*), COALESCE(SUM(size), 0) FROM lfs_objects WHERE repo_id = ?1")
            .bind(repo_id)
            .fetch_one(&self.db)
            .await?)
    }

    /// Deletes stored objects no repository uses any more, and uploads that
    /// were abandoned. Returns how many files were removed.
    pub async fn prune_lfs(&self) -> Result<usize> {
        let root = self.config.data_dir.join("lfs");
        if !root.is_dir() {
            return Ok(0);
        }
        let used: std::collections::HashSet<String> = sqlx::query_as::<_, (String,)>("SELECT DISTINCT oid FROM lfs_objects")
            .fetch_all(&self.db)
            .await?
            .into_iter()
            .map(|(o,)| o)
            .collect();
        let tmp = self.lfs_tmp_dir();
        let removed = tokio::task::spawn_blocking(move || -> std::io::Result<usize> {
            let mut removed = 0;
            let mut stack = vec![root];
            while let Some(dir) = stack.pop() {
                for entry in std::fs::read_dir(&dir)? {
                    let entry = entry?;
                    let path = entry.path();
                    if entry.file_type()?.is_dir() {
                        stack.push(path);
                        continue;
                    }
                    let stale = if path.starts_with(&tmp) {
                        // Uploads in progress are recent.
                        entry.metadata()?.modified()?.elapsed().map(|e| e.as_secs() > 86_400).unwrap_or(false)
                    } else {
                        !used.contains(&entry.file_name().to_string_lossy().into_owned())
                    };
                    if stale {
                        std::fs::remove_file(&path)?;
                        removed += 1;
                    }
                }
            }
            Ok(removed)
        })
        .await
        .map_err(anyhow::Error::from)?
        .map_err(anyhow::Error::from)?;
        Ok(removed)
    }

    /// A token allowing `user` to download (or, with `write`, also upload)
    /// the LFS objects of `repo` for an hour.
    pub fn lfs_token(&self, user: &User, repo: &Repo, write: bool) -> String {
        let payload = format!("lfs1.{}.{}.{}.{}", user.id, repo.id, u8::from(write), now() + LFS_TOKEN_SECS);
        let mac = sign(&self.token_secret, payload.as_bytes());
        format!("{payload}.{}", mac.trim_start_matches("sha256="))
    }

    /// Checks an LFS token for `repo`: its user, and whether it allows
    /// uploads.
    pub async fn check_lfs_token(&self, token: &str, repo: &Repo) -> Result<Option<(User, bool)>> {
        let Some((user_id, write)) = self.lfs_token_claims(token, "lfs1", repo) else { return Ok(None) };
        Ok(self.user_by_id(user_id).await?.filter(|u| !u.disabled).map(|u| (u, write)))
    }

    /// An LFS token for a deploy key (`git-lfs-authenticate` over SSH).
    pub fn lfs_deploy_token(&self, key: &crate::DeployKey, write: bool) -> String {
        let payload = format!("lfsk.{}.{}.{}.{}", key.id, key.repo_id, u8::from(write), now() + LFS_TOKEN_SECS);
        let mac = sign(&self.token_secret, payload.as_bytes());
        format!("{payload}.{}", mac.trim_start_matches("sha256="))
    }

    /// Checks a deploy key's LFS token for `repo`: whether it allows
    /// uploads (if the key still exists and still may).
    pub async fn check_lfs_deploy_token(&self, token: &str, repo: &Repo) -> Result<Option<bool>> {
        let Some((key_id, write)) = self.lfs_token_claims(token, "lfsk", repo) else { return Ok(None) };
        Ok(self.deploy_key(repo.id, key_id).await?.map(|k| write && k.can_write))
    }

    /// The subject (user or key) and the write bit of a valid, unexpired
    /// token of kind `tag` for `repo`.
    fn lfs_token_claims(&self, token: &str, tag: &str, repo: &Repo) -> Option<(i64, bool)> {
        let (payload, mac) = token.rsplit_once('.')?;
        let expected = sign(&self.token_secret, payload.as_bytes());
        let expected = expected.trim_start_matches("sha256=");
        if expected.len() != mac.len() || expected.bytes().zip(mac.bytes()).fold(0, |a, (x, y)| a | (x ^ y)) != 0 {
            return None;
        }
        let parts: Vec<&str> = payload.split('.').collect();
        let [kind, subject, repo_id, write, expires] = parts[..] else { return None };
        let (subject, repo_id, expires) = (subject.parse::<i64>().ok()?, repo_id.parse::<i64>().ok()?, expires.parse::<i64>().ok()?);
        (kind == tag && repo_id == repo.id && expires >= now()).then_some((subject, write == "1"))
    }

    /// A stable key for signing image proxy URLs (derived from the token
    /// secret, so it survives restarts).
    pub fn image_proxy_key(&self) -> String {
        sign(&self.token_secret, b"image proxy")
    }

    /// The secret that signs tokens, created on first start.
    pub(crate) async fn load_token_secret(db: &sqlx::SqlitePool) -> Result<String> {
        let row: Option<(String,)> = sqlx::query_as("SELECT value FROM site_settings WHERE key = 'token_secret'")
            .fetch_optional(db)
            .await?;
        if let Some((s,)) = row {
            return Ok(s);
        }
        let secret = crate::auth::new_token()?;
        sqlx::query("INSERT OR IGNORE INTO site_settings (key, value) VALUES ('token_secret', ?1)")
            .bind(&secret)
            .execute(db)
            .await?;
        let (s,): (String,) = sqlx::query_as("SELECT value FROM site_settings WHERE key = 'token_secret'").fetch_one(db).await?;
        Ok(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointers() {
        let oid = "a".repeat(64);
        let p = format!("version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize 12345\n");
        assert_eq!(parse_pointer(p.as_bytes()), Some((oid.clone(), 12345)));
        assert_eq!(parse_pointer(b"just a file"), None);
        assert_eq!(parse_pointer(p.replace("sha256:a", "sha256:Z").as_bytes()), None);
        assert!(valid_oid(&oid) && !valid_oid("abc") && !valid_oid(&"A".repeat(64)));
    }

    #[tokio::test]
    async fn objects_and_tokens() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let repo = core.create_repo(&ada, "engine", "", false).await.unwrap();
        let other = core.create_repo(&ada, "other", "", false).await.unwrap();
        let oid = "b".repeat(64);
        std::fs::create_dir_all(core.lfs_path(&oid).parent().unwrap()).unwrap();
        std::fs::write(core.lfs_path(&oid), "content").unwrap();
        assert_eq!(core.lfs_object(repo.id, &oid).await.unwrap(), None, "not linked");
        core.lfs_link(repo.id, &oid, 7).await.unwrap();
        assert_eq!(core.lfs_object(repo.id, &oid).await.unwrap(), Some(7));
        assert_eq!(core.lfs_object(other.id, &oid).await.unwrap(), None);
        assert_eq!(core.lfs_usage(repo.id).await.unwrap(), (1, 7));

        // Forks can read their parent's objects.
        let bob = core.register("bob", "bob@example.com", "password1").await.unwrap();
        let fork = core.fork_repo(&repo, &bob, "engine").await.unwrap();
        assert_eq!(core.lfs_object(fork.id, &oid).await.unwrap(), Some(7));

        let token = core.lfs_token(&ada, &repo, true);
        let (user, write) = core.check_lfs_token(&token, &repo).await.unwrap().unwrap();
        assert_eq!((user.id, write), (ada.id, true));
        assert!(core.check_lfs_token(&token, &other).await.unwrap().is_none(), "other repository");
        assert!(core.check_lfs_token(&token.replace("lfs1.1", "lfs1.2"), &repo).await.unwrap().is_none(), "forged");
        assert!(core.check_lfs_token("nonsense", &repo).await.unwrap().is_none());
        // Deploy keys get their own kind of token.
        let key = core
            .add_deploy_key(&repo, &ada, "ci", "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIC6zoxk3Z5bwvcVFAgXzaj00HQ9WJq6OzV7DGgTKSwKF", false)
            .await
            .unwrap();
        let deploy = core.lfs_deploy_token(&key, true);
        assert_eq!(core.check_lfs_deploy_token(&deploy, &repo).await.unwrap(), Some(false), "the key may only read");
        assert!(core.check_lfs_token(&deploy, &repo).await.unwrap().is_none(), "not a user's token");
        assert!(core.check_lfs_deploy_token(&token, &repo).await.unwrap().is_none(), "not a key's token");
        assert!(core.check_lfs_deploy_token(&deploy, &other).await.unwrap().is_none());
        core.delete_deploy_key(repo.id, key.id).await.unwrap();
        assert!(core.check_lfs_deploy_token(&deploy, &repo).await.unwrap().is_none(), "the key is gone");

        // Locks.
        let mine = core.create_lfs_lock(repo.id, &ada, "/art/logo.psd", "refs/heads/main").await.unwrap().unwrap();
        assert_eq!((mine.path.as_str(), mine.owner.as_str()), ("art/logo.psd", "ada"));
        let taken = core.create_lfs_lock(repo.id, &bob, "art/logo.psd", "").await.unwrap().unwrap_err();
        assert_eq!(taken.id, mine.id);
        assert!(core.create_lfs_lock(repo.id, &bob, " ", "").await.is_err());
        core.create_lfs_lock(repo.id, &bob, "b.bin", "").await.unwrap().unwrap();
        assert_eq!(core.lfs_locks(repo.id, None, None, 0, 10).await.unwrap().len(), 2);
        assert_eq!(core.lfs_locks(repo.id, None, None, mine.id, 10).await.unwrap()[0].path, "b.bin", "paged");
        assert_eq!(core.lfs_locks(repo.id, Some("b.bin"), None, 0, 10).await.unwrap().len(), 1);
        core.delete_lfs_lock(repo.id, mine.id).await.unwrap();
        assert_eq!(core.lfs_locks(repo.id, None, None, 0, 10).await.unwrap().len(), 1);

        // Unused objects are pruned.
        let orphan = "c".repeat(64);
        std::fs::create_dir_all(core.lfs_path(&orphan).parent().unwrap()).unwrap();
        std::fs::write(core.lfs_path(&orphan), "x").unwrap();
        assert_eq!(core.prune_lfs().await.unwrap(), 1);
        assert!(core.lfs_path(&oid).is_file());
        core.delete_repo(&fork).await.unwrap();
        core.delete_repo(&repo).await.unwrap();
        // Deleting prunes in the background too; either way it's gone.
        core.prune_lfs().await.unwrap();
        assert!(!core.lfs_path(&oid).exists());
    }
}
