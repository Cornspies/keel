//! Commit statuses: results that CI and other services report for commits,
//! one per context (e.g. `ci/build`), like GitHub's status API.

use std::collections::HashMap;

use crate::activity::Activity;
use crate::{Core, Repo, Result, User, invalid, now};

pub const STATUS_STATES: &[&str] = &["pending", "success", "failure", "error"];
/// Contexts a commit can have.
const MAX_CONTEXTS: i64 = 100;
const MAX_DESCRIPTION: usize = 140;

#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct CommitStatus {
    pub id: i64,
    pub repo_id: i64,
    pub sha: String,
    pub context: String,
    /// `pending`, `success`, `failure` or `error`.
    pub state: String,
    pub description: String,
    pub target_url: String,
    pub creator: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl CommitStatus {
    pub fn is_success(&self) -> bool {
        self.state == "success"
    }
}

/// The state of a commit as a whole: `failure` if any status failed (or
/// errored), else `pending` if any is pending, else `success`; `None`
/// without statuses.
pub fn combined_state(statuses: &[CommitStatus]) -> Option<&'static str> {
    if statuses.is_empty() {
        None
    } else if statuses.iter().any(|s| s.state == "failure" || s.state == "error") {
        Some("failure")
    } else if statuses.iter().any(|s| s.state == "pending") {
        Some("pending")
    } else {
        Some("success")
    }
}

macro_rules! status_select {
    () => {
        "SELECT s.id, s.repo_id, s.sha, s.context, s.state, s.description, s.target_url,
            u.username AS creator, s.created_at, s.updated_at
         FROM commit_statuses s LEFT JOIN users u ON u.id = s.creator_id "
    };
}

impl Core {
    /// Sets the status of `context` for commit `sha` (a full id the caller
    /// checked exists), replacing an earlier one for that context.
    #[allow(clippy::too_many_arguments)]
    pub async fn set_status(
        &self,
        repo: &Repo,
        creator: &User,
        sha: &str,
        state: &str,
        context: &str,
        description: &str,
        target_url: &str,
    ) -> Result<CommitStatus> {
        if !STATUS_STATES.contains(&state) {
            return invalid("state must be pending, success, failure or error.");
        }
        let context = match context.trim() {
            "" => "default",
            c => c,
        };
        if context.chars().count() > 100 || context.contains(',') || context.chars().any(char::is_control) {
            return invalid("Contexts are at most 100 characters, without commas.");
        }
        let target_url = target_url.trim();
        if !target_url.is_empty()
            && (!(target_url.starts_with("https://") || target_url.starts_with("http://")) || target_url.len() > 2000)
        {
            return invalid("target_url must be an http(s) URL.");
        }
        // Like GitHub, long descriptions are shortened rather than refused.
        let description: String = description.trim().chars().take(MAX_DESCRIPTION).collect();
        if sha.len() != 40 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            return invalid("Statuses are set on full commit ids.");
        }
        let sha = sha.to_ascii_lowercase();

        let (contexts, exists): (i64, bool) = sqlx::query_as(
            "SELECT COUNT(*), COALESCE(SUM(context = ?3), 0) > 0 FROM commit_statuses WHERE repo_id = ?1 AND sha = ?2",
        )
        .bind(repo.id)
        .bind(&sha)
        .bind(context)
        .fetch_one(&self.db)
        .await?;
        if !exists && contexts >= MAX_CONTEXTS {
            return invalid(format!("A commit can have at most {MAX_CONTEXTS} statuses."));
        }
        let now = now();
        let (id,): (i64,) = sqlx::query_as(
            "INSERT INTO commit_statuses
                (repo_id, sha, context, state, description, target_url, creator_id, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
             ON CONFLICT (repo_id, sha, context) DO UPDATE SET state = excluded.state,
                description = excluded.description, target_url = excluded.target_url,
                creator_id = excluded.creator_id, updated_at = excluded.updated_at
             RETURNING id",
        )
        .bind(repo.id)
        .bind(&sha)
        .bind(context)
        .bind(state)
        .bind(&description)
        .bind(target_url)
        .bind(creator.id)
        .bind(now)
        .fetch_one(&self.db)
        .await?;
        let status: CommitStatus = sqlx::query_as(concat!(status_select!(), "WHERE s.id = ?1"))
            .bind(id)
            .fetch_one(&self.db)
            .await?;
        self.record(Activity::Status { repo_id: repo.id, status: status.clone(), actor: creator.id }).await;
        Ok(status)
    }

