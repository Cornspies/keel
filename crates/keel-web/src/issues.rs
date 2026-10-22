//! Issue pages. The list, the timeline and the comment handlers are shared
//! with pull requests, which are issues with extra data.

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Redirect};
use keel_core::{Access, Issue, Repo, User};
use serde::Deserialize;

use crate::error::{AppResult, html};
use crate::render::{LinkBase, datetime, markdown_with_refs, relative_time};
use crate::repo::{RepoView, encode_path, load, with_git};
use crate::session::{SignedIn, Viewer};
use crate::{AppError, AppState, Page};

const PER_PAGE: i64 = 50;

// ---------------------------------------------------------------------------
// Shared view models
// ---------------------------------------------------------------------------

pub(crate) struct IssueRow {
    pub number: i64,
    pub title: String,
    pub url: String,
    pub state: String,
    pub author: String,
    pub opened: String,
    pub opened_full: String,
    pub comments: i64,
}

impl IssueRow {
    fn new(repo: &Repo, i: &Issue) -> Self {
        Self {
            url: issue_url(repo, i),
            number: i.number,
            title: i.title.clone(),
            state: i.state.clone(),
            author: author_name(&i.author),
            opened: relative_time(i.created_at),
            opened_full: datetime(i.created_at),
            comments: i.comment_count,
        }
    }
}

fn author_name(author: &Option<String>) -> String {
    author.clone().unwrap_or_else(|| "ghost".into())
}

pub(crate) fn issue_url(repo: &Repo, issue: &Issue) -> String {
    let kind = if issue.is_pull { "pull" } else { "issues" };
    format!("/{}/{}/{kind}/{}", repo.owner, repo.name, issue.number)
}

/// One entry of the conversation: the description, a comment or an event.
pub(crate) struct TimelineItem {
    pub id: i64,
    /// `description`, `comment`, `closed`, `reopened` or `merged`.
    pub kind: String,
    pub author: String,
    pub when: String,
    pub when_full: String,
    pub body_html: String,
    pub edited: bool,
    pub can_edit: bool,
    pub edit_url: String,
    pub delete_url: Option<String>,
}

/// Everything the conversation partial (`issues/conversation.html`) needs.
pub(crate) struct Conversation {
    pub number: i64,
    pub title: String,
    pub state: String,
    pub author: String,
    pub opened: String,
    pub comment_count: i64,
    pub timeline: Vec<TimelineItem>,
    /// Signed in with read access.
    pub can_comment: bool,
    /// Author or a writer.
    pub can_close: bool,
    pub can_edit: bool,
    pub is_pull: bool,
    pub base_url: String,
}

/// Builds the conversation of an issue or pull request for `viewer`.
pub(crate) async fn conversation(
    state: &AppState,
    repo: &Repo,
    access: Access,
    issue: &Issue,
    viewer: Option<&User>,
) -> AppResult<Conversation> {
    let base_url = format!("/{}/{}/issues/{}", repo.owner, repo.name, issue.number);
    let repo_url = format!("/{}/{}", repo.owner, repo.name);
    let branch = with_git(state, repo, |g| Ok(g.default_branch()?.unwrap_or_else(|| "main".into()))).await?;
    let blob = format!("{repo_url}/blob/{}", encode_path(&branch));
    let raw = format!("{repo_url}/raw/{}", encode_path(&branch));
    let links = LinkBase { blob: &blob, raw: &raw, dir: "" };
    let render = |body: &str| markdown_with_refs(body, &links, &repo_url);
    let writer = access >= Access::Write;
    let is_author = viewer.is_some_and(|v| issue.is_author(v));

    let mut timeline = vec![TimelineItem {
        id: 0,
        kind: "description".into(),
        author: author_name(&issue.author),
        when: relative_time(issue.created_at),
        when_full: datetime(issue.created_at),
        body_html: if issue.body.trim().is_empty() {
            "<p class=\"muted\"><em>No description provided.</em></p>".into()
        } else {
            render(&issue.body)
        },
        edited: false,
        can_edit: is_author || writer,
        edit_url: format!("{base_url}/edit"),
        delete_url: None,
    }];
    for c in state.core.comments(issue.id).await? {
        let mine = viewer.is_some_and(|v| c.author_id == Some(v.id));
        timeline.push(TimelineItem {
            id: c.id,
            author: author_name(&c.author),
            when: relative_time(c.created_at),
            when_full: datetime(c.created_at),
            body_html: if c.kind == "comment" { render(&c.body) } else { String::new() },
            edited: c.updated_at > c.created_at,
            can_edit: c.kind == "comment" && (mine || writer),
            edit_url: format!("{base_url}/comments/{}/edit", c.id),
            delete_url: (c.kind == "comment" && (mine || writer))
                .then(|| format!("{base_url}/comments/{}/delete", c.id)),
            kind: c.kind,
        });
    }
    Ok(Conversation {
        number: issue.number,
        title: issue.title.clone(),
        state: issue.state.clone(),
        author: author_name(&issue.author),
        opened: relative_time(issue.created_at),
        comment_count: issue.comment_count,
        timeline,
        can_comment: viewer.is_some(),
        can_close: is_author || writer,
        can_edit: is_author || writer,
        is_pull: issue.is_pull,
        base_url,
    })
}

