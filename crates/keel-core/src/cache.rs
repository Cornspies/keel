//! Storage for computed "last commit per entry" results.

use std::collections::HashMap;

use crate::{Core, Result, now};

/// Results kept per (repository, directory): a few recent ones are enough to
/// find an ancestor of whatever is pushed next.
const KEEP_PER_PATH: i64 = 4;

/// Entry name → commit id (hex).
pub type EntryCommits = HashMap<String, String>;

impl Core {
    pub async fn stored_last_commits(&self, repo_id: i64, path: &str, commit: &str) -> Result<Option<EntryCommits>> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT entries FROM last_commit_cache WHERE repo_id = ?1 AND path = ?2 AND commit_id = ?3",
        )
        .bind(repo_id)
        .bind(path)
        .bind(commit)
        .fetch_optional(&self.db)
        .await?;
        Ok(row.and_then(|(json,)| serde_json::from_str(&json).ok()))
    }

    /// The most recent stored results for a directory, newest first, as
    /// candidates for an incremental update.
    pub async fn recent_last_commits(&self, repo_id: i64, path: &str) -> Result<Vec<(String, EntryCommits)>> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT commit_id, entries FROM last_commit_cache
             WHERE repo_id = ?1 AND path = ?2 ORDER BY id DESC LIMIT ?3",
        )
        .bind(repo_id)
        .bind(path)
        .bind(KEEP_PER_PATH)
        .fetch_all(&self.db)
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(c, json)| Some((c, serde_json::from_str(&json).ok()?)))
            .collect())
    }

    pub async fn store_last_commits(&self, repo_id: i64, path: &str, commit: &str, entries: &EntryCommits) -> Result<()> {
        let json = serde_json::to_string(entries).map_err(anyhow::Error::from)?;
        let mut tx = self.db.begin().await?;
        sqlx::query(
            "INSERT OR REPLACE INTO last_commit_cache (repo_id, path, commit_id, entries, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(repo_id)
        .bind(path)
        .bind(commit)
        .bind(json)
        .bind(now())
        .execute(&mut *tx)
        .await?;
        sqlx::query(
            "DELETE FROM last_commit_cache WHERE repo_id = ?1 AND path = ?2 AND commit_id NOT IN (
                SELECT commit_id FROM last_commit_cache WHERE repo_id = ?1 AND path = ?2
                ORDER BY id DESC LIMIT ?3)",
        )
        .bind(repo_id)
        .bind(path)
        .bind(KEEP_PER_PATH)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(())
    }
}

impl Core {
    /// The commit the search index currently holds for a repository.
    pub async fn search_indexed_commit(&self, repo_id: i64) -> Result<Option<String>> {
        let row: Option<(String,)> = sqlx::query_as("SELECT commit_id FROM search_index_state WHERE repo_id = ?1")
            .bind(repo_id)
            .fetch_optional(&self.db)
            .await?;
        Ok(row.map(|(c,)| c))
    }

    pub async fn set_search_indexed_commit(&self, repo_id: i64, commit: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO search_index_state (repo_id, commit_id, indexed_at) VALUES (?1, ?2, ?3)
             ON CONFLICT (repo_id) DO UPDATE SET commit_id = excluded.commit_id, indexed_at = excluded.indexed_at",
        )
        .bind(repo_id)
        .bind(commit)
        .bind(now())
        .execute(&self.db)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn stores_and_prunes() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let repo = core.create_repo(&ada, "r", "", false).await.unwrap();
        let entries: EntryCommits = [("README.md".to_owned(), "abc".to_owned())].into();

        for i in 0..6 {
            core.store_last_commits(repo.id, "", &format!("c{i}"), &entries).await.unwrap();
        }
        assert_eq!(core.stored_last_commits(repo.id, "", "c5").await.unwrap(), Some(entries.clone()));
        assert_eq!(core.stored_last_commits(repo.id, "", "c0").await.unwrap(), None, "pruned");
        assert_eq!(core.recent_last_commits(repo.id, "").await.unwrap().len(), KEEP_PER_PATH as usize);
        assert!(core.recent_last_commits(repo.id, "src").await.unwrap().is_empty());

        core.delete_repo(&repo).await.unwrap();
        assert!(core.recent_last_commits(repo.id, "").await.unwrap().is_empty(), "cascade");
    }
}
