//! Showing commit statuses: a badge per commit, and the list of checks.

use keel_core::{CommitStatus, Repo, combined_state};

use crate::render::{datetime, escape, relative_time};
use crate::repo::CommitView;
use crate::{AppState, error::AppResult};

/// One status, as listed on a commit or pull request.
pub(crate) struct CheckView {
    pub context: String,
    pub state: String,
    pub description: String,
    /// Where the service shows details (http(s) only).
    pub url: Option<String>,
    pub when: String,
    pub when_full: String,
    /// Branch protection requires it.
    pub required: bool,
}

impl CheckView {
    pub(crate) fn new(s: &CommitStatus, required: bool) -> Self {
        Self {
            context: s.context.clone(),
            state: s.state.clone(),
            description: s.description.clone(),
            url: (s.target_url.starts_with("https://") || s.target_url.starts_with("http://")).then(|| s.target_url.clone()),
            when: relative_time(s.updated_at),
            when_full: datetime(s.updated_at),
            required,
        }
    }
}

/// A small icon for the combined state, linking to the commit's checks;
/// empty without statuses.
pub(crate) fn badge(statuses: &[CommitStatus], commit_url: &str) -> String {
    let Some(state) = combined_state(statuses) else { return String::new() };
    let n = statuses.len();
    let ok = statuses.iter().filter(|s| s.is_success()).count();
    let failed = statuses.iter().filter(|s| s.state == "failure" || s.state == "error").count();
    let title = match state {
        "success" if n == 1 => "The check passed".to_owned(),
        "success" => format!("All {n} checks passed"),
        "pending" => format!("{ok} of {n} checks passed, the rest are running"),
        _ => format!("{failed} of {n} checks failed"),
    };
    format!(
        "<a class=\"ci-status ci-{state}\" href=\"{}#checks\" title=\"{title}\" aria-label=\"{title}\"></a>",
        escape(commit_url)
    )
}

/// Fills in the status badges of `commits`.
pub(crate) async fn attach(state: &AppState, repo: &Repo, commits: &mut [&mut CommitView]) -> AppResult<()> {
    let shas: Vec<String> = commits.iter().map(|c| c.id.clone()).collect();
    let mut found = state.core.statuses_of(repo.id, &shas).await?;
    for c in commits.iter_mut() {
        if let Some(list) = found.remove(&c.id) {
            c.status = badge(&list, &c.url);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status(context: &str, state: &str) -> CommitStatus {
        CommitStatus {
            id: 1,
            repo_id: 1,
            sha: "a".repeat(40),
            context: context.into(),
            state: state.into(),
            description: String::new(),
            target_url: String::new(),
            creator: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    #[test]
    fn badges() {
        assert_eq!(badge(&[], "/c"), "");
        let ok = badge(&[status("ci", "success")], "/a/b/commit/1");
        assert!(ok.contains("ci-success") && ok.contains("The check passed") && ok.contains("/a/b/commit/1#checks"));
        let mixed = [status("ci", "success"), status("lint", "pending")];
        assert!(badge(&mixed, "/c").contains("1 of 2 checks passed, the rest are running"));
        let failed = [status("ci", "error"), status("lint", "success")];
        assert!(badge(&failed, "/c").contains("ci-failure") && badge(&failed, "/c").contains("1 of 2 checks failed"));
        // Only http(s) links are shown.
        let mut s = status("ci", "success");
        s.target_url = "javascript:alert(1)".into();
        assert_eq!(CheckView::new(&s, false).url, None);
    }
}
