//! Verified badges on commits.

use keel_core::{Repo, Verification};
use keel_git::Oid;

use crate::repo::CommitView;
use crate::{AppState, error::AppResult};

/// A signed commit's badge.
pub(crate) struct SignatureView {
    pub verified: bool,
    /// What was checked, in a sentence.
    pub title: String,
}

impl From<&Verification> for SignatureView {
    fn from(v: &Verification) -> Self {
        Self { verified: v.verified, title: v.describe() }
    }
}

/// Fills in the signature badges of those `commits` (of `repo`) that are
/// signed.
pub(crate) async fn attach(state: &AppState, repo: &Repo, commits: &mut [&mut CommitView]) -> AppResult<()> {
    let ids = commits.iter().filter_map(|c| Oid::from_str(&c.id).ok()).collect();
    let found = state.core.verify_commits(repo.id, ids).await?;
    for c in commits.iter_mut() {
        if let Some(v) = Oid::from_str(&c.id).ok().and_then(|id| found.get(&id)) {
            c.signature = Some(v.into());
        }
    }
    Ok(())
}
