use std::path::PathBuf;

use crate::labels::insert_default_labels;
use crate::{Core, Error, Result, User, invalid, now, validate_repo_name};

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Repo {
    pub id: i64,
    pub owner_id: i64,
    /// The owner's username.
    pub owner: String,
    pub name: String,
    pub description: String,
    pub is_private: bool,
    pub created_at: i64,
    pub updated_at: i64,
    /// The repository this one was forked from, if it still exists.
    pub forked_from: Option<i64>,
    /// Topics, separated by spaces; see [`topics`](Self::topics).
    pub topic_names: String,
    pub stars: i64,
    /// Can be used to start new repositories.
    pub is_template: bool,
    /// The template this one was generated from, if it still exists.
    pub template_id: Option<i64>,
    /// When it was archived (made read-only), if it is.
    pub archived_at: Option<i64>,
}

impl Repo {
    pub fn is_archived(&self) -> bool {
        self.archived_at.is_some()
    }

    pub fn topics(&self) -> impl Iterator<Item = &str> {
        self.topic_names.split_whitespace()
    }
}

// A macro rather than a const so queries stay `&'static str` via `concat!`.
macro_rules! repo_select {
    () => {
        "SELECT r.id, r.owner_id, u.username AS owner, r.name, r.description,
            r.is_private, r.created_at, r.updated_at, r.forked_from,
            COALESCE((SELECT group_concat(t.topic, ' ' ORDER BY t.topic) FROM repo_topics t WHERE t.repo_id = r.id), '')
              AS topic_names,
            r.star_count AS stars, r.is_template, r.template_id, r.archived_at
         FROM repositories r JOIN users u ON u.id = r.owner_id "
    };
}

/// SQL condition: repository `r` is visible to the user with id `?2`
/// (`-1` for anonymous), who is a site admin if `?3`.
macro_rules! visible_to {
    () => {
        "(r.is_private = 0 OR ?3 OR r.owner_id = ?2
          OR EXISTS (SELECT 1 FROM collaborators c WHERE c.repo_id = r.id AND c.user_id = ?2)
          OR EXISTS (SELECT 1 FROM org_members m WHERE m.org_id = r.owner_id AND m.user_id = ?2))"
    };
}

mod issue_search;
mod pins;
mod stars;
mod templates;
mod topics;
pub use issue_search::{IssueHit, IssueQuery, IssueSort, StateFilter};
pub use pins::MAX_PINS;
pub use stars::Stargazer;
pub use topics::{ExploreQuery, ExploreSort, MAX_TOPICS, parse_topics};

impl Core {
    pub fn repo_path(&self, repo: &Repo) -> PathBuf {
        self.config.repo_path(repo.id)
    }

    pub async fn create_repo(
        &self,
        owner: &User,
        name: &str,
        description: &str,
        is_private: bool,
    ) -> Result<Repo> {
        self.insert_repo(owner, name, description, is_private, None).await
    }

    /// Forks `source` into `owner`'s account as `name`. Forks of private
    /// repositories are private.
    pub async fn fork_repo(&self, source: &Repo, owner: &User, name: &str) -> Result<Repo> {
        if owner.id == source.owner_id {
            return invalid("A repository can't be forked into the account that owns it.");
        }
        self.check_storage(owner.id, self.repo_storage(source.id).await?).await?;
        let fork = self
            .insert_repo(owner, name, &source.description, source.is_private, Some(source))
            .await?;
        // Index, warm caches and write the commit-graph like after a push.
        self.after_push(fork.id, None, None).await?;
        Ok(fork)
    }

