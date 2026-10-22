use std::path::PathBuf;

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
}

// A macro rather than a const so queries stay `&'static str` via `concat!`.
macro_rules! repo_select {
    () => {
        "SELECT r.id, r.owner_id, u.username AS owner, r.name, r.description,
            r.is_private, r.created_at, r.updated_at
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
        let name = name.trim();
        validate_repo_name(name)?;
        let mut tx = self.db.begin().await?;
        let now = now();
        let inserted = sqlx::query_as::<_, (i64,)>(
            "INSERT INTO repositories (owner_id, name, description, is_private, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5) RETURNING id",
        )
        .bind(owner.id)
        .bind(name)
        .bind(description.trim())
        .bind(is_private)
        .bind(now)
        .fetch_one(&mut *tx)
        .await;
        let (id,) = match inserted {
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
                return invalid(format!("You already have a repository named “{name}”."));
            }
            r => r?,
        };
        // Create on disk inside the transaction so a failure leaves no row.
        let path = self.config.repo_path(id);
        tokio::task::spawn_blocking(move || keel_git::create_bare(&path, "main"))
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
        })
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
        sqlx::query("UPDATE repositories SET description = ?2, is_private = ?3 WHERE id = ?1")
            .bind(repo_id)
            .bind(description.trim())
            .bind(is_private)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    /// Records that refs changed (a push by `pusher`, or a merge), then in
    /// the background: runs repository maintenance, updates pull requests
    /// and notifies subscribers.
    pub async fn after_push(&self, repo_id: i64, pusher: Option<i64>) -> Result<()> {
        sqlx::query("UPDATE repositories SET updated_at = ?2 WHERE id = ?1")
            .bind(repo_id)
            .bind(now())
            .execute(&self.db)
            .await?;
        let path = self.config.repo_path(repo_id);
        let core = self.clone();
        tokio::spawn(async move {
            let maintenance = tokio::task::spawn_blocking(move || keel_git::run_maintenance(&path)).await;
            if let Ok(Err(e)) = maintenance {
                tracing::warn!(repo = repo_id, "maintenance failed: {e}");
            }
            if let Err(e) = core.sync_pulls(repo_id, pusher).await {
                tracing::warn!(repo = repo_id, "updating pull requests failed: {e}");
            }
            core.emit(crate::RepoEvent::Pushed { repo_id });
        });
        Ok(())
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
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
