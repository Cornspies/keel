//! Pull request pages: comparing branches, opening, reviewing and merging.

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Redirect};
use keel_core::{Access, Issue, PullRequest, Repo, User, pull_ref};
use keel_git::{CommitSummary, Identity, MergeStyle, Mergeability, Oid};
use serde::Deserialize;

use crate::error::{AppResult, html};
use crate::issues::{Conversation, ListQuery, conversation, issue_url, list_page, load_issue};
use crate::repo::{
    CommitView, DIFF_MAX_FILES, DIFF_MAX_LINES, FileView, RepoView, encode_path, file_views, load, with_git,
};
use crate::session::{SignedIn, Viewer};
use crate::{AppError, AppState, Page};

const MAX_COMMITS: usize = 250;

pub async fn list(
    State(state): State<AppState>,
    Viewer(viewer): Viewer,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<ListQuery>,
) -> AppResult {
    list_page(state, viewer, owner, name, q, true).await
}

/// A diff and its commits, for the compare page and "Files changed".
struct Changes {
    commits: Vec<CommitView>,
    files: Vec<FileView>,
    additions: usize,
    deletions: usize,
    truncated: bool,
}

/// Commits in `head` but not `base`, and the diff from their merge base.
async fn changes(state: &AppState, repo: &Repo, base: Oid, head: Oid) -> AppResult<Changes> {
    let url = format!("/{}/{}", repo.owner, repo.name);
    let (commits, diff) = with_git(state, repo, move |g| {
        let merge_base = g.merge_base(base, head)?;
        Ok((
            g.commits_between(base, head, MAX_COMMITS)?,
            g.diff_range(merge_base, head, DIFF_MAX_FILES, DIFF_MAX_LINES)?,
        ))
    })
    .await?;
    Ok(Changes {
        commits: commits.iter().map(|c| CommitView::new(&url, c)).collect(),
        files: file_views(&url, &head.to_string(), &diff),
        additions: diff.additions,
        deletions: diff.deletions,
        truncated: diff.truncated,
    })
}

// ---------------------------------------------------------------------------
// Compare and create
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "pulls/compare.html")]
struct ComparePage {
    page: Page,
    repo: RepoView,
    branches: Vec<String>,
    base: String,
    head: String,
    /// Why nothing can be compared, if so.
    notice: Option<String>,
    changes: Option<Changes>,
    existing: Option<(i64, String)>,
    can_open: bool,
    title: String,
    body: String,
    error: Option<String>,
    // Fields used by the shared diff partial.
    files: Vec<FileView>,
    additions: usize,
    deletions: usize,
    truncated: bool,
}

#[derive(Deserialize)]
pub struct CompareQuery {
    base: Option<String>,
    head: Option<String>,
}

/// `/compare`: picks sensible branches (or those from the form) and
/// redirects to `/compare/<base>...<head>`.
pub async fn compare_index(
    State(state): State<AppState>,
    Viewer(viewer): Viewer,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<CompareQuery>,
) -> AppResult {
    let (repo, _) = load(&state, viewer.as_ref(), &owner, &name).await?;
    let (default, branches) = with_git(&state, &repo, |g| {
        Ok((g.default_branch()?.unwrap_or_default(), g.branches()?))
    })
    .await?;
    let base = q.base.unwrap_or_else(|| default.clone());
    // Default head: the most recently updated other branch.
    let head = q
        .head
        .or_else(|| branches.iter().find(|b| b.name != base).map(|b| b.name.clone()))
        .unwrap_or_else(|| base.clone());
    Ok(Redirect::to(&format!(
        "/{}/{}/compare/{}...{}",
        repo.owner,
        repo.name,
        encode_path(&base),
        encode_path(&head)
    ))
    .into_response())
}

pub async fn compare(
    State(state): State<AppState>,
    Viewer(viewer): Viewer,
    Path((owner, name, spec)): Path<(String, String, String)>,
) -> AppResult {
    let (repo, access) = load(&state, viewer.as_ref(), &owner, &name).await?;
    let (base, head) = match spec.split_once("...") {
        Some((b, h)) => (b.to_owned(), h.to_owned()),
        None => {
            let default = with_git(&state, &repo, |g| Ok(g.default_branch()?.unwrap_or_default())).await?;
            (default, spec)
        }
    };
    compare_page(&state, viewer, &repo, access, base, head, String::new(), String::new(), None).await
}

