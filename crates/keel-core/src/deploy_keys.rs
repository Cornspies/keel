//! Deploy keys: SSH keys that give access to one repository, for servers
//! and CI rather than people. Read-only unless allowed to push; pushes with
//! them follow branch protection like anyone's. They work for git over SSH
//! and Git LFS (not for locking files, which belongs to people).

use crate::ssh_keys::parse_public_key;
use crate::{Core, Error, Repo, Result, User, invalid, now};

/// Deploy keys a repository can have.
pub const MAX_DEPLOY_KEYS: i64 = 50;

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct DeployKey {
    pub id: i64,
    pub repo_id: i64,
    pub title: String,
    pub fingerprint: String,
    pub public_key: String,
    pub can_write: bool,
    pub creator_id: Option<i64>,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

impl Core {
    pub async fn add_deploy_key(&self, repo: &Repo, creator: &User, title: &str, key: &str, can_write: bool) -> Result<DeployKey> {
        let parsed = parse_public_key(key)?;
        let title = match title.trim() {
            "" if parsed.comment.is_empty() => "Deploy key",
            "" => parsed.comment.as_str(),
            t => t,
        };
        if title.chars().count() > 100 {
            return invalid("Titles are up to 100 characters.");
        }
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM deploy_keys WHERE repo_id = ?1")
            .bind(repo.id)
            .fetch_one(&self.db)
            .await?;
        if count >= MAX_DEPLOY_KEYS {
            return invalid(format!("A repository can have at most {MAX_DEPLOY_KEYS} deploy keys."));
        }
        // One key, one meaning: someone's key or this repository's.
        if self.ssh_key_exists(&parsed.fingerprint).await? {
            return invalid("This key is in use as someone's SSH key. Make a new key for deploying.");
        }
        let result = sqlx::query_as::<_, DeployKey>(
            "INSERT INTO deploy_keys (repo_id, title, fingerprint, public_key, can_write, creator_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7) RETURNING *",
        )
        .bind(repo.id)
        .bind(title)
        .bind(&parsed.fingerprint)
        .bind(&parsed.normalized)
        .bind(can_write)
        .bind(creator.id)
        .bind(now())
        .fetch_one(&self.db)
        .await;
        match result {
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
                invalid("This key is already a deploy key, here or of another repository.")
            }
            r => Ok(r?),
        }
    }

    pub async fn deploy_keys(&self, repo_id: i64) -> Result<Vec<DeployKey>> {
        Ok(sqlx::query_as("SELECT * FROM deploy_keys WHERE repo_id = ?1 ORDER BY id")
            .bind(repo_id)
            .fetch_all(&self.db)
            .await?)
    }

    pub async fn deploy_key(&self, repo_id: i64, id: i64) -> Result<Option<DeployKey>> {
        Ok(sqlx::query_as("SELECT * FROM deploy_keys WHERE repo_id = ?1 AND id = ?2")
            .bind(repo_id)
            .bind(id)
            .fetch_optional(&self.db)
            .await?)
    }

    pub async fn delete_deploy_key(&self, repo_id: i64, id: i64) -> Result<()> {
        let done = sqlx::query("DELETE FROM deploy_keys WHERE repo_id = ?1 AND id = ?2")
            .bind(repo_id)
            .bind(id)
            .execute(&self.db)
            .await?;
        if done.rows_affected() == 0 {
            return Err(Error::NotFound);
        }
        Ok(())
    }

    pub async fn deploy_key_exists(&self, fingerprint: &str) -> Result<bool> {
        let row: Option<(i64,)> = sqlx::query_as("SELECT id FROM deploy_keys WHERE fingerprint = ?1")
            .bind(fingerprint)
            .fetch_optional(&self.db)
            .await?;
        Ok(row.is_some())
    }

    /// The deploy key with this fingerprint, recording that it was used.
    pub async fn deploy_key_by_fingerprint(&self, fingerprint: &str) -> Result<Option<DeployKey>> {
        Ok(sqlx::query_as("UPDATE deploy_keys SET last_used_at = ?2 WHERE fingerprint = ?1 RETURNING *")
            .bind(fingerprint)
            .bind(now())
            .fetch_optional(&self.db)
            .await?)
    }
}

#[cfg(test)]
mod tests {
    use crate::Core;

    const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIC6zoxk3Z5bwvcVFAgXzaj00HQ9WJq6OzV7DGgTKSwKF ci@build";
    const OTHER: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICfostx/oKp+O+xjcX255z9zQFvnoQ+vdTuFIYxBV87f";

    #[tokio::test]
    async fn deploy_keys() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let repo = core.create_repo(&ada, "engine", "", false).await.unwrap();
        let other = core.create_repo(&ada, "other", "", false).await.unwrap();
        let key = core.add_deploy_key(&repo, &ada, "", KEY, false).await.unwrap();
        assert_eq!((key.title.as_str(), key.can_write), ("ci@build", false));
        assert!(core.add_deploy_key(&other, &ada, "again", KEY, true).await.is_err(), "one repository per key");
        assert!(core.deploy_key_exists(&key.fingerprint).await.unwrap());
        let found = core.deploy_key_by_fingerprint(&key.fingerprint).await.unwrap().unwrap();
        assert_eq!(found.repo_id, repo.id);
        assert!(core.deploy_keys(repo.id).await.unwrap()[0].last_used_at.is_some());
        // A key can't be both a person's and a repository's.
        assert!(core.add_ssh_key(ada.id, "laptop", KEY).await.is_err());
        core.add_ssh_key(ada.id, "laptop", OTHER).await.unwrap();
        assert!(core.add_deploy_key(&repo, &ada, "", OTHER, false).await.is_err());
        assert!(core.delete_deploy_key(other.id, key.id).await.is_err(), "only from its repository");
        core.delete_deploy_key(repo.id, key.id).await.unwrap();
        assert!(core.deploy_key_by_fingerprint(&key.fingerprint).await.unwrap().is_none());
        assert!(core.add_deploy_key(&repo, &ada, "", "ssh-rsa nonsense", false).await.is_err());
    }
}
