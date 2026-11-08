//! Per-account limits for servers with open sign-ups: how many repositories
//! an account may own (`--max-repos`) and how many bytes they may store
//! (`--quota`). Site administrators' accounts have no limits.

use std::path::Path;

use crate::{Core, Result, User, invalid};

/// Bytes stored for an account's repositories; see
/// [`Core::storage_used`].
async fn used_by<'c>(db: impl sqlx::SqliteExecutor<'c>, owner_id: i64) -> Result<u64> {
    let (used,): (i64,) = sqlx::query_as(
        "SELECT
            (SELECT COALESCE(SUM(disk_bytes), 0) FROM repositories WHERE owner_id = ?1)
          + (SELECT COALESCE(SUM(size), 0) FROM (
                SELECT DISTINCT l.oid, l.size FROM lfs_objects l
                JOIN repositories r ON r.id = l.repo_id WHERE r.owner_id = ?1))
          + (SELECT COALESCE(SUM(a.size), 0) FROM release_assets a
                JOIN releases rel ON rel.id = a.release_id
                JOIN repositories r ON r.id = rel.repo_id WHERE r.owner_id = ?1)",
    )
    .bind(owner_id)
    .fetch_one(db)
    .await?;
    Ok(used.max(0) as u64)
}

impl Core {
    /// Errors when `owner` may not have another repository.
    pub(crate) async fn check_repo_limit(&self, owner: &User) -> Result<()> {
        let Some(max) = self.config.max_repos.filter(|_| !owner.is_admin) else { return Ok(()) };
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM repositories WHERE owner_id = ?1")
            .bind(owner.id)
            .fetch_one(&self.db)
            .await?;
        if count as u64 >= max {
            return invalid(format!("{} already has {max} repositories, the most this server allows.", owner.username));
        }
        Ok(())
    }