#[allow(clippy::too_many_arguments)]
async fn compare_page(
    state: &AppState,
    viewer: Option<User>,
    repo: &Repo,
    access: Access,
    base: String,
    head: String,
    title: String,
    body: String,
    error: Option<String>,
) -> AppResult {
    let (b, h) = (base.clone(), head.clone());
    let (default, branches, tips) = with_git(state, repo, move |g| {
        Ok((
            g.default_branch()?.unwrap_or_default(),
            g.branches()?.into_iter().map(|r| r.name).collect::<Vec<_>>(),
            (g.branch_tip(&b), g.branch_tip(&h)),
        ))
    })
    .await?;

    let mut notice = None;
    let mut changes_found = None;
    match tips {
        _ if base == head => notice = Some("Choose two different branches to compare.".to_owned()),
        (Some(base_tip), Some(head_tip)) => {
            let c = changes(state, repo, base_tip, head_tip).await?;
            if c.commits.is_empty() {
                notice = Some(format!("{base} is up to date with all commits from {head}."));
            } else {
                changes_found = Some(c);
            }
        }
        _ => notice = Some("One of these branches doesn't exist.".to_owned()),
    }

    let existing = open_pull_for(state, repo, &base, &head).await?;
    let title = match (&changes_found, title.is_empty()) {
        (Some(c), true) if c.commits.len() == 1 => c.commits[0].summary.clone(),
        (_, true) => head.replace(['-', '_', '/'], " "),
        _ => title,
    };
    let (files, additions, deletions, truncated) = match &mut changes_found {
        Some(c) => (std::mem::take(&mut c.files), c.additions, c.deletions, c.truncated),
        None => (Vec::new(), 0, 0, false),
    };
    html(ComparePage {
        repo: RepoView::new(state, repo, access, "pulls", &default).await?,
        can_open: viewer.is_some() && existing.is_none(),
        page: Page::new(state, viewer, format!("Compare {base}...{head} · {}/{}", repo.owner, repo.name)),
        branches,
        base,
        head,
        notice,
        changes: changes_found,
        existing,
        title,
        body,
        error,
        files,
        additions,
        deletions,
        truncated,
    })
}

async fn open_pull_for(state: &AppState, repo: &Repo, base: &str, head: &str) -> AppResult<Option<(i64, String)>> {
    for issue in state.core.issues(repo.id, true, true, 0, 1000).await? {
        if let Some(p) = state.core.pull(issue.id).await?
            && p.base_ref == base
            && p.head_ref == head
        {
            return Ok(Some((issue.number, issue_url(repo, &issue))));
        }
    }
    Ok(None)
}

#[derive(Deserialize)]
pub struct CreateForm {
    base: String,
    head: String,
    title: String,
    #[serde(default)]
    body: String,
}

pub async fn create(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name)): Path<(String, String)>,
    Form(f): Form<CreateForm>,
) -> AppResult {
    let (repo, access) = load(&state, Some(&user), &owner, &name).await?;
    match state.core.create_pull(&repo, &user, &f.title, &f.body, &f.base, &f.head).await {
        Ok(issue) => Ok(Redirect::to(&issue_url(&repo, &issue)).into_response()),
        Err(keel_core::Error::Invalid(msg)) => {
            compare_page(&state, Some(user), &repo, access, f.base, f.head, f.title, f.body, Some(msg)).await
        }
        Err(e) => Err(e.into()),
    }
}

// ---------------------------------------------------------------------------
// Pull request pages
// ---------------------------------------------------------------------------

/// What the merge box shows for an open pull request.
pub(crate) enum MergeStatus {
    Ready,
    Conflicts(Vec<String>),
    /// The base already contains the branch; it'll be marked merged.
    AlreadyMerged,
    BranchMissing,
}

/// Shared by the three tabs.
pub(crate) struct PullHeader {
    pub base_ref: String,
    pub head_ref: String,
    pub commit_count: usize,
    pub file_count: usize,
    pub tab: &'static str,
    pub url: String,
    pub merged_by: Option<String>,
    pub merge_commit: Option<(String, String)>,
}

struct Loaded {
    issue: Issue,
    pull: PullRequest,
    base: Option<Oid>,
    head: Option<Oid>,
    /// Current tips, for open pull requests.
    base_tip: Option<Oid>,
    head_tip: Option<Oid>,
}

