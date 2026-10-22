//! "Last commit per directory entry", the slowest thing a tree page shows,
//! served from three cache levels: memory, the database (survives
//! restarts), and incremental updates from a stored result for an ancestor
//! commit. Only when all of those miss is history walked from scratch.

use std::collections::HashMap;
use std::sync::Arc;

use keel_core::{EntryCommits, Repo, RepoEvent};
use keel_git::{CommitSummary, GitRepo, Oid};

use crate::AppState;
use crate::error::AppResult;
use crate::repo::with_git;

/// Commits an incremental update may walk before it is cheaper to start
/// over.
const INCREMENTAL_MAX_COMMITS: usize = 20_000;
/// Budget when computing in the background (after a push, or for the
/// browser's late request): effectively unlimited.
pub const BACKGROUND_WALK: usize = 1_000_000;

pub type LastCommits = Arc<HashMap<String, CommitSummary>>;

/// Returns the last commit of every entry of `path` at `commit`, or `None`
/// if it couldn't be computed within `budget` commits of history.
pub async fn last_commits_for(
    state: &AppState,
    repo: &Repo,
    commit: Oid,
    path: &str,
    budget: usize,
) -> AppResult<Option<LastCommits>> {
    let key = (commit, path.to_owned());
    if let Some(hit) = state.last_commits.get(&key) {
        return Ok(Some(hit));
    }
    let stored = state.core.stored_last_commits(repo.id, path, &commit.to_string()).await?;
    let bases = match stored {
        Some(_) => Vec::new(),
        None => state.core.recent_last_commits(repo.id, path).await?,
    };

    let dir = path.to_owned();
    let (found, computed) = with_git(state, repo, move |g| {
        if let Some(stored) = stored
            && let Some(found) = summaries(g, &stored)
        {
            return Ok((Some(found), false));
        }
        for (base_commit, entries) in &bases {
            let Ok(base_commit) = Oid::from_str(base_commit) else { continue };
            let ids = entries
                .iter()
                .filter_map(|(name, id)| Some((name.clone(), Oid::from_str(id).ok()?)))
                .collect();
            // A base that no longer exists (force push + gc) just doesn't help.
            if let Ok(Some(found)) = g.last_commits_since(commit, &dir, base_commit, &ids, INCREMENTAL_MAX_COMMITS) {
                return Ok((Some(found), true));
            }
        }
        let (found, complete) = g.last_commits(commit, &dir, budget)?;
        Ok((complete.then_some(found), true))
    })
    .await?;

    let Some(found) = found else {
        return Ok(None);
    };
    if computed {
        let ids: EntryCommits = found.iter().map(|(name, c)| (name.clone(), c.id.to_string())).collect();
        state.core.store_last_commits(repo.id, path, &commit.to_string(), &ids).await?;
    }
    let found = Arc::new(found);
    state.last_commits.insert(key, found.clone());
    Ok(Some(found))
}

/// Looks up the commits of a stored result; `None` if any is missing.
fn summaries(g: &GitRepo, stored: &EntryCommits) -> Option<HashMap<String, CommitSummary>> {
    stored
        .iter()
        .map(|(name, id)| Some((name.clone(), g.commit_summary(Oid::from_str(id).ok()?).ok()?)))
        .collect()
}

/// After each push, computes the default branch's root listing in the
/// background, so the first visitor gets a complete page.
pub fn warm_after_pushes(state: AppState) {
    let mut events = state.core.subscribe();
    tokio::spawn(async move {
        loop {
            let repo_id = match events.recv().await {
                Ok(RepoEvent::Pushed { repo_id }) => repo_id,
                Ok(_) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
            };
            if let Err(e) = warm(&state, repo_id).await {
                tracing::warn!(repo = repo_id, "warming last commits: {e:?}");
            }
        }
    });
}

async fn warm(state: &AppState, repo_id: i64) -> AppResult<()> {
    let Some(repo) = state.core.repo_by_id(repo_id).await? else {
        return Ok(());
    };
    let head = with_git(state, &repo, |g| {
        Ok(g.default_branch()?.and_then(|b| g.resolve_rev(&b)))
    })
    .await?;
    if let Some(head) = head {
        let started = std::time::Instant::now();
        last_commits_for(state, &repo, head, "", BACKGROUND_WALK).await?;
        tracing::debug!(repo = repo_id, "warmed last commits in {:?}", started.elapsed());
    }
    Ok(())
}
