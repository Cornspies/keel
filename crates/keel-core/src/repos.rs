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

/// What a viewer may do with a repository.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Access {
    None,
    Read,
    Write,
}

impl Repo {
    /// v1 permission model: owners (and admins) can write, everyone can read
    /// public repositories, nobody else sees private ones.
    pub fn access(&self, viewer: Option<&User>) -> Access {
        match viewer {
            Some(u) if u.id == self.owner_id || u.is_admin => Access::Write,
            _ if !self.is_private => Access::Read,
            _ => Access::None,
        }
    }
}

// A macro rather than a const so queries stay `&'static str` via `concat!`.
macro_rules! repo_select {
    () => {
        "SELECT r.id, r.owner_id, u.username AS owner, r.name, r.description,
            r.is_private, r.created_at, r.updated_at
         FROM repositories r JOIN users u ON u.id = r.owner_id "
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

    /// An owner's repositories visible to `viewer`, most recently pushed first.
    pub async fn repos_of(&self, owner_id: i64, viewer: Option<&User>) -> Result<Vec<Repo>> {
        let repos: Vec<Repo> = sqlx::query_as(concat!(
            repo_select!(),
            "WHERE r.owner_id = ?1 ORDER BY r.updated_at DESC"
        ))
        .bind(owner_id)
        .fetch_all(&self.db)
        .await?;
        Ok(repos
            .into_iter()
            .filter(|r| r.access(viewer) >= Access::Read)
            .collect())
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

    /// Records a push and runs repository maintenance in the background.
    pub async fn after_push(&self, repo_id: i64) -> Result<()> {
        sqlx::query("UPDATE repositories SET updated_at = ?2 WHERE id = ?1")
            .bind(repo_id)
            .bind(now())
            .execute(&self.db)
            .await?;
        let path = self.config.repo_path(repo_id);
        tokio::task::spawn_blocking(move || {
            if let Err(e) = keel_git::run_maintenance(&path) {
                tracing::warn!(repo = repo_id, "maintenance failed: {e}");
            }
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

        assert_eq!(public.access(None), Access::Read);
        assert_eq!(public.access(Some(&bob)), Access::Read);
        assert_eq!(public.access(Some(&ada)), Access::Write);
        assert_eq!(secret.access(None), Access::None);
        assert_eq!(secret.access(Some(&bob)), Access::None);

        assert_eq!(core.repos_of(ada.id, Some(&bob)).await.unwrap().len(), 1);
        assert_eq!(core.repos_of(ada.id, Some(&ada)).await.unwrap().len(), 2);
        assert_eq!(core.recent_public_repos(10).await.unwrap().len(), 2);

        core.update_repo(secret.id, "now public", false).await.unwrap();
        assert_eq!(core.recent_public_repos(10).await.unwrap().len(), 3);

        core.delete_repo(&public).await.unwrap();
        assert!(!core.repo_path(&public).exists());
        assert!(core.repo("ada", "engine").await.unwrap().is_none());
    }
}