/// Loads a pull request and the commits its views compare.
async fn load_pull(state: &AppState, repo: &Repo, number: i64) -> AppResult<Loaded> {
    let issue = load_issue(state, repo, number).await?;
    let pull = state.core.pull(issue.id).await?.ok_or(AppError::NotFound)?;
    let (base_ref, head_ref) = (pull.base_ref.clone(), pull.head_ref.clone());
    let stored_base = pull.base_commit.as_deref().and_then(|c| Oid::from_str(c).ok());
    let stored_head = pull.head_commit.as_deref().and_then(|c| Oid::from_str(c).ok());
    let open = issue.is_open();
    let (base, head, base_tip, head_tip) = with_git(state, repo, move |g| {
        let base_tip = g.branch_tip(&base_ref);
        let head_tip = g.branch_tip(&head_ref);
        let pinned = g.ref_tip(&pull_ref(number));
        // Open: compare the live branches. Merged/closed: what was recorded.
        let head = if open { head_tip.or(pinned) } else { stored_head.or(pinned) };
        let base = match (open, head, base_tip) {
            (true, Some(h), Some(b)) => g.merge_base(b, h)?,
            _ => stored_base,
        };
        Ok((base, head, base_tip, head_tip))
    })
    .await?;
    Ok(Loaded { issue, pull, base, head, base_tip, head_tip })
}

async fn header(state: &AppState, repo: &Repo, l: &Loaded, tab: &'static str) -> AppResult<(PullHeader, Option<Changes>)> {
    let changes = match (l.base, l.head) {
        (Some(base), Some(head)) => Some(changes(state, repo, base, head).await?),
        _ => None,
    };
    let url = format!("/{}/{}", repo.owner, repo.name);
    Ok((
        PullHeader {
            base_ref: l.pull.base_ref.clone(),
            head_ref: l.pull.head_ref.clone(),
            commit_count: changes.as_ref().map_or(0, |c| c.commits.len()),
            file_count: changes.as_ref().map_or(0, |c| c.files.len()),
            tab,
            url: issue_url(repo, &l.issue),
            merged_by: l.pull.merged_by.clone(),
            merge_commit: l
                .pull
                .merge_commit
                .as_ref()
                .map(|c| (c[..10].to_owned(), format!("{url}/commit/{c}"))),
        },
        changes,
    ))
}

#[derive(Template)]
#[template(path = "pulls/show.html")]
struct PullPage {
    page: Page,
    repo: RepoView,
    pr: PullHeader,
    conv: Conversation,
    merge: Option<MergeStatus>,
    can_merge: bool,
    error: Option<String>,
}