    async fn insert_repo(
        &self,
        owner: &User,
        name: &str,
        description: &str,
        is_private: bool,
        fork_of: Option<&Repo>,
    ) -> Result<Repo> {
        let name = name.trim();
        validate_repo_name(name)?;
        self.check_repo_limit(owner).await?;
        let mut tx = self.write_tx().await?;
        let now = now();
        let inserted = sqlx::query_as::<_, (i64,)>(
            "INSERT INTO repositories (owner_id, name, description, is_private, created_at, updated_at, forked_from)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6) RETURNING id",
        )
        .bind(owner.id)
        .bind(name)
        .bind(description.trim())
        .bind(is_private)
        .bind(now)
        .bind(fork_of.map(|r| r.id))
        .fetch_one(&mut *tx)
        .await;
        let (id,) = match inserted {
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
                return invalid(format!("{} already has a repository named “{name}”.", owner.username));
            }
            r => r?,
        };
        // The name may have redirected to a renamed repository; not any more.
        sqlx::query("DELETE FROM repo_redirects WHERE owner_id = ?1 AND name = ?2")
            .bind(owner.id)
            .bind(name)
            .execute(&mut *tx)
            .await?;
        insert_default_labels(&mut tx, id).await?;
        // Forks can read their parent's large files.
        if let Some(parent) = fork_of {
            sqlx::query(
                "INSERT INTO lfs_objects (repo_id, oid, size, created_at)
                 SELECT ?1, oid, size, ?3 FROM lfs_objects WHERE repo_id = ?2",
            )
            .bind(id)
            .bind(parent.id)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        }
        // People watch their own repositories.
        if !owner.is_org() {
            sqlx::query("INSERT INTO watches (user_id, repo_id, created_at) VALUES (?1, ?2, ?3)")
                .bind(owner.id)
                .bind(id)
                .bind(now)
                .execute(&mut *tx)
                .await?;
        }
        // Create on disk inside the transaction so a failure leaves no row.
        let path = self.config.repo_path(id);
        let source = fork_of.map(|r| self.repo_path(r));
        tokio::task::spawn_blocking(move || match source {
            Some(source) => keel_git::fork(&source, &path),
            None => keel_git::create_bare(&path, "main"),
        })
        .await
        .map_err(anyhow::Error::from)??;
        tx.commit().await?;
        Ok(Repo {
            id,
            owner_id: owner.id,
            owner: owner.username.clone(),
            name: name.to_owned(),
            description: description.trim().to_owned(),
            is_private,
            created_at: now,
            updated_at: now,
            forked_from: fork_of.map(|r| r.id),
            topic_names: String::new(),
            stars: 0,
            is_template: false,
            template_id: None,
            archived_at: None,
        })
    }

    /// Forks of `repo` visible to `viewer`, most recently pushed first.
    pub async fn forks_of(&self, repo_id: i64, viewer: Option<&User>) -> Result<Vec<Repo>> {
        Ok(sqlx::query_as(concat!(
            repo_select!(),
            "WHERE r.forked_from = ?1 AND ",
            visible_to!(),
            " ORDER BY r.updated_at DESC"
        ))
        .bind(repo_id)
        .bind(viewer.map_or(-1, |u| u.id))
        .bind(viewer.is_some_and(|u| u.is_admin))
        .fetch_all(&self.db)
        .await?)
    }

    pub async fn fork_count(&self, repo_id: i64) -> Result<i64> {
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM repositories WHERE forked_from = ?1")
            .bind(repo_id)
            .fetch_one(&self.db)
            .await?;
        Ok(n)
    }

    /// Looks up `owner/name` (case-insensitively).
    pub async fn repo(&self, owner: &str, name: &str) -> Result<Option<Repo>> {
        Ok(sqlx::query_as(concat!(
            repo_select!(),
            "WHERE u.username = ?1 AND r.name = ?2"
        ))
        .bind(owner)
        .bind(name)
        .fetch_optional(&self.db)
        .await?)
    }

    pub async fn repo_by_id(&self, id: i64) -> Result<Option<Repo>> {
        Ok(sqlx::query_as(concat!(repo_select!(), "WHERE r.id = ?1"))
            .bind(id)
            .fetch_optional(&self.db)
            .await?)
    }

    /// An owner's repositories visible to `viewer`, most recently pushed first.
    pub async fn repos_of(&self, owner_id: i64, viewer: Option<&User>) -> Result<Vec<Repo>> {
        Ok(sqlx::query_as(concat!(
            repo_select!(),
            "WHERE r.owner_id = ?1 AND ",
            visible_to!(),
            " ORDER BY r.updated_at DESC"
        ))
        .bind(owner_id)
        .bind(viewer.map_or(-1, |u| u.id))
        .bind(viewer.is_some_and(|u| u.is_admin))
        .fetch_all(&self.db)
        .await?)
    }

    /// Repositories `user` can push to: their own, their organizations' and
    /// those they collaborate on with write access. Most recent first.
    pub async fn repos_for(&self, user: &User) -> Result<Vec<Repo>> {
        Ok(sqlx::query_as(concat!(
            repo_select!(),
            "WHERE r.owner_id = ?1
               OR EXISTS (SELECT 1 FROM org_members m WHERE m.org_id = r.owner_id AND m.user_id = ?1)
               OR EXISTS (SELECT 1 FROM collaborators c WHERE c.repo_id = r.id AND c.user_id = ?1
                          AND c.role != 'read')
             ORDER BY r.updated_at DESC"
        ))
        .bind(user.id)
        .fetch_all(&self.db)
        .await?)
    }

    pub async fn all_repo_ids(&self) -> Result<Vec<i64>> {
        let rows: Vec<(i64,)> = sqlx::query_as("SELECT id FROM repositories ORDER BY updated_at DESC")
            .fetch_all(&self.db)
            .await?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    /// Ids of all repositories `viewer` can read.
    pub async fn visible_repo_ids(&self, viewer: Option<&User>) -> Result<Vec<i64>> {
        let rows: Vec<(i64,)> = sqlx::query_as(concat!(
            "SELECT r.id FROM repositories r WHERE ?1 = ?1 AND ",
            visible_to!()
        ))
        .bind(0)
        .bind(viewer.map_or(-1, |u| u.id))
        .bind(viewer.is_some_and(|u| u.is_admin))
        .fetch_all(&self.db)
        .await?;
        Ok(rows.into_iter().map(|(id,)| id).collect())
    }

    pub async fn recent_public_repos(&self, limit: i64) -> Result<Vec<Repo>> {
        Ok(sqlx::query_as(concat!(
            repo_select!(),
            "WHERE r.is_private = 0 ORDER BY r.updated_at DESC LIMIT ?1"
        ))
        .bind(limit)
        .fetch_all(&self.db)
        .await?)
    }

    pub async fn update_repo(&self, repo_id: i64, description: &str, is_private: bool) -> Result<()> {
        let (parent_private,): (bool,) = sqlx::query_as(
            "SELECT COALESCE((SELECT p.is_private FROM repositories r JOIN repositories p ON p.id = r.forked_from
                              WHERE r.id = ?1), 0)",
        )
        .bind(repo_id)
        .fetch_one(&self.db)
        .await?;
        if parent_private && !is_private {
            return invalid("Forks of private repositories stay private.");
        }
        sqlx::query("UPDATE repositories SET description = ?2, is_private = ?3 WHERE id = ?1")
            .bind(repo_id)
            .bind(description.trim())
            .bind(is_private)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    /// The repository's branches and tags, to pass to
    /// [`after_push`](Self::after_push) once they changed.
    pub async fn ref_snapshot(&self, repo_id: i64) -> Result<keel_git::Refs> {
        self.git(repo_id, |g| g.refs_snapshot()).await
    }

    /// Records that refs changed (a push by `pusher`, or a merge), then in
    /// the background: runs repository maintenance, updates pull requests,
    /// sends `push` webhooks for what changed since `before` (if given) and
    /// notifies subscribers.
    pub async fn after_push(&self, repo_id: i64, pusher: Option<i64>, before: Option<keel_git::Refs>) -> Result<()> {
        sqlx::query("UPDATE repositories SET updated_at = ?2 WHERE id = ?1")
            .bind(repo_id)
            .bind(now())
            .execute(&self.db)
            .await?;
        self.push_mirrors_pending(repo_id).await?;
        let path = self.config.repo_path(repo_id);
        let core = self.clone();
        tokio::spawn(async move {
            let maintenance = tokio::task::spawn_blocking(move || keel_git::run_maintenance(&path)).await;
            if let Ok(Err(e)) = maintenance {
                tracing::warn!(repo = repo_id, "maintenance failed: {e}");
            }
            if let Err(e) = core.measure_repo(repo_id).await {
                tracing::warn!(repo = repo_id, "measuring disk usage failed: {e}");
            }
            if let Some(before) = before {
                match core.ref_snapshot(repo_id).await {
                    Ok(after) => {
                        let changes = keel_git::ref_changes(&before, &after);
                        core.record(crate::Activity::Push { repo_id, pusher, before, changes }).await;
                    }
                    Err(e) => tracing::warn!(repo = repo_id, "reading refs failed: {e}"),
                }
            }
            if let Err(e) = core.sync_pulls(repo_id, pusher).await {
                tracing::warn!(repo = repo_id, "updating pull requests failed: {e}");
            }
            core.emit(crate::RepoEvent::Pushed { repo_id });
        });
        Ok(())
    }

    /// The repository at `owner/name`, or the one that was there before a
    /// rename or transfer.
    pub async fn repo_or_redirect(&self, owner: &str, name: &str) -> Result<Option<Repo>> {
        if let Some(repo) = self.repo(owner, name).await? {
            return Ok(Some(repo));
        }
        self.repo_redirect(owner, name).await
    }

    /// Where `owner/name` moved to, if it was renamed or transferred.
    pub async fn repo_redirect(&self, owner: &str, name: &str) -> Result<Option<Repo>> {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT r.repo_id FROM repo_redirects r JOIN users u ON u.id = r.owner_id
             WHERE u.username = ?1 AND r.name = ?2",
        )
        .bind(owner)
        .bind(name)
        .fetch_optional(&self.db)
        .await?;
        match row {
            Some((id,)) => self.repo_by_id(id).await,
            None => Ok(None),
        }
    }

    /// Renames a repository. The old name redirects to it.
    pub async fn rename_repo(&self, repo: &Repo, new_name: &str) -> Result<Repo> {
        let owner = self.user_by_id(repo.owner_id).await?.ok_or(Error::NotFound)?;
        self.move_repo(repo, &owner, new_name).await
    }

    /// Moves a repository to `new_owner`: the actor themselves or an
    /// organization they belong to (callers check that the actor
    /// administers the repository). The old name redirects to it.
    pub async fn transfer_repo(&self, repo: &Repo, actor: &User, new_owner: &User) -> Result<Repo> {
        if new_owner.id == repo.owner_id {
            return invalid(format!("{} already owns it.", new_owner.username));
        }
        if !self.can_create_repo_in(new_owner, actor).await? {
            return invalid(format!("You can't move repositories to {}.", new_owner.username));
        }
        self.check_repo_limit(new_owner).await?;
        self.check_storage(new_owner.id, self.repo_storage(repo.id).await?).await?;
        self.move_repo(repo, new_owner, &repo.name).await
    }

    async fn move_repo(&self, repo: &Repo, owner: &User, name: &str) -> Result<Repo> {
        let name = name.trim();
        validate_repo_name(name)?;
        if owner.id == repo.owner_id && name == repo.name {
            return Ok(repo.clone());
        }
        let mut tx = self.write_tx().await?;
        let moved = sqlx::query("UPDATE repositories SET owner_id = ?2, name = ?3 WHERE id = ?1")
            .bind(repo.id)
            .bind(owner.id)
            .bind(name)
            .execute(&mut *tx)
            .await;
        match moved {
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
                return invalid(format!("{} already has a repository named “{name}”.", owner.username));
            }
            r => r?,
        };
        sqlx::query("DELETE FROM repo_redirects WHERE owner_id = ?1 AND name = ?2")
            .bind(owner.id)
            .bind(name)
            .execute(&mut *tx)
            .await?;
        // A change of case only needs no redirect (names ignore case).
        if owner.id != repo.owner_id || !name.eq_ignore_ascii_case(&repo.name) {
            sqlx::query(
                "INSERT INTO repo_redirects (owner_id, name, repo_id, created_at) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (owner_id, name) DO UPDATE SET repo_id = excluded.repo_id, created_at = excluded.created_at",
            )
            .bind(repo.owner_id)
            .bind(&repo.name)
            .bind(repo.id)
            .bind(now())
            .execute(&mut *tx)
            .await?;
        }
        if !owner.is_org() {
            sqlx::query("INSERT OR IGNORE INTO watches (user_id, repo_id, created_at) VALUES (?1, ?2, ?3)")
                .bind(owner.id)
                .bind(repo.id)
                .bind(now())
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        self.repo_by_id(repo.id).await?.ok_or(Error::NotFound)
    }

    pub async fn delete_repo(&self, repo: &Repo) -> Result<()> {
        let done = sqlx::query("DELETE FROM repositories WHERE id = ?1")
            .bind(repo.id)
            .execute(&self.db)
            .await?;
        if done.rows_affected() == 0 {
            return Err(Error::NotFound);
        }
        self.emit(crate::RepoEvent::Deleted { repo_id: repo.id });
        let path = self.repo_path(repo);
        tokio::task::spawn_blocking(move || std::fs::remove_dir_all(path))
            .await
            .map_err(anyhow::Error::from)?
            .map_err(anyhow::Error::from)?;
        self.delete_wiki(repo.id).await;
        // Large files and release files only it used can go.
        let core = self.clone();
        tokio::spawn(async move {
            if let Err(e) = core.prune_lfs().await {
                tracing::warn!("pruning LFS objects: {e}");
            }
            if let Err(e) = core.prune_release_assets().await {
                tracing::warn!("pruning release files: {e}");
            }
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn renaming_and_transferring() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let bob = core.register("bob", "bob@example.com", "password1").await.unwrap();
        let acme = core.create_org(&ada, "acme").await.unwrap();
        let engine = core.create_repo(&ada, "engine", "", false).await.unwrap();
        core.create_repo(&ada, "taken", "", false).await.unwrap();

        assert!(core.rename_repo(&engine, "taken").await.is_err());
        assert!(core.rename_repo(&engine, "bad name").await.is_err());
        let motor = core.rename_repo(&engine, "motor").await.unwrap();
        assert_eq!((motor.id, motor.name.as_str()), (engine.id, "motor"));
        assert_eq!(core.repo_or_redirect("ada", "engine").await.unwrap().unwrap().name, "motor");
        assert!(core.repo("ada", "engine").await.unwrap().is_none());

        // Renaming again keeps the first redirect working.
        let turbine = core.rename_repo(&motor, "turbine").await.unwrap();
        assert_eq!(core.repo_or_redirect("ada", "engine").await.unwrap().unwrap().id, turbine.id);
        assert_eq!(core.repo_or_redirect("ada", "motor").await.unwrap().unwrap().id, turbine.id);
        let cased = core.rename_repo(&turbine, "Turbine").await.unwrap();
        assert_eq!(cased.name, "Turbine");

        // Transfers: to organizations you're in, not to other people.
        assert!(core.transfer_repo(&cased, &ada, &bob).await.is_err());
        let moved = core.transfer_repo(&cased, &ada, &acme).await.unwrap();
        assert_eq!((moved.owner.as_str(), moved.name.as_str()), ("acme", "Turbine"));
        assert_eq!(core.repo_or_redirect("ada", "turbine").await.unwrap().unwrap().owner, "acme");
        assert_eq!(core.repo_or_redirect("ada", "engine").await.unwrap().unwrap().owner, "acme");

        // A new repository takes the old name over.
        let fresh = core.create_repo(&ada, "engine", "", false).await.unwrap();
        assert_eq!(core.repo_or_redirect("ada", "engine").await.unwrap().unwrap().id, fresh.id);
        core.delete_repo(&fresh).await.unwrap();
        assert!(core.repo_or_redirect("ada", "engine").await.unwrap().is_none(), "the redirect is gone for good");
    }

    #[tokio::test]
    async fn repo_lifecycle_and_access() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let bob = core.register("bob", "bob@example.com", "password1").await.unwrap();

        let public = core.create_repo(&ada, "engine", "Analytical", false).await.unwrap();
        let secret = core.create_repo(&ada, "notes", "", true).await.unwrap();
        assert!(core.repo_path(&public).join("HEAD").exists());
        assert!(core.create_repo(&ada, "ENGINE", "", false).await.is_err());
        assert!(core.create_repo(&ada, "bad/name", "", false).await.is_err());
        // Same name under another owner is fine.
        core.create_repo(&bob, "engine", "", false).await.unwrap();

        let found = core.repo("ADA", "Engine").await.unwrap().unwrap();
        assert_eq!((found.id, found.owner.as_str()), (public.id, "ada"));


        assert_eq!(core.repos_of(ada.id, Some(&bob)).await.unwrap().len(), 1);
        assert_eq!(core.repos_of(ada.id, Some(&ada)).await.unwrap().len(), 2);
        assert_eq!(core.recent_public_repos(10).await.unwrap().len(), 2);
        assert_eq!(core.visible_repo_ids(None).await.unwrap().len(), 2);
        assert_eq!(core.visible_repo_ids(Some(&ada)).await.unwrap().len(), 3);
        assert_eq!(core.all_repo_ids().await.unwrap().len(), 3);

        core.update_repo(secret.id, "now public", false).await.unwrap();
        assert_eq!(core.recent_public_repos(10).await.unwrap().len(), 3);

        core.delete_repo(&public).await.unwrap();
        assert!(!core.repo_path(&public).exists());
        assert!(core.repo("ada", "engine").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn forks() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let bob = core.register("bob", "bob@example.com", "password1").await.unwrap();
        let eve = core.register("eve", "eve@example.com", "password1").await.unwrap();
        let engine = core.create_repo(&ada, "engine", "Analytical", false).await.unwrap();
        let secret = core.create_repo(&ada, "secret", "", true).await.unwrap();

        assert!(core.fork_repo(&engine, &ada, "engine2").await.is_err(), "same owner");
        let fork = core.fork_repo(&engine, &bob, "engine").await.unwrap();
        assert_eq!((fork.forked_from, fork.description.as_str()), (Some(engine.id), "Analytical"));
        assert!(core.repo_path(&fork).join("HEAD").exists());
        assert!(core.fork_repo(&engine, &bob, "engine").await.is_err(), "name taken");
        assert_eq!(core.fork_count(engine.id).await.unwrap(), 1);
        assert_eq!(core.forks_of(engine.id, None).await.unwrap().len(), 1);
        assert_eq!(core.labels(fork.id).await.unwrap().len(), 6, "default labels");

        // Forks of private repositories stay private.
        let private_fork = core.fork_repo(&secret, &bob, "secret").await.unwrap();
        assert!(private_fork.is_private);
        assert!(core.forks_of(secret.id, Some(&eve)).await.unwrap().is_empty());
        assert!(core.update_repo(private_fork.id, "", false).await.is_err());
        core.update_repo(fork.id, "", true).await.unwrap();

        // Deleting the parent detaches its forks.
        core.delete_repo(&engine).await.unwrap();
        assert_eq!(core.repo_by_id(fork.id).await.unwrap().unwrap().forked_from, None);
    }
}
