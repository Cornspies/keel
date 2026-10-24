//! Git LFS storage: large files kept outside the repositories, under
//! `data/lfs/<aa>/<bb>/<sha256>`, once however many repositories use them.
//! Which repository may read which object is recorded in `lfs_objects`.
//!
//! LFS clients get short-lived tokens (for object transfers, and over SSH
//! from `git-lfs-authenticate`), signed with a server secret rather than
//! stored.

use std::path::PathBuf;

use crate::{Core, Repo, Result, User, now, sign};

/// How long LFS tokens are valid.
pub const LFS_TOKEN_SECS: i64 = 3600;

/// Whether `oid` is a SHA-256 in lowercase hex, as LFS uses.
pub fn valid_oid(oid: &str) -> bool {
    oid.len() == 64 && oid.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The object a Git LFS pointer file refers to: (oid, size).
pub fn parse_pointer(data: &[u8]) -> Option<(String, u64)> {
    if data.len() > 1024 {
        return None;
    }
    let text = std::str::from_utf8(data).ok()?;
    if !text.starts_with("version https://git-lfs.github.com/spec/v1\n") {
        return None;
    }
    let mut oid = None;
    let mut size = None;
    for line in text.lines() {
        if let Some(o) = line.strip_prefix("oid sha256:") {
            oid = Some(o.trim().to_owned());
        } else if let Some(s) = line.strip_prefix("size ") {
            size = s.trim().parse().ok();
        }
    }
    let oid = oid.filter(|o| valid_oid(o))?;
    Some((oid, size?))
}

impl Core {
    /// Where an object is stored.
    pub fn lfs_path(&self, oid: &str) -> PathBuf {
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
            .git(to, move |g| {
                let files = match base {
                    Some(b) => g.changed_files(b, head)?.changed,
                    None => g.files(head)?,
                };
                let mut out = Vec::new();
                for f in files {
                    if g.blob_size(f.id)? <= 1024
                        && let Some((data, _)) = g.blob_by_id(f.id)?
                        && let Some((oid, _)) = parse_pointer(&data)
                    {
                        out.push(oid);
                    }
                }
                Ok(out)
            })
            .await?;
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
        let Some((payload, mac)) = token.rsplit_once('.') else { return Ok(None) };
        let expected = sign(&self.token_secret, payload.as_bytes());
        let expected = expected.trim_start_matches("sha256=");
        if expected.len() != mac.len() || expected.bytes().zip(mac.bytes()).fold(0, |a, (x, y)| a | (x ^ y)) != 0 {
            return Ok(None);
        }
        let parts: Vec<&str> = payload.split('.').collect();
        let [tag, user, repo_id, write, expires] = parts[..] else { return Ok(None) };
        let parsed = (user.parse::<i64>(), repo_id.parse::<i64>(), expires.parse::<i64>());
        let (Ok(user_id), Ok(repo_id), Ok(expires)) = parsed else { return Ok(None) };
        if tag != "lfs1" || repo_id != repo.id || expires < now() {
            return Ok(None);
        }
        Ok(self.user_by_id(user_id).await?.filter(|u| !u.disabled).map(|u| (u, write == "1")))
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