/// Loads an issue or pull request by number (either kind).
pub(crate) async fn load_issue(state: &AppState, repo: &Repo, number: i64) -> AppResult<Issue> {
    state.core.issue(repo.id, number).await?.ok_or(AppError::NotFound)
}

// ---------------------------------------------------------------------------
// List
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "issues/list.html")]
struct ListPage {
    page: Page,
    repo: RepoView,
    pulls: bool,
    show_open: bool,
    open_count: i64,
    closed_count: i64,
    rows: Vec<IssueRow>,
    newer_url: Option<String>,
    older_url: Option<String>,
    signed_in: bool,
}

#[derive(Deserialize)]
pub struct ListQuery {
    state: Option<String>,
    page: Option<i64>,
}

pub async fn list(
    State(state): State<AppState>,
    Viewer(viewer): Viewer,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<ListQuery>,
) -> AppResult {
    list_page(state, viewer, owner, name, q, false).await
}

pub(crate) async fn list_page(
    state: AppState,
    viewer: Option<User>,
    owner: String,
    name: String,
    q: ListQuery,
    pulls: bool,
) -> AppResult {
    let (repo, access) = load(&state, viewer.as_ref(), &owner, &name).await?;
    let show_open = q.state.as_deref() != Some("closed");
    let page_no = q.page.unwrap_or(1).max(1);
    let mut issues = state
        .core
        .issues(repo.id, pulls, show_open, (page_no - 1) * PER_PAGE, PER_PAGE + 1)
        .await?;
    let has_more = issues.len() as i64 > PER_PAGE;
    issues.truncate(PER_PAGE as usize);
    let counts = state.core.issue_counts(repo.id).await?;
    let (open_count, closed_count) = if pulls {
        (counts.open_pulls, counts.closed_pulls)
    } else {
        (counts.open_issues, counts.closed_issues)
    };
    let kind = if pulls { "pulls" } else { "issues" };
    let here = format!(
        "/{}/{}/{kind}?state={}",
        repo.owner,
        repo.name,
        if show_open { "open" } else { "closed" }
    );
    let default_branch = with_git(&state, &repo, |g| Ok(g.default_branch()?.unwrap_or_default())).await?;
    html(ListPage {
        repo: RepoView::new(&state, &repo, access, kind, &default_branch).await?,
        page: Page::new(
            &state,
            viewer.clone(),
            format!("{} · {}/{}", if pulls { "Pull requests" } else { "Issues" }, repo.owner, repo.name),
        ),
        pulls,
        show_open,
        open_count,
        closed_count,
        rows: issues.iter().map(|i| IssueRow::new(&repo, i)).collect(),
        newer_url: (page_no > 1).then(|| format!("{here}&page={}", page_no - 1)),
        older_url: has_more.then(|| format!("{here}&page={}", page_no + 1)),
        signed_in: viewer.is_some(),
    })
}

// ---------------------------------------------------------------------------
// New issue
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "issues/new.html")]
struct NewIssuePage {
    page: Page,
    repo: RepoView,
    error: Option<String>,
    title: String,
    body: String,
}

pub async fn new_form(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name)): Path<(String, String)>,
) -> AppResult {
    let (repo, access) = load(&state, Some(&user), &owner, &name).await?;
    html(NewIssuePage {
        repo: RepoView::new(&state, &repo, access, "issues", "").await?,
        page: Page::new(&state, Some(user), format!("New issue · {}/{}", repo.owner, repo.name)),
        error: None,
        title: String::new(),
        body: String::new(),
    })
}

