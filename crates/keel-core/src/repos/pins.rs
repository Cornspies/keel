//! Repositories pinned to the top of a profile.

use crate::{Core, Repo, Result, User, invalid};

pub const MAX_PINS: usize = 6;

impl Core {
    /// What `owner_id` pinned that `viewer` can see, in their order.
    pub async fn pinned_repos(&self, owner_id: i64, viewer: Option<&User>) -> Result<Vec<Repo>> {
        Ok(sqlx::query_as(concat!(
            repo_select!(),
            "JOIN pinned_repos p ON p.repo_id = r.id AND p.owner_id = ?1 WHERE ",
            visible_to!(),
            " ORDER BY p.position"
        ))
        .bind(owner_id)
        .bind(viewer.map_or(-1, |u| u.id))
        .bind(viewer.is_some_and(|u| u.is_admin))
        .fetch_all(&self.db)
        .await?)
    }

    /// Pins `repo_ids` (in this order) to `owner`'s profile, replacing what
    /// was pinned. Only `owner`'s own repositories can be pinned.
    pub async fn set_pins(&self, owner: &User, repo_ids: &[i64]) -> Result<()> {
        if repo_ids.len() > MAX_PINS {
            return invalid(format!("At most {MAX_PINS} repositories can be pinned."));
        }
        let mut tx = self.db.begin().await?;
        sqlx::query("DELETE FROM pinned_repos WHERE owner_id = ?1").bind(owner.id).execute(&mut *tx).await?;
        for (position, id) in repo_ids.iter().enumerate() {
            // Silently skips repositories that aren't the owner's.
            sqlx::query(
                "INSERT OR IGNORE INTO pinned_repos (owner_id, repo_id, position)
                 SELECT ?1, id, ?3 FROM repositories WHERE id = ?2 AND owner_id = ?1",
            )
            .bind(owner.id)
            .bind(id)
            .bind(position as i64)
            .execute(&mut *tx)
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn pinning() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let bob = core.register("bob", "bob@example.com", "password1").await.unwrap();
        let a = core.create_repo(&ada, "a", "", false).await.unwrap();
        let b = core.create_repo(&ada, "b", "", true).await.unwrap();
        let theirs = core.create_repo(&bob, "c", "", false).await.unwrap();
        core.set_pins(&ada, &[b.id, theirs.id, a.id]).await.unwrap();
        let names = |repos: Vec<Repo>| repos.into_iter().map(|r| r.name).collect::<Vec<_>>();
        assert_eq!(names(core.pinned_repos(ada.id, Some(&ada)).await.unwrap()), ["b", "a"]);
        assert_eq!(names(core.pinned_repos(ada.id, Some(&bob)).await.unwrap()), ["a"], "private ones stay hidden");
        assert!(core.set_pins(&ada, &[1, 2, 3, 4, 5, 6, 7]).await.is_err());
        core.set_pins(&ada, &[]).await.unwrap();
        assert!(core.pinned_repos(ada.id, None).await.unwrap().is_empty());
    }
}
