//! Searching issues and pull requests with qualifiers (see
//! [`keel_core::IssueQuery`]): within a repository's lists, and across
//! repositories on `/issues` and `/pulls`.

use std::collections::HashMap;

use askama::Template;
use axum::extract::{Query, State};
use keel_core::{Access, IssueQuery, Repo, StateFilter, User};
use serde::Deserialize;

use crate::error::{AppResult, html};
use crate::issues::IssueRow;
use crate::repo::RepoView;
use crate::session::Viewer;
use crate::{AppState, Page};

const PER_PAGE: i64 = 25;

/// `q` with its open/closed qualifiers replaced by `state`.
fn with_state(q: &str, state: Option<&str>) -> String {
    let mut parts: Vec<&str> = q
        .split_whitespace()
        .filter(|t| !matches!(t.to_ascii_lowercase().as_str(), "is:open" | "is:closed" | "state:open" | "state:closed"))
        .collect();
    if let Some(s) = state {
        parts.insert(0, s);
    }
    parts.join(" ")
}

fn search_url(base: &str, q: &str, page: Option<i64>) -> String {
    let mut s = form_urlencoded::Serializer::new(String::new());
    s.append_pair("q", q);
    if let Some(p) = page {
        s.append_pair("page", &p.to_string());
    }
    format!("{base}?{}", s.finish())
}

/// A page of results, with what the list around it needs.
pub(crate) struct Results {
    pub q: String,
    pub rows: Vec<IssueRow>,
    pub total: i64,
    pub open_count: i64,
    pub closed_count: i64,
    pub open_active: bool,
    pub closed_active: bool,
    pub open_url: String,
    pub closed_url: String,
    pub newer_url: Option<String>,
    pub older_url: Option<String>,
}

/// Searches with `q` in `repo` (or everywhere), as a page of rows.
/// `pulls` limits the search to issues or pull requests.
async fn results(
    state: &AppState,
    viewer: Option<&User>,
    repo: Option<&Repo>,
    q: &str,
    pulls: Option<bool>,
    page: i64,
    base: &str,
) -> AppResult<Results> {
    let mut parsed = IssueQuery::parse(q, viewer.map(|v| v.username.as_str()));
    if pulls.is_some() {
        parsed.pulls = pulls;
    }
    let repo_id = repo.map(|r| r.id);
    let (hits, total) = state.core.search_issues(viewer, repo_id, &parsed, (page - 1) * PER_PAGE, PER_PAGE).await?;
    let count = |s: StateFilter| {
        let q = IssueQuery { state: Some(s), ..parsed.clone() };
        async move { Ok::<_, keel_core::Error>(state.core.search_issues(viewer, repo_id, &q, 0, 0).await?.1) }
    };
    let (open_count, closed_count) = (count(StateFilter::Open).await?, count(StateFilter::Closed).await?);

    let ids: Vec<i64> = hits.iter().map(|h| h.issue.id).collect();
    let mut labels = state.core.labels_of(&ids).await?;
    let mut assignees = state.core.assignees_of(&ids).await?;
    let mut repos: HashMap<i64, Repo> = repo.map(|r| (r.id, r.clone())).into_iter().collect();
    let mut rows = Vec::with_capacity(hits.len());
    for h in &hits {
        if !repos.contains_key(&h.issue.repo_id)
            && let Some(r) = state.core.repo_by_id(h.issue.repo_id).await?
        {
            repos.insert(r.id, r);
        }
        let Some(r) = repos.get(&h.issue.repo_id) else { continue };
        let mut row = IssueRow::new(r, &h.issue, &mut labels, &mut assignees);
        if repo.is_none() {
            row.repo = format!("{}/{}", h.repo_owner, h.repo_name);
        }
        rows.push(row);
    }
    Ok(Results {
        open_active: parsed.state == Some(StateFilter::Open),
        closed_active: matches!(parsed.state, Some(StateFilter::Closed | StateFilter::Merged | StateFilter::Unmerged)),
        open_url: search_url(base, &with_state(q, Some("is:open")), None),
        closed_url: search_url(base, &with_state(q, Some("is:closed")), None),
        newer_url: (page > 1).then(|| search_url(base, q, Some(page - 1))),
        older_url: (page * PER_PAGE < total).then(|| search_url(base, q, Some(page + 1))),
        q: q.to_owned(),
        rows,
        total,
        open_count,
        closed_count,
    })
}