    /// Errors when `creator` may not create another organization. With a
    /// repository limit, organizations would otherwise multiply it.
    pub(crate) async fn check_org_limit(&self, creator: &User) -> Result<()> {
        let Some(max) = self.config.max_repos.filter(|_| !creator.is_admin) else { return Ok(()) };
        let (count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM org_members WHERE user_id = ?1 AND role = 'owner'")
                .bind(creator.id)
                .fetch_one(&self.db)
                .await?;
        if count as u64 >= max {
            return invalid(format!("You already own {max} organizations, the most this server allows."));
        }
        Ok(())
    }

    /// Bytes stored for an account's repositories: git data and wikis (as
    /// last measured), Git LFS files and release files.
    pub async fn storage_used(&self, owner_id: i64) -> Result<u64> {
        used_by(&self.db, owner_id).await
    }

    /// How many more bytes the account `owner_id` may store; `None` without
    /// a limit.
    pub async fn storage_room(&self, owner_id: i64) -> Result<Option<u64>> {
        let Some(quota) = self.config.quota_bytes else { return Ok(None) };
        let owner = self.user_by_id(owner_id).await?;
        if owner.is_none_or(|o| o.is_admin) {
            return Ok(None);
        }
        Ok(Some(quota.saturating_sub(self.storage_used(owner_id).await?)))
    }

    /// Errors when the account `owner_id` can't store `bytes` more. Pushes
    /// pass 1: they're refused once the account is full, and can go over by
    /// at most a push (`--max-push-size`).
    pub async fn check_storage(&self, owner_id: i64, bytes: u64) -> Result<()> {
        match self.storage_room(owner_id).await? {
            Some(room) if bytes > room => {
                let owner = self.user_by_id(owner_id).await?.map(|u| u.username).unwrap_or_default();
                self.out_of_storage(&owner)
            }
            _ => Ok(()),
        }
    }

    fn out_of_storage(&self, owner: &str) -> Result<()> {
        let quota = self.config.quota_bytes.unwrap_or(0);
        invalid(format!(
            "{owner} is out of storage: this server allows {} MiB per account. \
             Delete repositories, large files or release files to make room.",
            quota >> 20
        ))
    }

    /// [`check_storage`](Self::check_storage) for the account owning
    /// `repo_id`, in the write transaction that then records the `bytes`,
    /// so that concurrent uploads can't each use the same free space.
    pub(crate) async fn check_repo_storage_in(&self, conn: &mut sqlx::SqliteConnection, repo_id: i64, bytes: u64) -> Result<()> {
        let Some(quota) = self.config.quota_bytes else { return Ok(()) };
        let (owner_id, owner, is_admin): (i64, String, bool) = sqlx::query_as(
            "SELECT u.id, u.username, u.is_admin FROM repositories r JOIN users u ON u.id = r.owner_id WHERE r.id = ?1",
        )
        .bind(repo_id)
        .fetch_one(&mut *conn)
        .await?;
        if !is_admin && used_by(&mut *conn, owner_id).await?.saturating_add(bytes) > quota {
            return self.out_of_storage(&owner);
        }
        Ok(())
    }

    /// [`storage_room`](Self::storage_room) of the account that owns
    /// `repo_id`.
    pub(crate) async fn repo_storage_room(&self, repo_id: i64) -> Result<Option<u64>> {
        if self.config.quota_bytes.is_none() {
            return Ok(None);
        }
        let (owner_id,): (i64,) = sqlx::query_as("SELECT owner_id FROM repositories WHERE id = ?1")
            .bind(repo_id)
            .fetch_one(&self.db)
            .await?;
        self.storage_room(owner_id).await
    }

    /// Like [`check_storage`](Self::check_storage), for the account that
    /// owns `repo_id`.
    pub async fn check_repo_storage(&self, repo_id: i64, bytes: u64) -> Result<()> {
        if self.config.quota_bytes.is_none() {
            return Ok(());
        }
        let (owner_id,): (i64,) = sqlx::query_as("SELECT owner_id FROM repositories WHERE id = ?1")
            .bind(repo_id)
            .fetch_one(&self.db)
            .await?;
        self.check_storage(owner_id, bytes).await
    }

    /// Bytes stored for one repository, counted like
    /// [`storage_used`](Self::storage_used).
    pub(crate) async fn repo_storage(&self, repo_id: i64) -> Result<u64> {
        let (used,): (i64,) = sqlx::query_as(
            "SELECT
                (SELECT disk_bytes FROM repositories WHERE id = ?1)
              + (SELECT COALESCE(SUM(size), 0) FROM lfs_objects WHERE repo_id = ?1)
              + (SELECT COALESCE(SUM(a.size), 0) FROM release_assets a
                    JOIN releases rel ON rel.id = a.release_id WHERE rel.repo_id = ?1)",
        )
        .bind(repo_id)
        .fetch_one(&self.db)
        .await?;
        Ok(used.max(0) as u64)
    }

    /// Records how much space a repository and its wiki take on disk.
    pub(crate) async fn measure_repo(&self, repo_id: i64) -> Result<()> {
        let paths = [self.config.repo_path(repo_id), self.wiki_path(repo_id)];
        let bytes = tokio::task::spawn_blocking(move || paths.iter().map(|p| dir_size(p)).sum::<u64>())
            .await
            .map_err(anyhow::Error::from)?;
        sqlx::query("UPDATE repositories SET disk_bytes = ?2 WHERE id = ?1")
            .bind(repo_id)
            .bind(bytes.min(i64::MAX as u64) as i64)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    /// Measures repositories that weren't yet (made before quotas existed).
    pub async fn measure_repos(&self) -> Result<usize> {
        let ids: Vec<(i64,)> = sqlx::query_as("SELECT id FROM repositories WHERE disk_bytes = 0")
            .fetch_all(&self.db)
            .await?;
        for (id,) in &ids {
            self.measure_repo(*id).await?;
        }
        Ok(ids.len())
    }
}

/// The space the files under `path` take (0 if it doesn't exist), as the
/// System page counts it. A fork's objects shared with its original count
/// for both: each account pays for what it stores.
fn dir_size(path: &Path) -> u64 {
    crate::system::disk_usage(path, &mut std::collections::HashSet::new())
}

#[cfg(test)]
mod tests {
    use crate::{Config, Core, Error};

    async fn limited(quota: u64, max_repos: u64) -> (Core, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            data_dir: dir.path().to_owned(),
            quota_bytes: Some(quota),
            max_repos: Some(max_repos),
            ..Config::default()
        };
        (Core::open(config).await.unwrap(), dir)
    }

    #[tokio::test]
    async fn repository_limit() {
        let (core, _dir) = limited(1 << 30, 2).await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let bob = core.register("bob", "bob@example.com", "password1").await.unwrap();
        assert!(ada.is_admin && !bob.is_admin);
        for name in ["a", "b", "c"] {
            core.create_repo(&ada, name, "", false).await.unwrap();
        }
        let source = core.create_repo(&bob, "a", "", false).await.unwrap();
        core.create_repo(&bob, "b", "", false).await.unwrap();
        let e = core.create_repo(&bob, "c", "", false).await.unwrap_err();
        assert!(matches!(&e, Error::Invalid(m) if m.contains("already has 2 repositories")), "{e:?}");
        let theirs = core.repo("ada", "a").await.unwrap().unwrap();
        assert!(core.fork_repo(&theirs, &bob, "fork").await.is_err());
        // Organizations have their own limit, and don't multiply it.
        let org = core.create_org(&bob, "team").await.unwrap();
        core.create_org(&bob, "team2").await.unwrap();
        assert!(core.create_org(&bob, "team3").await.is_err());
        core.create_repo(&org, "one", "", false).await.unwrap();
        core.create_repo(&org, "two", "", false).await.unwrap();
        assert!(core.transfer_repo(&source, &bob, &org).await.is_err());
        core.delete_repo(&source).await.unwrap();
        core.create_repo(&bob, "c", "", false).await.unwrap();
    }

    #[tokio::test]
    async fn storage_quota() {
        let (core, _dir) = limited(1 << 20, 10).await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let bob = core.register("bob", "bob@example.com", "password1").await.unwrap();
        let repo = core.create_repo(&bob, "engine", "", false).await.unwrap();
        core.measure_repo(repo.id).await.unwrap();
        let git = core.storage_used(bob.id).await.unwrap();
        assert!(git > 0);
        core.check_storage(bob.id, 1).await.unwrap();
        // Large files count once, however many repositories have them.
        let other = core.create_repo(&bob, "other", "", false).await.unwrap();
        let oid = "b".repeat(64);
        core.lfs_link(repo.id, &oid, 600 << 10).await.unwrap();
        core.lfs_link(other.id, &oid, 600 << 10).await.unwrap();
        assert_eq!(core.storage_used(bob.id).await.unwrap(), git + (600 << 10));
        let e = core.check_storage(bob.id, 500 << 10).await.unwrap_err();
        assert!(matches!(&e, Error::Invalid(m) if m.contains("bob is out of storage")), "{e:?}");
        core.lfs_link(other.id, &"c".repeat(64), 500 << 10).await.unwrap();
        assert_eq!(core.storage_room(bob.id).await.unwrap(), Some(0));
        // Pushes are refused once the account is full.
        let target = core.git_target("bob", "engine").await.unwrap().unwrap();
        assert!(matches!(core.git_session(&target, true).await, Err(Error::Invalid(_))));
        assert!(core.git_session(&target, false).await.unwrap().is_some(), "reading still works");
        // Administrators have no limit.
        assert_eq!(core.storage_room(ada.id).await.unwrap(), None);
    }
}