#[derive(Deserialize)]
pub struct IssueForm {
    title: String,
    #[serde(default)]
    body: String,
}

pub async fn create(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name)): Path<(String, String)>,
    Form(f): Form<IssueForm>,
) -> AppResult {
    let (repo, access) = load(&state, Some(&user), &owner, &name).await?;
    match state.core.create_issue(repo.id, &user, &f.title, &f.body).await {
        Ok(issue) => Ok(Redirect::to(&issue_url(&repo, &issue)).into_response()),
        Err(keel_core::Error::Invalid(msg)) => html(NewIssuePage {
            repo: RepoView::new(&state, &repo, access, "issues", "").await?,
            page: Page::new(&state, Some(user), format!("New issue · {}/{}", repo.owner, repo.name)),
            error: Some(msg),
            title: f.title,
            body: f.body,
        }),
        Err(e) => Err(e.into()),
    }
}

// ---------------------------------------------------------------------------
// Issue page
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "issues/show.html")]
struct IssuePage {
    page: Page,
    repo: RepoView,
    conv: Conversation,
    error: Option<String>,
}

pub async fn show(
    State(state): State<AppState>,
    Viewer(viewer): Viewer,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> AppResult {
    let (repo, access) = load(&state, viewer.as_ref(), &owner, &name).await?;
    let issue = load_issue(&state, &repo, number).await?;
    if issue.is_pull {
        return Ok(Redirect::to(&issue_url(&repo, &issue)).into_response());
    }
    show_page(&state, viewer, &repo, access, &issue, None).await
}

async fn show_page(
    state: &AppState,
    viewer: Option<User>,
    repo: &Repo,
    access: Access,
    issue: &Issue,
    error: Option<String>,
) -> AppResult {
    let conv = conversation(state, repo, access, issue, viewer.as_ref()).await?;
    html(IssuePage {
        repo: RepoView::new(state, repo, access, "issues", "").await?,
        page: Page::new(state, viewer, format!("{} · Issue #{} · {}/{}", issue.title, issue.number, repo.owner, repo.name)),
        conv,
        error,
    })
}

// ---------------------------------------------------------------------------
// Comments, closing and editing (issues and pull requests)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct CommentForm {
    #[serde(default)]
    body: String,
    /// `comment`, `close` or `reopen`.
    #[serde(default)]
    action: String,
}

pub async fn comment(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Form(f): Form<CommentForm>,
) -> AppResult {
    let (repo, access) = load(&state, Some(&user), &owner, &name).await?;
    let issue = load_issue(&state, &repo, number).await?;
    let can_close = issue.is_author(&user) || access >= Access::Write;
    let has_body = !f.body.trim().is_empty();
    let state_change = match f.action.as_str() {
        "close" => Some(false),
        "reopen" => Some(true),
        _ => None,
    };
    if state_change.is_some() && !can_close {
        return Err(AppError::Forbidden);
    }
    let anchor = if has_body {
        match state.core.add_comment(&issue, &user, &f.body).await {
            Ok(id) => format!("#comment-{id}"),
            Err(keel_core::Error::Invalid(msg)) if !issue.is_pull => {
                return show_page(&state, Some(user), &repo, access, &issue, Some(msg)).await;
            }
            Err(e) => return Err(e.into()),
        }
    } else if state_change.is_none() {
        return Ok(Redirect::to(&issue_url(&repo, &issue)).into_response());
    } else {
        String::new()
    };
    if let Some(open) = state_change {
        state.core.set_issue_open(&issue, &user, open).await?;
    }
    Ok(Redirect::to(&format!("{}{anchor}", issue_url(&repo, &issue))).into_response())
}

#[derive(Template)]
#[template(path = "issues/edit.html")]
struct EditPage {
    page: Page,
    repo: RepoView,
    heading: String,
    action: String,
    cancel_url: String,
    /// Editing the issue itself (with a title) rather than a comment.
    with_title: bool,
    title: String,
    body: String,
    error: Option<String>,
}

