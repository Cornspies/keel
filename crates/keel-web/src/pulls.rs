//! Pull request pages: comparing branches, opening, reviewing and merging.

use std::collections::HashSet;

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Redirect};
use keel_core::{Access, Comment, InlineAnchor, Issue, PullRequest, Repo, User, Verdict, pull_ref};
use keel_git::{CommitSummary, DiffLineKind, Identity, MergeStyle, Mergeability, Oid};
use serde::Deserialize;

use crate::error::{AppResult, html};
use crate::issues::{Conversation, ListQuery, Renderer, conversation, issue_url, list_page, load_issue};
use crate::render::{datetime, relative_time};
use crate::repo::{
    CommitView, DIFF_MAX_FILES, DIFF_MAX_LINES, FileView, RepoView, encode_path, file_views, load, with_git,
    with_git_pair,
};
use crate::session::{SignedIn, Viewer};
use crate::{AppError, AppState, Page};

const MAX_COMMITS: usize = 250;
/// Lines of diff kept with an inline comment for showing it in the
/// conversation.
const HUNK_CONTEXT: usize = 4;

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
/// `head` may be a commit of `head_repo` (a fork) that `repo` doesn't have.
async fn changes(state: &AppState, repo: &Repo, head_repo: &Repo, base: Oid, head: Oid) -> AppResult<Changes> {
    let url = format!("/{}/{}", head_repo.owner, head_repo.name);
    let (commits, diff) = with_git_pair(state, repo, head_repo, move |g| {
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
// Branches across forks
// ---------------------------------------------------------------------------

fn full_name(r: &Repo) -> String {
    format!("{}/{}", r.owner, r.name)
}

/// How compare URLs of `base` refer to `branch` of `head`: `branch`, or
/// `owner/name:branch` in another repository (`:` can't occur in branch
/// names).
fn head_spec(base: &Repo, head: &Repo, branch: &str) -> String {
    if base.id == head.id {
        branch.to_owned()
    } else {
        format!("{}:{branch}", full_name(head))
    }
}

/// The repositories pull requests into `base` can come from, as visible to
/// `viewer`: `base` itself, its parent and its forks.
async fn related_repos(state: &AppState, base: &Repo, viewer: Option<&User>) -> AppResult<Vec<Repo>> {
    let mut out = vec![base.clone()];
    if let Some(parent) = base.forked_from
        && let Some(parent) = state.core.repo_by_id(parent).await?
        && state.core.access(&parent, viewer).await? >= Access::Read
    {
        out.push(parent);
    }
    out.extend(state.core.forks_of(base.id, viewer).await?);
    Ok(out)
}

/// Parses a head spec (see [`head_spec`]); `owner:branch` picks that
/// owner's repository among `related`.
fn resolve_head(related: &[Repo], spec: &str) -> Option<(Repo, String)> {
    let Some((repo, branch)) = spec.split_once(':') else {
        return Some((related[0].clone(), spec.to_owned()));
    };
    let found = match repo.split_once('/') {
        Some((owner, name)) => related
            .iter()
            .find(|r| r.owner.eq_ignore_ascii_case(owner) && r.name.eq_ignore_ascii_case(name)),
        None => related.iter().find(|r| r.owner.eq_ignore_ascii_case(repo)),
    };
    found.map(|r| (r.clone(), branch.to_owned()))
}

// ---------------------------------------------------------------------------
// Compare and create
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "pulls/compare.html")]
struct ComparePage {
    page: Page,
    repo: RepoView,
    /// Repositories that can be the base (this one or its parent) and the
    /// head (also forks), by full name.
    base_repos: Vec<String>,
    head_repos: Vec<String>,
    base_repo: String,
    head_repo: String,
    branches: Vec<String>,
    head_branches: Vec<String>,
    base: String,
    head: String,
    /// What the create form posts as the head.
    head_label: String,
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
    inline: Option<InlineCtx>,
}

#[derive(Deserialize)]
pub struct CompareQuery {
    base_repo: Option<String>,
    base: Option<String>,
    head_repo: Option<String>,
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
    let related = related_repos(&state, &repo, viewer.as_ref()).await?;
    // Switching the base repository (to this one's parent) means comparing
    // over there.
    if let Some(base_repo) = &q.base_repo
        && let Some(other) = related.iter().find(|r| full_name(r) == *base_repo && r.id != repo.id)
        && repo.forked_from == Some(other.id)
    {
        let mut query = form_urlencoded::Serializer::new(String::new());
        query.append_pair("head_repo", q.head_repo.as_deref().unwrap_or(&full_name(&repo)));
        if let Some(h) = &q.head {
            query.append_pair("head", h);
        }
        return Ok(Redirect::to(&format!("/{base_repo}/compare?{}", query.finish())).into_response());
    }
    let head_repo = q
        .head_repo
        .as_deref()
        .and_then(|h| related.iter().find(|r| full_name(r) == h))
        .unwrap_or(&repo)
        .clone();
    let (default, branches) = with_git(&state, &repo, |g| {
        Ok((g.default_branch()?.unwrap_or_default(), g.branches()?))
    })
    .await?;
    let base = q.base.filter(|b| branches.iter().any(|x| x.name == *b)).unwrap_or_else(|| default.clone());
    let head = match q.head {
        Some(h) => h,
        // Default head: the most recently updated other branch here, or
        // the default branch of a fork.
        None if head_repo.id == repo.id => branches
            .iter()
            .find(|b| b.name != base)
            .map(|b| b.name.clone())
            .unwrap_or_else(|| base.clone()),
        None => with_git(&state, &head_repo, |g| Ok(g.default_branch()?.unwrap_or_default())).await?,
    };
    Ok(Redirect::to(&format!(
        "/{}/{}/compare/{}...{}",
        repo.owner,
        repo.name,
        encode_path(&base),
        encode_path(&head_spec(&repo, &head_repo, &head))
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
    head_label: String,
    title: String,
    body: String,
    error: Option<String>,
) -> AppResult {
    let related = related_repos(state, repo, viewer.as_ref()).await?;
    let (head_repo, head) = resolve_head(&related, &head_label).ok_or(AppError::NotFound)?;
    let b = base.clone();
    let (default, branches, base_tip) = with_git(state, repo, move |g| {
        Ok((
            g.default_branch()?.unwrap_or_default(),
            g.branches()?.into_iter().map(|r| r.name).collect::<Vec<_>>(),
            g.branch_tip(&b),
        ))
    })
    .await?;
    let h = head.clone();
    let (head_branches, head_tip) = with_git(state, &head_repo, move |g| {
        Ok((g.branches()?.into_iter().map(|r| r.name).collect::<Vec<_>>(), g.branch_tip(&h)))
    })
    .await?;

    let mut notice = None;
    let mut changes_found = None;
    match (base_tip, head_tip) {
        _ if base == head && repo.id == head_repo.id => {
            notice = Some("Choose two different branches to compare.".to_owned())
        }
        (Some(base_tip), Some(head_tip)) => {
            let c = changes(state, repo, &head_repo, base_tip, head_tip).await?;
            if c.commits.is_empty() {
                notice = Some(format!("{base} is up to date with all commits from {head_label}."));
            } else {
                changes_found = Some(c);
            }
        }
        _ => notice = Some("One of these branches doesn't exist.".to_owned()),
    }

    let existing = state
        .core
        .open_pull_between(repo.id, &base, head_repo.id, &head)
        .await?
        .map(|n| (n, format!("/{}/{}/pull/{n}", repo.owner, repo.name)));
    let title = match (&changes_found, title.is_empty()) {
        (Some(c), true) if c.commits.len() == 1 => c.commits[0].summary.clone(),
        (_, true) => head.replace(['-', '_', '/'], " "),
        _ => title,
    };
    let (files, additions, deletions, truncated) = match &mut changes_found {
        Some(c) => (std::mem::take(&mut c.files), c.additions, c.deletions, c.truncated),
        None => (Vec::new(), 0, 0, false),
    };
    let mut base_repos = vec![full_name(repo)];
    base_repos.extend(related.iter().filter(|r| repo.forked_from == Some(r.id)).map(full_name));
    html(ComparePage {
        repo: RepoView::new(state, repo, access, "pulls", &default).await?,
        can_open: viewer.is_some() && existing.is_none(),
        page: Page::new(state, viewer, format!("Compare {base}...{head_label} · {}/{}", repo.owner, repo.name)),
        base_repos,
        head_repos: related.iter().map(full_name).collect(),
        base_repo: full_name(repo),
        head_repo: full_name(&head_repo),
        branches,
        head_branches,
        base,
        head,
        head_label,
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
        inline: None,
    })
}

#[derive(Deserialize)]
pub struct CreateForm {
    base: String,
    /// A head spec, see [`head_spec`].
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
    let related = related_repos(&state, &repo, Some(&user)).await?;
    let (head_repo, head) = resolve_head(&related, &f.head).ok_or(AppError::NotFound)?;
    match state.core.create_pull(&repo, &user, &f.title, &f.body, &f.base, &head_repo, &head).await {
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
    /// `topic`, or `owner:topic` for a branch in a fork.
    pub head_label: String,
    /// The head branch in its repository, if that still exists.
    pub head_url: Option<String>,
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
    let stored_base = pull.base_commit.as_deref().and_then(|c| Oid::from_str(c).ok());
    let stored_head = pull.head_commit.as_deref().and_then(|c| Oid::from_str(c).ok());
    let (base_tip, head_tip) = if issue.is_open() {
        state.core.pull_tips(&issue, &pull).await?
    } else {
        (None, None)
    };
    let open = issue.is_open();
    let (base, head) = with_git(state, repo, move |g| {
        let pinned = g.ref_tip(&pull_ref(number));
        // Open: compare the live branches. Merged/closed: what was recorded.
        let head = if open { head_tip.or(pinned) } else { stored_head.or(pinned) };
        let base = match (open, head, base_tip) {
            (true, Some(h), Some(b)) => g.merge_base(b, h)?,
            _ => stored_base,
        };
        Ok((base, head))
    })
    .await?;
    Ok(Loaded { issue, pull, base, head, base_tip, head_tip })
}

async fn header(state: &AppState, repo: &Repo, l: &Loaded, tab: &'static str) -> AppResult<(PullHeader, Option<Changes>)> {
    let changes = match (l.base, l.head) {
        (Some(base), Some(head)) => Some(changes(state, repo, repo, base, head).await?),
        _ => None,
    };
    let url = format!("/{}/{}", repo.owner, repo.name);
    Ok((
        PullHeader {
            base_ref: l.pull.base_ref.clone(),
            head_label: l.pull.head_label(&l.issue),
            head_url: l
                .pull
                .head_repo
                .as_ref()
                .map(|r| format!("/{r}/tree/{}", encode_path(&l.pull.head_ref))),
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
    /// May delete the head branch after merging.
    can_delete_branch: bool,
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
    let (pr, changes) = header(state, repo, &l, "conversation").await?;
    let merge = if l.issue.is_open() {
        Some(merge_status(state, repo, l.base_tip, l.head_tip).await?)
    } else {
        None
    };
    let comments = state.core.comments(l.issue.id).await?;
    let placed = placed_threads(changes.as_ref().map_or(&[][..], |c| &c.files), &comments);
    let conv = conversation(state, repo, access, &l.issue, viewer.as_ref(), Some(&placed)).await?;
    let can_delete_branch = can_delete_branch(state, repo, &l, viewer.as_ref()).await?;
    html(PullPage {
        repo: RepoView::new(state, repo, access, "pulls", "").await?,
        page: Page::new(
            state,
            viewer,
            format!("{} · Pull request #{} · {}/{}", l.issue.title, number, repo.owner, repo.name),
        ),
        can_merge: access >= Access::Write,
        can_delete_branch,
        pr,
        conv,
        merge,
        error,
    })
}

/// Whether `user` may delete the head branch: they need write access to
/// the repository it is in.
async fn can_delete_branch(state: &AppState, repo: &Repo, l: &Loaded, user: Option<&User>) -> AppResult<bool> {
    let head_repo = match l.pull.head_repo_id {
        Some(id) if id == repo.id => repo.clone(),
        Some(id) => match state.core.repo_by_id(id).await? {
            Some(r) => r,
            None => return Ok(false),
        },
        None => return Ok(false),
    };
    Ok(state.core.access(&head_repo, user).await? >= Access::Write)
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
    let conv = conversation(&state, &repo, access, &l.issue, viewer.as_ref(), None).await?;
    html(CommitsTab {
        repo: RepoView::new(&state, &repo, access, "pulls", "").await?,
        page: Page::new(&state, viewer, format!("Commits · Pull request #{number}")),
        pr,
        conv,
        commits: changes.map(|c| c.commits).unwrap_or_default(),
    })
}

// ---------------------------------------------------------------------------
// Files changed and inline comments
// ---------------------------------------------------------------------------

/// What the diff partial needs to offer inline comments.
pub(crate) struct InlineCtx {
    pub post_url: String,
    /// The head commit the diff shows; comments are made against it.
    pub head: String,
    /// Signed in, so lines get a comment button.
    pub can_comment: bool,
    /// Row to show a new-comment form below (without JavaScript).
    pub form_at: String,
}

pub(crate) struct InlineView {
    pub id: i64,
    pub author: String,
    pub when: String,
    pub when_full: String,
    pub body_html: String,
    pub edited: bool,
    pub edit_url: Option<String>,
    pub delete_url: Option<String>,
}

/// An inline comment and its replies.
pub(crate) struct ThreadView {
    /// The first comment's id.
    pub id: i64,
    pub path: String,
    pub side: String,
    pub line: i64,
    blob: String,
    /// The diff lines the comment was made on: (row class, text).
    pub hunk: Vec<(&'static str, String)>,
    pub comments: Vec<InlineView>,
    pub reply_url: String,
    pub can_reply: bool,
    /// No longer attaches to the current diff.
    pub outdated: bool,
    /// Shown inside the diff (rather than in the conversation, with its
    /// diff context).
    pub in_diff: bool,
}

/// Groups the inline comments of a pull request into threads, oldest first.
pub(crate) fn inline_threads(
    repo: &Repo,
    issue: &Issue,
    comments: &[Comment],
    viewer: Option<&User>,
    access: Access,
    render: &dyn Fn(&str) -> String,
) -> Vec<ThreadView> {
    let issue_base = format!("/{}/{}/issues/{}", repo.owner, repo.name, issue.number);
    let view = |c: &Comment| {
        let mine = viewer.is_some_and(|v| c.author_id == Some(v.id));
        let editable = mine || access >= Access::Write;
        InlineView {
            id: c.id,
            author: c.author.clone().unwrap_or_else(|| "ghost".into()),
            when: relative_time(c.created_at),
            when_full: datetime(c.created_at),
            body_html: render(&c.body),
            edited: c.updated_at > c.created_at,
            edit_url: editable.then(|| format!("{issue_base}/comments/{}/edit", c.id)),
            delete_url: editable.then(|| format!("{issue_base}/comments/{}/delete", c.id)),
        }
    };
    comments
        .iter()
        .filter(|c| c.kind == "inline" && c.reply_to.is_none())
        .map(|root| ThreadView {
            id: root.id,
            path: root.path.clone().unwrap_or_default(),
            side: root.side.clone().unwrap_or_default(),
            line: root.line.unwrap_or_default(),
            blob: root.blob_id.clone().unwrap_or_default(),
            hunk: root
                .hunk
                .as_deref()
                .unwrap_or_default()
                .lines()
                .map(|l| match l.as_bytes().first() {
                    Some(b'+') => ("add", l[1..].to_owned()),
                    Some(b'-') => ("del", l[1..].to_owned()),
                    Some(b'@') => ("hunk", l.to_owned()),
                    _ => ("ctx", l.get(1..).unwrap_or_default().to_owned()),
                })
                .collect(),
            comments: std::iter::once(root)
                .chain(comments.iter().filter(|c| c.reply_to == Some(root.id)))
                .map(view)
                .collect(),
            reply_url: format!("/{}/{}/pull/{}/comments/{}/reply", repo.owner, repo.name, issue.number, root.id),
            can_reply: viewer.is_some(),
            outdated: false,
            in_diff: false,
        })
        .collect()
}

/// Where in `files` an inline comment on `line` of `side` of `path` (whose
/// blob there was `blob`) shows: (file, hunk, line) indexes. Only if that
/// side of the file is unchanged, so the line still says the same thing.
fn place(files: &[FileView], path: &str, side: &str, line: i64, blob: &str) -> Option<(usize, usize, usize)> {
    let (fi, file) = files.iter().enumerate().find(|(_, f)| f.path == path)?;
    let current = if side == "old" { &file.old_blob } else { &file.new_blob };
    if current.as_deref() != Some(blob) {
        return None;
    }
    file.hunks.iter().enumerate().find_map(|(hi, h)| {
        let li = h.lines.iter().position(|l| l.side == side && i64::from(l.line) == line)?;
        Some((fi, hi, li))
    })
}

/// Ids of the inline threads that attach to the diff `files`.
fn placed_threads(files: &[FileView], comments: &[Comment]) -> HashSet<i64> {
    comments
        .iter()
        .filter(|c| c.kind == "inline" && c.reply_to.is_none())
        .filter(|c| {
            place(
                files,
                c.path.as_deref().unwrap_or_default(),
                c.side.as_deref().unwrap_or_default(),
                c.line.unwrap_or_default(),
                c.blob_id.as_deref().unwrap_or_default(),
            )
            .is_some()
        })
        .map(|c| c.id)
        .collect()
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
    inline: Option<InlineCtx>,
    /// Threads that no longer attach to the diff.
    outdated: usize,
    /// Signed in and not the author: may approve or request changes.
    can_review: bool,
    signed_in: bool,
}

#[derive(Deserialize)]
pub struct FilesQuery {
    /// Row anchor to open a comment form at.
    comment: Option<String>,
}

pub async fn files(
    State(state): State<AppState>,
    Viewer(viewer): Viewer,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Query(q): Query<FilesQuery>,
) -> AppResult {
    let (repo, access) = load(&state, viewer.as_ref(), &owner, &name).await?;
    let l = load_pull(&state, &repo, number).await?;
    let (pr, changes) = header(&state, &repo, &l, "files").await?;
    let conv = conversation(&state, &repo, access, &l.issue, viewer.as_ref(), None).await?;
    let mut c = changes.unwrap_or(Changes {
        commits: Vec::new(),
        files: Vec::new(),
        additions: 0,
        deletions: 0,
        truncated: false,
    });

    let comments = state.core.comments(l.issue.id).await?;
    let renderer = Renderer::new(&state, &repo).await?;
    let render = |body: &str| renderer.render(body);
    let mut outdated = 0;
    for mut t in inline_threads(&repo, &l.issue, &comments, viewer.as_ref(), access, &render) {
        match place(&c.files, &t.path, &t.side, t.line, &t.blob) {
            Some((fi, hi, li)) => {
                t.in_diff = true;
                c.files[fi].hunks[hi].lines[li].threads.push(t);
            }
            None => outdated += 1,
        }
    }
    let signed_in = viewer.is_some();
    html(FilesTab {
        repo: RepoView::new(&state, &repo, access, "pulls", "").await?,
        can_review: viewer.as_ref().is_some_and(|v| !l.issue.is_author(v)) && l.issue.is_open(),
        signed_in,
        page: Page::new(&state, viewer, format!("Files changed · Pull request #{number}")),
        inline: l.head.map(|head| InlineCtx {
            post_url: format!("{}/comments", pr.url),
            head: head.to_string(),
            can_comment: signed_in,
            form_at: q.comment.unwrap_or_default(),
        }),
        pr,
        conv,
        files: c.files,
        additions: c.additions,
        deletions: c.deletions,
        truncated: c.truncated,
        outdated,
    })
}

#[derive(Deserialize)]
pub struct InlineForm {
    path: String,
    side: String,
    line: u32,
    /// The head commit the commenter saw.
    head: String,
    body: String,
}

/// Adds a comment on a line of the diff.
pub async fn inline_comment(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Form(f): Form<InlineForm>,
) -> AppResult {
    let (repo, _) = load(&state, Some(&user), &owner, &name).await?;
    let l = load_pull(&state, &repo, number).await?;
    let (Some(head), base) = (l.head, l.base) else {
        return Err(AppError::NotFound);
    };
    if head.to_string() != f.head {
        return Err(AppError::BadRequest(
            "The pull request changed since you loaded it. Go back, reload and comment again.".into(),
        ));
    }
    let new_side = match f.side.as_str() {
        "new" => true,
        "old" => false,
        _ => return Err(AppError::BadRequest("Unknown side".into())),
    };
    let (path, line) = (f.path.clone(), f.line);
    let anchor = with_git(&state, &repo, move |g| {
        let Some(file) = g.diff_file(base, head, &path, DIFF_MAX_LINES)? else {
            return Ok(None);
        };
        let blob = if new_side { file.new_id } else { file.old_id };
        for hunk in &file.hunks {
            let at = hunk.lines.iter().position(|l| match (new_side, l.kind) {
                (true, DiffLineKind::Added | DiffLineKind::Context) => l.new_no == Some(line),
                (false, DiffLineKind::Removed) => l.old_no == Some(line),
                _ => false,
            });
            if let (Some(at), Some(blob)) = (at, blob) {
                let mut context = vec![hunk.header.clone()];
                context.extend(hunk.lines[at.saturating_sub(HUNK_CONTEXT - 1)..=at].iter().map(|l| {
                    let mark = match l.kind {
                        DiffLineKind::Added => '+',
                        DiffLineKind::Removed => '-',
                        _ => ' ',
                    };
                    format!("{mark}{}", l.content)
                }));
                return Ok(Some((blob, context.join("\n"))));
            }
        }
        Ok(None)
    })
    .await?;
    let Some((blob_id, hunk)) = anchor else {
        return Err(AppError::BadRequest("That line isn't part of this pull request's changes.".into()));
    };
    let anchor = InlineAnchor { path: f.path, new_side, line: i64::from(f.line), commit_id: head, blob_id, hunk };
    match state.core.add_inline_comment(&l.issue, &user, &anchor, &f.body).await {
        Ok(id) => Ok(Redirect::to(&format!("{}/files#comment-{id}", issue_url(&repo, &l.issue))).into_response()),
        Err(keel_core::Error::Invalid(msg)) => Err(AppError::BadRequest(msg)),
        Err(e) => Err(e.into()),
    }
}

#[derive(Deserialize)]
pub struct ReplyForm {
    body: String,
    /// `files` to return to the diff, otherwise the conversation.
    #[serde(default)]
    back: String,
}

pub async fn reply(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, number, id)): Path<(String, String, i64, i64)>,
    Form(f): Form<ReplyForm>,
) -> AppResult {
    let (repo, _) = load(&state, Some(&user), &owner, &name).await?;
    let issue = load_issue(&state, &repo, number).await?;
    let to = state.core.comment(issue.id, id).await?.ok_or(AppError::NotFound)?;
    match state.core.reply_inline(&issue, &to, &user, &f.body).await {
        Ok(id) => {
            let tab = if f.back == "files" { "/files" } else { "" };
            Ok(Redirect::to(&format!("{}{tab}#comment-{id}", issue_url(&repo, &issue))).into_response())
        }
        Err(keel_core::Error::Invalid(msg)) => Err(AppError::BadRequest(msg)),
        Err(e) => Err(e.into()),
    }
}

#[derive(Deserialize)]
pub struct ReviewForm {
    #[serde(default)]
    body: String,
    /// `comment`, `approve` or `request_changes`.
    verdict: String,
}

pub async fn review(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Form(f): Form<ReviewForm>,
) -> AppResult {
    let (repo, _) = load(&state, Some(&user), &owner, &name).await?;
    let issue = load_issue(&state, &repo, number).await?;
    let result = match Verdict::parse(&f.verdict) {
        Some(verdict) => state.core.add_review(&issue, &user, verdict, &f.body).await,
        None => state.core.add_comment(&issue, &user, &f.body).await,
    };
    match result {
        Ok(id) => Ok(Redirect::to(&format!("{}#comment-{id}", issue_url(&repo, &issue))).into_response()),
        Err(keel_core::Error::Invalid(msg)) => Err(AppError::BadRequest(msg)),
        Err(e) => Err(e.into()),
    }
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
    let head_label = l.pull.head_label(&l.issue);
    let message = match style {
        MergeStyle::Merge => format!("Merge pull request #{number} from {head_label}\n\n{}", l.issue.title),
        MergeStyle::Squash => {
            let list: Vec<String> = commits.iter().map(|c| format!("* {}", c.summary)).collect();
            format!("{} (#{number})\n\n{}", l.issue.title, list.join("\n"))
        }
    };
    let base_ref = l.pull.base_ref.clone();
    let (name_, email) = (user.username.clone(), user.email.clone());
    let merged = with_git(&state, &repo, move |g| {
        let merge_base = g.merge_base(base, head)?;
        let who = Identity { name: &name_, email: &email };
        let commit = g.merge(&base_ref, base, head, style, &message, &who)?;
        g.update_ref(&pull_ref(number), Some(head), None)?;
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

    if f.delete_branch.is_some()
        && can_delete_branch(&state, &repo, &l, Some(&user)).await?
        && let Some(head_repo) = state.core.repo_by_id(l.pull.head_repo_id.unwrap_or(-1)).await?
    {
        let branch = format!("refs/heads/{}", l.pull.head_ref);
        // Only if nobody pushed to it meanwhile.
        let deleted = with_git(&state, &head_repo, move |g| Ok(g.update_ref(&branch, None, Some(head)).is_ok())).await?;
        if deleted && head_repo.id != repo.id {
            state.core.after_push(head_repo.id, Some(user.id)).await?;
        }
    }
    Ok(Redirect::to(&issue_url(&repo, &l.issue)).into_response())
}
