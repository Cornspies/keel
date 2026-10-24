//! Stars: people bookmark repositories they like, and the count shows how
//! popular a repository is. Triggers keep `repositories.star_count` in step
//! so lists can show and sort by it without counting.

use serde_json::json;

use crate::activity::time;
use crate::events::EventTarget;
use crate::{Core, Repo, Result, User, now};

/// Someone who starred a repository, and when.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Stargazer {
    #[sqlx(flatten)]
    pub user: User,
    pub starred_at: i64,
}

impl Core {
    /// Stars or unstars `repo` for `user`. Returns whether that changed
    /// anything.
    pub async fn set_star(&self, user: &User, repo: &Repo, on: bool) -> Result<bool> {
        let now = now();
        let changed = if on {
            sqlx::query("INSERT OR IGNORE INTO stars (user_id, repo_id, created_at) VALUES (?1, ?2, ?3)")
                .bind(user.id)
                .bind(repo.id)
                .bind(now)
                .execute(&self.db)
                .await?
        } else {
            sqlx::query("DELETE FROM stars WHERE user_id = ?1 AND repo_id = ?2")
                .bind(user.id)
                .bind(repo.id)
                .execute(&self.db)
                .await?
        }
        .rows_affected()
            > 0;
        if !changed {
            return Ok(false);
        }
        if on {
            self.add_event(repo.id, Some(user.id), "star", EventTarget::default(), json!({})).await?;
        }
        let payload = json!({
            "action": if on { "created" } else { "deleted" },
            "starred_at": if on { time(now) } else { serde_json::Value::Null },
            "repository": self.repo_json(repo),
            "sender": self.user_json(user),
        });
        self.enqueue_webhooks(repo.id, "star", if on { "created" } else { "deleted" }, &payload).await?;
        Ok(true)
    }

    pub async fn is_starred(&self, user_id: i64, repo_id: i64) -> Result<bool> {
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM stars WHERE user_id = ?1 AND repo_id = ?2")
            .bind(user_id)
            .bind(repo_id)
            .fetch_one(&self.db)
            .await?;
        Ok(n > 0)
    }

    /// What `user_id` starred that `viewer` can see, most recently starred
    /// first.
    pub async fn starred_repos(&self, user_id: i64, viewer: Option<&User>, offset: i64, limit: i64) -> Result<Vec<Repo>> {
        Ok(sqlx::query_as(concat!(
            repo_select!(),
            "JOIN stars s ON s.repo_id = r.id AND s.user_id = ?1 WHERE ",
            visible_to!(),
            " ORDER BY s.created_at DESC, r.id DESC LIMIT ?4 OFFSET ?5"
        ))
        .bind(user_id)
        .bind(viewer.map_or(-1, |u| u.id))
        .bind(viewer.is_some_and(|u| u.is_admin))
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.db)
        .await?)
    }

    /// How many repositories `user_id` starred that `viewer` can see.
    pub async fn starred_count(&self, user_id: i64, viewer: Option<&User>) -> Result<i64> {
        let (n,): (i64,) = sqlx::query_as(concat!(
            "SELECT COUNT(*) FROM repositories r JOIN stars s ON s.repo_id = r.id AND s.user_id = ?1 WHERE ",
            visible_to!()
        ))
        .bind(user_id)
        .bind(viewer.map_or(-1, |u| u.id))
        .bind(viewer.is_some_and(|u| u.is_admin))
        .fetch_one(&self.db)
        .await?;
        Ok(n)
    }

    /// Who starred `repo_id`, most recent first.
    pub async fn stargazers(&self, repo_id: i64, offset: i64, limit: i64) -> Result<Vec<Stargazer>> {
        Ok(sqlx::query_as(concat!(
            "SELECT ",
            user_columns!(),
            ", starred_at FROM (SELECT users.*, s.created_at AS starred_at FROM stars s JOIN users ON users.id = s.user_id
                               WHERE s.repo_id = ?1)
             ORDER BY starred_at DESC, id DESC LIMIT ?2 OFFSET ?3"
        ))
        .bind(repo_id)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.db)
        .await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn starring() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let bob = core.register("bob", "bob@example.com", "password1").await.unwrap();
        let engine = core.create_repo(&ada, "engine", "", false).await.unwrap();
        let notes = core.create_repo(&ada, "notes", "", true).await.unwrap();
        assert!(core.set_star(&bob, &engine, true).await.unwrap());
        assert!(!core.set_star(&bob, &engine, true).await.unwrap());
        assert!(core.set_star(&ada, &engine, true).await.unwrap());
        assert!(core.set_star(&ada, &notes, true).await.unwrap());
        assert_eq!(core.repo_by_id(engine.id).await.unwrap().unwrap().stars, 2);
        assert!(core.is_starred(bob.id, engine.id).await.unwrap());

        let gazers = core.stargazers(engine.id, 0, 10).await.unwrap();
        assert_eq!(gazers.len(), 2);
        assert!(gazers.iter().any(|g| g.user.username == "bob" && g.starred_at > 0));
        // Private repositories only show to those who can read them.
        assert_eq!(core.starred_repos(ada.id, Some(&bob), 0, 10).await.unwrap().len(), 1);
        assert_eq!(core.starred_count(ada.id, Some(&ada)).await.unwrap(), 2);

        assert!(core.set_star(&bob, &engine, false).await.unwrap());
        assert!(!core.set_star(&bob, &engine, false).await.unwrap());
        assert_eq!(core.repo_by_id(engine.id).await.unwrap().unwrap().stars, 1);
        // Deleted accounts take their stars along.
        core.set_star(&bob, &engine, true).await.unwrap();
        core.delete_user(&ada, &bob).await.unwrap();
        assert_eq!(core.repo_by_id(engine.id).await.unwrap().unwrap().stars, 1);
    }
}