pub async fn edit_form(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> AppResult {
    let (repo, access) = load(&state, Some(&user), &owner, &name).await?;
    let issue = load_issue(&state, &repo, number).await?;
    if !(issue.is_author(&user) || access >= Access::Write) {
        return Err(AppError::Forbidden);
    }
    let tab = if issue.is_pull { "pulls" } else { "issues" };
    html(EditPage {
        repo: RepoView::new(&state, &repo, access, tab, "").await?,
        page: Page::new(&state, Some(user), format!("Edit #{}", issue.number)),
        heading: format!("Edit #{}", issue.number),
        action: format!("/{}/{}/issues/{}/edit", repo.owner, repo.name, issue.number),
        cancel_url: issue_url(&repo, &issue),
        with_title: true,
        title: issue.title,
        body: issue.body,
        error: None,
    })
}

pub async fn edit(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Form(f): Form<IssueForm>,
) -> AppResult {
    let (repo, access) = load(&state, Some(&user), &owner, &name).await?;
    let issue = load_issue(&state, &repo, number).await?;
    if !(issue.is_author(&user) || access >= Access::Write) {
        return Err(AppError::Forbidden);
    }
    match state.core.edit_issue(&issue, &f.title, &f.body).await {
        Ok(()) => Ok(Redirect::to(&issue_url(&repo, &issue)).into_response()),
        Err(keel_core::Error::Invalid(msg)) => html(EditPage {
            repo: RepoView::new(&state, &repo, access, "issues", "").await?,
            page: Page::new(&state, Some(user), format!("Edit #{}", issue.number)),
            heading: format!("Edit #{}", issue.number),
            action: format!("/{}/{}/issues/{}/edit", repo.owner, repo.name, issue.number),
            cancel_url: issue_url(&repo, &issue),
            with_title: true,
            title: f.title,
            body: f.body,
            error: Some(msg),
        }),
        Err(e) => Err(e.into()),
    }
}

/// Loads a comment the user may edit or delete.
async fn own_comment(
    state: &AppState,
    user: &User,
    owner: &str,
    name: &str,
    number: i64,
    comment_id: i64,
) -> AppResult<(Repo, Access, Issue, keel_core::Comment)> {
    let (repo, access) = load(state, Some(user), owner, name).await?;
    let issue = load_issue(state, &repo, number).await?;
    let comment = state
        .core
        .comment(issue.id, comment_id)
        .await?
        .filter(|c| c.kind == "comment")
        .ok_or(AppError::NotFound)?;
    if comment.author_id != Some(user.id) && access < Access::Write {
        return Err(AppError::Forbidden);
    }
    Ok((repo, access, issue, comment))
}

pub async fn edit_comment_form(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, number, id)): Path<(String, String, i64, i64)>,
) -> AppResult {
    let (repo, access, issue, comment) = own_comment(&state, &user, &owner, &name, number, id).await?;
    html(EditPage {
        repo: RepoView::new(&state, &repo, access, if issue.is_pull { "pulls" } else { "issues" }, "").await?,
        page: Page::new(&state, Some(user), "Edit comment"),
        heading: "Edit comment".into(),
        action: format!("/{}/{}/issues/{}/comments/{id}/edit", repo.owner, repo.name, number),
        cancel_url: format!("{}#comment-{id}", issue_url(&repo, &issue)),
        with_title: false,
        title: String::new(),
        body: comment.body,
        error: None,
    })
}

#[derive(Deserialize)]
pub struct CommentEditForm {
    body: String,
}

pub async fn edit_comment(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, number, id)): Path<(String, String, i64, i64)>,
    Form(f): Form<CommentEditForm>,
) -> AppResult {
    let (repo, access, issue, comment) = own_comment(&state, &user, &owner, &name, number, id).await?;
    match state.core.edit_comment(&comment, &f.body).await {
        Ok(()) => Ok(Redirect::to(&format!("{}#comment-{id}", issue_url(&repo, &issue))).into_response()),
        Err(keel_core::Error::Invalid(msg)) => html(EditPage {
            repo: RepoView::new(&state, &repo, access, "issues", "").await?,
            page: Page::new(&state, Some(user), "Edit comment"),
            heading: "Edit comment".into(),
            action: format!("/{}/{}/issues/{}/comments/{id}/edit", repo.owner, repo.name, number),
            cancel_url: issue_url(&repo, &issue),
            with_title: false,
            title: String::new(),
            body: f.body,
            error: Some(msg),
        }),
        Err(e) => Err(e.into()),
    }
}

pub async fn delete_comment(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, number, id)): Path<(String, String, i64, i64)>,
) -> AppResult {
    let (repo, _, issue, comment) = own_comment(&state, &user, &owner, &name, number, id).await?;
    state.core.delete_comment(&comment).await?;
    Ok(Redirect::to(&issue_url(&repo, &issue)).into_response())
}