pub async fn show(
    State(state): State<AppState>,
    Viewer(viewer): Viewer,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> AppResult {
    let (repo, access) = load(&state, viewer.as_ref(), &owner, &name).await?;
    show_page(&state, viewer, &repo, access, number, None).await
}

async fn show_page(
    state: &AppState,
    viewer: Option<User>,
    repo: &Repo,
    access: Access,
    number: i64,
    error: Option<String>,
) -> AppResult {
    let l = load_pull(state, repo, number).await?;
    if !l.issue.is_pull {
        return Ok(Redirect::to(&issue_url(repo, &l.issue)).into_response());
    }
    let (pr, _) = header(state, repo, &l, "conversation").await?;
    let merge = if l.issue.is_open() {
        Some(merge_status(state, repo, l.base_tip, l.head_tip).await?)
    } else {
        None
    };
    let conv = conversation(state, repo, access, &l.issue, viewer.as_ref()).await?;
    html(PullPage {
        repo: RepoView::new(state, repo, access, "pulls", "").await?,
        page: Page::new(
            state,
            viewer,
            format!("{} · Pull request #{} · {}/{}", l.issue.title, number, repo.owner, repo.name),
        ),
        can_merge: access >= Access::Write,
        pr,
        conv,
        merge,
        error,
    })
}

async fn merge_status(
    state: &AppState,
    repo: &Repo,
    base_tip: Option<Oid>,
    head_tip: Option<Oid>,
) -> AppResult<MergeStatus> {
    let (Some(base), Some(head)) = (base_tip, head_tip) else {
        return Ok(MergeStatus::BranchMissing);
    };
    // A pair of commits always merges the same way, so this is cacheable.
    let result = match state.mergeability.get(&(base, head)) {
        Some(hit) => hit,
        None => {
            let result = with_git(state, repo, move |g| {
                if g.is_ancestor(head, base)? {
                    return Ok(None);
                }
                Ok(Some(g.mergeability(base, head)?))
            })
            .await?;
            state.mergeability.insert((base, head), result.clone());
            result
        }
    };
    Ok(match result {
        None => MergeStatus::AlreadyMerged,
        Some(Mergeability::Clean(_)) => MergeStatus::Ready,
        Some(Mergeability::Conflicts(files)) => MergeStatus::Conflicts(files),
    })
}

#[derive(Template)]
#[template(path = "pulls/commits.html")]
struct CommitsTab {
    page: Page,
    repo: RepoView,
    pr: PullHeader,
    conv: Conversation,
    commits: Vec<CommitView>,
}

pub async fn commits(
    State(state): State<AppState>,
    Viewer(viewer): Viewer,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> AppResult {
    let (repo, access) = load(&state, viewer.as_ref(), &owner, &name).await?;
    let l = load_pull(&state, &repo, number).await?;
    let (pr, changes) = header(&state, &repo, &l, "commits").await?;
    let conv = conversation(&state, &repo, access, &l.issue, viewer.as_ref()).await?;
    html(CommitsTab {
        repo: RepoView::new(&state, &repo, access, "pulls", "").await?,
        page: Page::new(&state, viewer, format!("Commits · Pull request #{number}")),
        pr,
        conv,
        commits: changes.map(|c| c.commits).unwrap_or_default(),
    })
}

#[derive(Template)]
#[template(path = "pulls/files.html")]
struct FilesTab {
    page: Page,
    repo: RepoView,
    pr: PullHeader,
    conv: Conversation,
    files: Vec<FileView>,
    additions: usize,
    deletions: usize,
    truncated: bool,
}

pub async fn files(
    State(state): State<AppState>,
    Viewer(viewer): Viewer,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> AppResult {
    let (repo, access) = load(&state, viewer.as_ref(), &owner, &name).await?;
    let l = load_pull(&state, &repo, number).await?;
    let (pr, changes) = header(&state, &repo, &l, "files").await?;
    let conv = conversation(&state, &repo, access, &l.issue, viewer.as_ref()).await?;
    let c = changes.unwrap_or(Changes {
        commits: Vec::new(),
        files: Vec::new(),
        additions: 0,
        deletions: 0,
        truncated: false,
    });
    html(FilesTab {
        repo: RepoView::new(&state, &repo, access, "pulls", "").await?,
        page: Page::new(&state, viewer, format!("Files changed · Pull request #{number}")),
        pr,
        conv,
        files: c.files,
        additions: c.additions,
        deletions: c.deletions,
        truncated: c.truncated,
    })
}

// ---------------------------------------------------------------------------
// Merging
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct MergeForm {
    style: String,
    delete_branch: Option<String>,
}

pub async fn merge(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Form(f): Form<MergeForm>,
) -> AppResult {
    let (repo, access) = load(&state, Some(&user), &owner, &name).await?;
    if access < Access::Write {
        return Err(AppError::Forbidden);
    }
    let l = load_pull(&state, &repo, number).await?;
    if !l.issue.is_open() {
        return Err(AppError::BadRequest("This pull request is not open.".into()));
    }
    let style = if f.style == "squash" { MergeStyle::Squash } else { MergeStyle::Merge };
    let (Some(base), Some(head)) = (l.base_tip, l.head_tip) else {
        return show_page(&state, Some(user), &repo, access, number, Some("A branch is missing.".into())).await;
    };
    let commits: Vec<CommitSummary> =
        with_git(&state, &repo, move |g| Ok(g.commits_between(base, head, MAX_COMMITS)?)).await?;
    let message = match style {
        MergeStyle::Merge => format!(
            "Merge pull request #{number} from {}\n\n{}",
            l.pull.head_ref, l.issue.title
        ),
        MergeStyle::Squash => {
            let list: Vec<String> = commits.iter().map(|c| format!("* {}", c.summary)).collect();
            format!("{} (#{number})\n\n{}", l.issue.title, list.join("\n"))
        }
    };
    let (base_ref, head_ref) = (l.pull.base_ref.clone(), l.pull.head_ref.clone());
    let (name_, email) = (user.username.clone(), user.email.clone());
    let delete_branch = f.delete_branch.is_some();
    let merged = with_git(&state, &repo, move |g| {
        let merge_base = g.merge_base(base, head)?;
        let who = Identity { name: &name_, email: &email };
        let commit = g.merge(&base_ref, base, head, style, &message, &who)?;
        g.update_ref(&pull_ref(number), Some(head), None)?;
        if delete_branch {
            // Only if nobody pushed to it meanwhile.
            let _ = g.update_ref(&format!("refs/heads/{head_ref}"), None, Some(head));
        }
        Ok((merge_base, commit))
    })
    .await;
    let (merge_base, commit) = match merged {
        Ok(v) => v,
        Err(AppError::Internal(e)) => {
            tracing::info!("merge of {}/{}#{number} failed: {e:#}", repo.owner, repo.name);
            let msg = "Merging failed: the branches changed or conflict. Reload and try again.".to_owned();
            return show_page(&state, Some(user), &repo, access, number, Some(msg)).await;
        }
        Err(e) => return Err(e),
    };
    state.core.mark_merged(&l.issue, Some(user.id), merge_base, head, Some(commit)).await?;
    state.core.after_push(repo.id, Some(user.id)).await?;
    Ok(Redirect::to(&issue_url(&repo, &l.issue)).into_response())
}