    /// A commit's statuses, by context.
    pub async fn statuses(&self, repo_id: i64, sha: &str) -> Result<Vec<CommitStatus>> {
        Ok(sqlx::query_as(concat!(status_select!(), "WHERE s.repo_id = ?1 AND s.sha = ?2 ORDER BY s.context"))
            .bind(repo_id)
            .bind(sha)
            .fetch_all(&self.db)
            .await?)
    }

    /// The statuses of several commits, by commit id.
    pub async fn statuses_of(&self, repo_id: i64, shas: &[String]) -> Result<HashMap<String, Vec<CommitStatus>>> {
        let mut out: HashMap<String, Vec<CommitStatus>> = HashMap::new();
        if shas.is_empty() {
            return Ok(out);
        }
        let ids = serde_json::to_string(shas).map_err(anyhow::Error::from)?;
        let rows: Vec<CommitStatus> = sqlx::query_as(concat!(
            status_select!(),
            "WHERE s.repo_id = ?1 AND s.sha IN (SELECT value FROM json_each(?2)) ORDER BY s.context"
        ))
        .bind(repo_id)
        .bind(ids)
        .fetch_all(&self.db)
        .await?;
        for s in rows {
            out.entry(s.sha.clone()).or_default().push(s);
        }
        Ok(out)
    }

    /// Contexts reported in the last 30 days, for choosing required checks.
    pub async fn recent_contexts(&self, repo_id: i64) -> Result<Vec<String>> {
        let rows: Vec<(String,)> = sqlx::query_as(
            "SELECT context FROM commit_statuses WHERE repo_id = ?1 AND updated_at > ?2
             GROUP BY context ORDER BY MAX(updated_at) DESC LIMIT 20",
        )
        .bind(repo_id)
        .bind(now() - 30 * 86_400)
        .fetch_all(&self.db)
        .await?;
        Ok(rows.into_iter().map(|(c,)| c).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn statuses() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let repo = core.create_repo(&ada, "engine", "", false).await.unwrap();
        let sha = "a".repeat(40);
        let set = |state: &'static str, context: &'static str| {
            let (core, repo, ada, sha) = (core.clone(), repo.clone(), ada.clone(), sha.clone());
            async move { core.set_status(&repo, &ada, &sha, state, context, "", "").await }
        };

        assert!(set("great", "ci").await.is_err());
        assert!(core.set_status(&repo, &ada, "abc", "success", "ci", "", "").await.is_err(), "full ids only");
        assert!(core.set_status(&repo, &ada, &sha, "success", "ci", "", "javascript:x").await.is_err());
        assert!(set("success", "a,b").await.is_err());
        let s = set("pending", "ci/build").await.unwrap();
        assert_eq!((s.context.as_str(), s.creator.as_deref()), ("ci/build", Some("ada")));
        let long = "x".repeat(500);
        let lint = core.set_status(&repo, &ada, &sha.to_uppercase(), "success", "", &long, "https://ci.example.com/1").await.unwrap();
        assert_eq!((lint.context.as_str(), lint.description.len(), lint.sha.as_str()), ("default", 140, sha.as_str()));

        assert_eq!(combined_state(&core.statuses(repo.id, &sha).await.unwrap()), Some("pending"));
        set("failure", "ci/build").await.unwrap();
        let all = core.statuses(repo.id, &sha).await.unwrap();
        assert_eq!(all.len(), 2, "replaced, not added");
        assert_eq!(combined_state(&all), Some("failure"));
        set("success", "ci/build").await.unwrap();
        assert_eq!(combined_state(&core.statuses(repo.id, &sha).await.unwrap()), Some("success"));
        assert_eq!(combined_state(&[]), None);

        let by_sha = core.statuses_of(repo.id, &[sha.clone(), "b".repeat(40)]).await.unwrap();
        assert_eq!((by_sha.len(), by_sha[&sha].len()), (1, 2));
        assert_eq!(core.recent_contexts(repo.id).await.unwrap().len(), 2);
    }
}