#[derive(Template)]
#[template(path = "issues/search.html")]
struct RepoSearchPage {
    page: Page,
    repo: RepoView,
    res: Results,
    action: String,
}

/// A repository's issues or pull requests matching `q`.
pub(crate) async fn repo_page(
    state: &AppState,
    viewer: Option<User>,
    repo: &Repo,
    access: Access,
    q: &str,
    pulls: bool,
    page: i64,
) -> AppResult {
    let kind = if pulls { "pulls" } else { "issues" };
    let action = format!("/{}/{}/{kind}", repo.owner, repo.name);
    let res = results(state, viewer.as_ref(), Some(repo), q, Some(pulls), page, &action).await?;
    html(RepoSearchPage {
        repo: RepoView::new(state, repo, access, viewer.as_ref(), kind, "").await?,
        page: Page::new(state, viewer, format!("{} · {}/{}", if pulls { "Pull requests" } else { "Issues" }, repo.owner, repo.name)),
        res,
        action,
    })
}

#[derive(Template)]
#[template(path = "issues/global.html")]
struct GlobalPage {
    page: Page,
    pulls: bool,
    res: Results,
    action: String,
    /// Shortcuts for the signed-in viewer: (label, url, active).
    tabs: Vec<(&'static str, String, bool)>,
}

#[derive(Deserialize)]
pub struct SearchQuery {
    q: Option<String>,
    page: Option<i64>,
}

async fn global(state: AppState, viewer: Option<User>, q: SearchQuery, pulls: bool) -> AppResult {
    let (kind, base) = if pulls { ("is:pr", "/pulls") } else { ("is:issue", "/issues") };
    let default = if viewer.is_some() { format!("is:open {kind} involves:@me") } else { format!("is:open {kind}") };
    let text = q.q.map(|t| t.trim().to_owned()).filter(|t| !t.is_empty()).unwrap_or(default);
    let page = q.page.unwrap_or(1).max(1);
    let res = results(&state, viewer.as_ref(), None, &text, None, page, base).await?;
    let mut shortcuts = vec![("Involving you", "involves:@me"), ("Created", "author:@me"), ("Assigned", "assignee:@me")];
    if pulls {
        shortcuts.push(("Review requests", "review-requested:@me"));
    }
    let tabs = match viewer {
        Some(_) => shortcuts
            .into_iter()
            .map(|(label, who)| {
                let tab_q = format!("is:open {kind} {who}");
                let active = text == tab_q;
                (label, search_url(base, &tab_q, None), active)
            })
            .collect(),
        None => Vec::new(),
    };
    html(GlobalPage {
        page: Page::new(&state, viewer, if pulls { "Pull requests" } else { "Issues" }),
        pulls,
        res,
        action: base.to_owned(),
        tabs,
    })
}

pub async fn issues(State(state): State<AppState>, Viewer(viewer): Viewer, Query(q): Query<SearchQuery>) -> AppResult {
    global(state, viewer, q, false).await
}

pub async fn pulls(State(state): State<AppState>, Viewer(viewer): Viewer, Query(q): Query<SearchQuery>) -> AppResult {
    global(state, viewer, q, true).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_tabs() {
        assert_eq!(with_state("is:open label:bug crash", Some("is:closed")), "is:closed label:bug crash");
        assert_eq!(with_state("state:closed author:ada", None), "author:ada");
        assert_eq!(search_url("/issues", "is:open x", Some(2)), "/issues?q=is%3Aopen+x&page=2");
    }
}
