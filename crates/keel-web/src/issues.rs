//! Issue pages. The list, the timeline and the comment handlers are shared
//! with pull requests, which are issues with extra data.

use std::collections::{HashMap, HashSet};

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Redirect};
use keel_core::{Access, Issue, IssueFilter, Label, Repo, User};
use serde::Deserialize;

use crate::error::{AppResult, html};
use crate::labels::LabelView;
use crate::milestones::milestone_url;
use crate::pulls::{ThreadView, inline_threads};
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
    pub labels: Vec<LabelView>,
    pub assignees: Vec<String>,
}

impl IssueRow {
    fn new(repo: &Repo, i: &Issue, labels: &mut HashMap<i64, Vec<Label>>, assignees: &mut HashMap<i64, Vec<String>>) -> Self {
        Self {
            labels: labels
                .remove(&i.id)
                .unwrap_or_default()
                .iter()
                .map(|l| LabelView::new(repo, l, i.is_pull))
                .collect(),
            assignees: assignees.remove(&i.id).unwrap_or_default(),
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

/// One entry of the conversation: the description, a comment, a review,
/// a thread of inline comments or an event.
pub(crate) struct TimelineItem {
    pub id: i64,
    /// `description` or a comment kind (see [`keel_core::Comment`]).
    pub kind: String,
    pub author: String,
    pub when: String,
    pub when_full: String,
    pub body_html: String,
    pub edited: bool,
    pub can_edit: bool,
    pub edit_url: String,
    pub delete_url: Option<String>,
    /// For `inline`: the thread this comment starts.
    pub thread: Option<ThreadView>,
    /// For `labeled`/`unlabeled`: the label (gray if since deleted).
    pub label: Option<LabelView>,
    /// For `assigned`/`unassigned`, `review_requested`/`review_request_removed`:
    /// who; for `milestoned`/`demilestoned`: the milestone.
    pub target: String,
    /// For milestone events: the milestone's issues.
    pub target_url: String,
}

/// Someone asked to review a pull request, or who reviewed it.
pub(crate) struct ReviewerView {
    pub name: String,
    /// `requested`, `approved` or `changes_requested`.
    pub state: &'static str,
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
    pub labels: Vec<LabelView>,
    pub assignees: Vec<String>,
    /// May set labels and assignees (write access).
    pub can_triage: bool,
    pub label_choices: Vec<(LabelView, bool)>,
    pub assignee_choices: Vec<(i64, String, bool)>,
    /// Whether the viewer gets notified, and why (signed in only).
    pub subscription: Option<keel_core::Subscription>,
    /// Pull requests: each reviewer's latest verdict.
    pub reviews: Vec<(String, &'static str)>,
    /// Pull requests: requested reviewers and those who reviewed.
    pub reviewers: Vec<ReviewerView>,
    /// May ask people to review (writers and the author).
    pub can_request_review: bool,
    pub reviewer_choices: Vec<(i64, String, bool)>,
    /// The milestone: (title, url).
    pub milestone: Option<(String, String)>,
    pub milestone_choices: Vec<(i64, String, bool)>,
}

/// Renders comments: Markdown with `#123` and `@name` links, and relative
/// links resolved against the default branch.
pub(crate) struct Renderer {
    blob: String,
    raw: String,
    repo_url: String,
}

impl Renderer {
    pub(crate) async fn new(state: &AppState, repo: &Repo) -> AppResult<Self> {
        let repo_url = format!("/{}/{}", repo.owner, repo.name);
        let branch = with_git(state, repo, |g| Ok(g.default_branch()?.unwrap_or_else(|| "main".into()))).await?;
        Ok(Self {
            blob: format!("{repo_url}/blob/{}", encode_path(&branch)),
            raw: format!("{repo_url}/raw/{}", encode_path(&branch)),
            repo_url,
        })
    }

    pub(crate) fn render(&self, body: &str) -> String {
        let links = LinkBase { blob: &self.blob, raw: &self.raw, dir: "" };
        markdown_with_refs(body, &links, &self.repo_url)
    }
}

/// Builds the conversation of an issue or pull request for `viewer`.
/// `placed` holds the inline threads that still attach to the current
/// diff; the others are shown as outdated.
pub(crate) async fn conversation(
    state: &AppState,
    repo: &Repo,
    access: Access,
    issue: &Issue,
    viewer: Option<&User>,
    placed: Option<&HashSet<i64>>,
) -> AppResult<Conversation> {
    let base_url = format!("/{}/{}/issues/{}", repo.owner, repo.name, issue.number);
    let subscription = match viewer {
        Some(v) => {
            if state.core.mark_issue_read(v.id, issue.id).await? {
                state.refresh_unread(v.id).await?;
            }
            Some(state.core.subscription(v, issue).await?)
        }
        None => None,
    };
    let renderer = Renderer::new(state, repo).await?;
    let render = |body: &str| renderer.render(body);
    let writer = access >= Access::Write;
    let is_author = viewer.is_some_and(|v| issue.is_author(v));

    let all_labels = state.core.labels(repo.id).await?;
    let label_view = |name: &str| match all_labels.iter().find(|l| l.name == name) {
        Some(l) => LabelView::new(repo, l, issue.is_pull),
        None => LabelView {
            id: 0,
            name: name.to_owned(),
            color: "gray".into(),
            description: String::new(),
            url: crate::labels::label_url(repo, name, issue.is_pull),
        },
    };

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
        thread: None,
        label: None,
        target: String::new(),
        target_url: String::new(),
    }];
    let comments = state.core.comments(issue.id).await?;
    let can_apply = crate::pulls::viewer_can_apply(state, repo, issue, viewer).await?;
    let mut threads: HashMap<i64, ThreadView> = inline_threads(repo, issue, &comments, viewer, access, can_apply, &render)
        .into_iter()
        .map(|mut t| {
            t.outdated = placed.is_some_and(|p| !p.contains(&t.id));
            (t.id, t)
        })
        .collect();
    for c in comments {
        if c.kind == "inline" && c.reply_to.is_some() {
            continue; // Shown in its thread.
        }
        let mine = viewer.is_some_and(|v| c.author_id == Some(v.id));
        let editable = c.is_editable() && (mine || writer);
        timeline.push(TimelineItem {
            id: c.id,
            author: author_name(&c.author),
            when: relative_time(c.created_at),
            when_full: datetime(c.created_at),
            body_html: match c.kind.as_str() {
                "comment" | "approved" | "changes_requested" if !c.body.is_empty() => render(&c.body),
                _ => String::new(),
            },
            edited: c.updated_at > c.created_at,
            can_edit: editable,
            edit_url: format!("{base_url}/comments/{}/edit", c.id),
            delete_url: editable.then(|| format!("{base_url}/comments/{}/delete", c.id)),
            thread: threads.remove(&c.id),
            label: matches!(c.kind.as_str(), "labeled" | "unlabeled").then(|| label_view(&c.body)),
            target_url: if matches!(c.kind.as_str(), "milestoned" | "demilestoned") {
                milestone_url(repo, &c.body, issue.is_pull)
            } else {
                String::new()
            },
            target: c.body,
            kind: c.kind,
        });
    }

    let mut labels = state.core.labels_of(&[issue.id]).await?.remove(&issue.id).unwrap_or_default();
    labels.sort_by(|a, b| a.name.cmp(&b.name));
    let assignees = state.core.assignees_of(&[issue.id]).await?.remove(&issue.id).unwrap_or_default();
    let (label_choices, assignee_choices) = if writer {
        (
            all_labels
                .iter()
                .map(|l| (LabelView::new(repo, l, issue.is_pull), labels.iter().any(|x| x.id == l.id)))
                .collect(),
            state
                .core
                .assignable_users(repo)
                .await?
                .into_iter()
                .map(|u| {
                    let on = assignees.contains(&u.username);
                    (u.id, u.username, on)
                })
                .collect(),
        )
    } else {
        (Vec::new(), Vec::new())
    };
    let reviews: Vec<(String, &'static str)> = if issue.is_pull {
        state
            .core
            .review_states(issue.id)
            .await?
            .into_iter()
            .map(|(user, v)| (user, v.as_str()))
            .collect()
    } else {
        Vec::new()
    };
    let requested = if issue.is_pull { state.core.review_requests(issue.id).await? } else { Vec::new() };
    let mut reviewers: Vec<ReviewerView> =
        requested.iter().map(|(_, name)| ReviewerView { name: name.clone(), state: "requested" }).collect();
    for (name, verdict) in &reviews {
        if !reviewers.iter().any(|r| r.name == *name) {
            reviewers.push(ReviewerView { name: name.clone(), state: verdict });
        }
    }
    let can_request_review = issue.is_pull && issue.is_open() && (writer || is_author);
    let reviewer_choices = if can_request_review {
        state
            .core
            .reviewer_candidates(repo, issue)
            .await?
            .into_iter()
            .map(|u| {
                let on = requested.iter().any(|(id, _)| *id == u.id);
                (u.id, u.username, on)
            })
            .collect()
    } else {
        Vec::new()
    };
    let milestone = state.core.issue_milestone(issue).await?;
    let milestone_choices = if writer {
        let mut all = state.core.milestones(repo.id, true).await?;
        if let Some(m) = milestone.as_ref().filter(|m| !m.is_open()) {
            all.push(m.clone());
        }
        all.into_iter()
            .map(|m| {
                let on = milestone.as_ref().is_some_and(|x| x.id == m.id);
                (m.id, m.title, on)
            })
            .collect()
    } else {
        Vec::new()
    };
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
        labels: labels.iter().map(|l| LabelView::new(repo, l, issue.is_pull)).collect(),
        assignees,
        can_triage: writer,
        label_choices,
        assignee_choices,
        subscription,
        reviews,
        reviewers,
        can_request_review,
        reviewer_choices,
        milestone: milestone.map(|m| {
            let url = milestone_url(repo, &m.title, issue.is_pull);
            (m.title, url)
        }),
        milestone_choices,
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
    open_url: String,
    closed_url: String,
    /// Filter choices: (label or name, url, selected).
    label_filters: Vec<(LabelView, String, bool)>,
    assignee_filters: Vec<(String, String, bool)>,
    milestone_filters: Vec<(String, String, bool)>,
    /// Pull requests: the viewer's review requests, (url, selected).
    review_filter: Option<(String, bool)>,
    /// The active filters, described, and a link to clear them.
    filter_note: Option<(String, String)>,
}

#[derive(Deserialize)]
pub struct ListQuery {
    state: Option<String>,
    page: Option<i64>,
    label: Option<String>,
    assignee: Option<String>,
    milestone: Option<String>,
    /// Pull requests waiting for this user's review.
    review_requested: Option<String>,
}

/// The filters of an issue list, by name, as in its query string.
#[derive(Clone, Copy, Default)]
struct Filters<'a> {
    label: Option<&'a str>,
    assignee: Option<&'a str>,
    milestone: Option<&'a str>,
    review_requested: Option<&'a str>,
}

/// A query string for an issue list.
fn list_query(open: bool, f: Filters<'_>, page: Option<i64>) -> String {
    let mut q = form_urlencoded::Serializer::new(String::new());
    q.append_pair("state", if open { "open" } else { "closed" });
    for (key, value) in [
        ("label", f.label),
        ("assignee", f.assignee),
        ("milestone", f.milestone),
        ("review_requested", f.review_requested),
    ] {
        if let Some(v) = value {
            q.append_pair(key, v);
        }
    }
    if let Some(p) = page {
        q.append_pair("page", &p.to_string());
    }
    q.finish()
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
    fn nonempty(v: &Option<String>) -> Option<&str> {
        v.as_deref().map(str::trim).filter(|v| !v.is_empty())
    }
    let f = Filters {
        label: nonempty(&q.label),
        assignee: nonempty(&q.assignee),
        milestone: nonempty(&q.milestone),
        review_requested: if pulls { nonempty(&q.review_requested) } else { None },
    };

    // Unknown names match nothing rather than everything.
    let all_labels = state.core.labels(repo.id).await?;
    let label_id = f.label.map(|name| all_labels.iter().find(|l| l.name.eq_ignore_ascii_case(name)).map_or(-1, |l| l.id));
    let assignable = state.core.assignable_users(&repo).await?;
    let user_id = |name: Option<&str>| {
        let state = state.clone();
        let name = name.map(str::to_owned);
        async move {
            Ok::<_, AppError>(match name {
                Some(name) => Some(state.core.user_by_name(&name).await?.map_or(-1, |u| u.id)),
                None => None,
            })
        }
    };
    let assignee_id = user_id(f.assignee).await?;
    let reviewer_id = user_id(f.review_requested).await?;
    let milestones = state.core.milestones(repo.id, true).await?;
    let milestone_id = match f.milestone {
        Some(title) => Some(state.core.milestone_by_title(repo.id, title).await?.map_or(-1, |m| m.id)),
        None => None,
    };
    let filter = IssueFilter { label: label_id, assignee: assignee_id, milestone: milestone_id, review_requested: reviewer_id };

    let mut issues = state
        .core
        .issues(repo.id, pulls, Some(show_open), filter, (page_no - 1) * PER_PAGE, PER_PAGE + 1)
        .await?;
    let has_more = issues.len() as i64 > PER_PAGE;
    issues.truncate(PER_PAGE as usize);
    let (open_count, closed_count) = state.core.filtered_counts(repo.id, pulls, filter).await?;
    let ids: Vec<i64> = issues.iter().map(|i| i.id).collect();
    let mut labels = state.core.labels_of(&ids).await?;
    let mut assignees = state.core.assignees_of(&ids).await?;

    let kind = if pulls { "pulls" } else { "issues" };
    let here = format!("/{}/{}/{kind}", repo.owner, repo.name);
    let url = |open: bool, f: Filters<'_>, page: Option<i64>| format!("{here}?{}", list_query(open, f, page));
    let mut parts = Vec::new();
    if let Some(l) = f.label {
        parts.push(format!("labeled “{l}”"));
    }
    if let Some(a) = f.assignee {
        parts.push(format!("assigned to {a}"));
    }
    if let Some(m) = f.milestone {
        parts.push(format!("in milestone “{m}”"));
    }
    if let Some(r) = f.review_requested {
        parts.push(format!("awaiting review from {r}"));
    }
    let filter_note = (!parts.is_empty()).then(|| (parts.join(" and "), url(show_open, Filters::default(), None)));
    let review_filter = match (&viewer, pulls) {
        (Some(v), true) => {
            let on = f.review_requested.is_some_and(|r| r.eq_ignore_ascii_case(&v.username));
            let target = if on { None } else { Some(v.username.as_str()) };
            Some((url(show_open, Filters { review_requested: target, ..f }, None), on))
        }
        _ => None,
    };
    let default_branch = with_git(&state, &repo, |g| Ok(g.default_branch()?.unwrap_or_default())).await?;
    html(ListPage {
        repo: RepoView::new(&state, &repo, access, viewer.as_ref(), kind, &default_branch).await?,
        page: Page::new(
            &state,
            viewer.clone(),
            format!("{} · {}/{}", if pulls { "Pull requests" } else { "Issues" }, repo.owner, repo.name),
        ),
        pulls,
        show_open,
        open_count,
        closed_count,
        rows: issues.iter().map(|i| IssueRow::new(&repo, i, &mut labels, &mut assignees)).collect(),
        newer_url: (page_no > 1).then(|| url(show_open, f, Some(page_no - 1))),
        older_url: has_more.then(|| url(show_open, f, Some(page_no + 1))),
        signed_in: viewer.is_some(),
        open_url: url(true, f, None),
        closed_url: url(false, f, None),
        label_filters: all_labels
            .iter()
            .map(|l| {
                let on = label_id == Some(l.id);
                let target = if on { None } else { Some(l.name.as_str()) };
                (LabelView::new(&repo, l, pulls), url(show_open, Filters { label: target, ..f }, None), on)
            })
            .collect(),
        assignee_filters: assignable
            .iter()
            .map(|u| {
                let on = assignee_id == Some(u.id);
                let target = if on { None } else { Some(u.username.as_str()) };
                (u.username.clone(), url(show_open, Filters { assignee: target, ..f }, None), on)
            })
            .collect(),
        milestone_filters: milestones
            .iter()
            .map(|m| {
                let on = milestone_id == Some(m.id);
                let target = if on { None } else { Some(m.title.as_str()) };
                (m.title.clone(), url(show_open, Filters { milestone: target, ..f }, None), on)
            })
            .collect(),
        review_filter,
        filter_note,
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
        repo: RepoView::new(&state, &repo, access, Some(&user), "issues", "").await?,
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
            repo: RepoView::new(&state, &repo, access, Some(&user), "issues", "").await?,
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
    let conv = conversation(state, repo, access, issue, viewer.as_ref(), None).await?;
    html(IssuePage {
        repo: RepoView::new(state, repo, access, viewer.as_ref(), "issues", "").await?,
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
        repo: RepoView::new(&state, &repo, access, Some(&user), tab, "").await?,
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
    match state.core.edit_issue(&issue, &user, &f.title, &f.body).await {
        Ok(()) => Ok(Redirect::to(&issue_url(&repo, &issue)).into_response()),
        Err(keel_core::Error::Invalid(msg)) => html(EditPage {
            repo: RepoView::new(&state, &repo, access, Some(&user), "issues", "").await?,
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
        .filter(keel_core::Comment::is_editable)
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
        repo: RepoView::new(&state, &repo, access, Some(&user), if issue.is_pull { "pulls" } else { "issues" }, "").await?,
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
            repo: RepoView::new(&state, &repo, access, Some(&user), "issues", "").await?,
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
