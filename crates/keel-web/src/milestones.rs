//! Milestone pages, and setting the milestone of an issue or pull request.

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Redirect};
use keel_core::{Access, Milestone, Repo, User, parse_due_date};
use serde::Deserialize;

use crate::error::{AppResult, html};
use crate::issues::{Renderer, issue_url, load_issue};
use crate::render::{date, relative_time};
use crate::repo::{RepoView, load};
use crate::session::{SignedIn, Viewer};
use crate::{AppError, AppState, Page};

pub(crate) struct MilestoneView {
    pub id: i64,
    pub title: String,
    pub description_html: String,
    pub is_open: bool,
    /// "Due by 2026-11-01", "Overdue by …", "Closed 3 days ago" or empty.
    pub due: String,
    pub overdue: bool,
    pub open_items: i64,
    pub closed_items: i64,
    pub progress: i64,
    /// The issues and pull requests in it.
    pub issues_url: String,
    pub pulls_url: String,
}

impl MilestoneView {
    pub(crate) fn new(repo: &Repo, m: &Milestone, description_html: String) -> Self {
        let now = crate::now();
        let due = match (m.is_open(), m.due_on, m.closed_at) {
            (false, _, Some(closed)) => format!("Closed {}", relative_time(closed)),
            (true, Some(d), _) if m.is_overdue(now) => format!("Past due by {}", ago(now - d - 86_400)),
            (true, Some(d), _) => format!("Due by {}", date(d)),
            _ => String::new(),
        };
        Self {
            id: m.id,
            title: m.title.clone(),
            description_html,
            is_open: m.is_open(),
            overdue: m.is_overdue(now),
            due,
            open_items: m.open_items,
            closed_items: m.closed_items,
            progress: m.progress(),
            issues_url: milestone_url(repo, &m.title, false),
            pulls_url: milestone_url(repo, &m.title, true),
        }
    }
}

/// "3 days", for overdue milestones.
fn ago(secs: i64) -> String {
    match secs / 86_400 {
        0 => "less than a day".into(),
        1 => "1 day".into(),
        n => format!("{n} days"),
    }
}

pub(crate) fn milestone_url(repo: &Repo, title: &str, pulls: bool) -> String {
    let kind = if pulls { "pulls" } else { "issues" };
    let q = form_urlencoded::Serializer::new(String::new()).append_pair("milestone", title).finish();
    format!("/{}/{}/{kind}?{q}", repo.owner, repo.name)
}

// ---------------------------------------------------------------------------
// List
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "milestones.html")]
struct ListPage {
    page: Page,
    repo: RepoView,
    milestones: Vec<MilestoneView>,
    show_open: bool,
    open_count: i64,
    closed_count: i64,
    can_edit: bool,
}

#[derive(Deserialize)]
pub struct ListQuery {
    state: Option<String>,
}

pub async fn index(
    State(state): State<AppState>,
    Viewer(viewer): Viewer,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<ListQuery>,
) -> AppResult {
    let (repo, access) = load(&state, viewer.as_ref(), &owner, &name).await?;
    let show_open = q.state.as_deref() != Some("closed");
    let renderer = Renderer::new(&state, &repo).await?;
    let milestones = state.core.milestones(repo.id, show_open).await?;
    let (open_count, closed_count) = state.core.milestone_counts(repo.id).await?;
    html(ListPage {
        repo: RepoView::new(&state, &repo, access, viewer.as_ref(), "milestones", "").await?,
        page: Page::new(&state, viewer, format!("Milestones · {}/{}", repo.owner, repo.name)),
        milestones: milestones
            .iter()
            .map(|m| {
                let description = if m.description.is_empty() { String::new() } else { renderer.render(&m.description) };
                MilestoneView::new(&repo, m, description)
            })
            .collect(),
        show_open,
        open_count,
        closed_count,
        can_edit: access >= Access::Write,
    })
}

// ---------------------------------------------------------------------------
// New and edit
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "milestone_form.html")]
struct FormPage {
    page: Page,
    repo: RepoView,
    heading: String,
    action: String,
    title: String,
    description: String,
    due_on: String,
    error: Option<String>,
}

async fn writer_repo(state: &AppState, user: &User, owner: &str, name: &str) -> AppResult<Repo> {
    let (repo, access) = load(state, Some(user), owner, name).await?;
    if access < Access::Write {
        return Err(AppError::Forbidden);
    }
    Ok(repo)
}

#[allow(clippy::too_many_arguments)]
async fn form_page(
    state: &AppState,
    user: User,
    repo: &Repo,
    id: Option<i64>,
    title: String,
    description: String,
    due_on: String,
    error: Option<String>,
) -> AppResult {
    let base = format!("/{}/{}/milestones", repo.owner, repo.name);
    let (heading, action) = match id {
        Some(id) => ("Edit milestone".to_owned(), format!("{base}/{id}/edit")),
        None => ("New milestone".to_owned(), base),
    };
    html(FormPage {
        repo: RepoView::new(state, repo, Access::Write, Some(&user), "milestones", "").await?,
        page: Page::new(state, Some(user), format!("{heading} · {}/{}", repo.owner, repo.name)),
        heading,
        action,
        title,
        description,
        due_on,
        error,
    })
}

pub async fn new_form(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name)): Path<(String, String)>,
) -> AppResult {
    let repo = writer_repo(&state, &user, &owner, &name).await?;
    form_page(&state, user, &repo, None, String::new(), String::new(), String::new(), None).await
}

pub async fn edit_form(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> AppResult {
    let repo = writer_repo(&state, &user, &owner, &name).await?;
    let m = state.core.milestone(repo.id, id).await?.ok_or(AppError::NotFound)?;
    let due = m.due_on.map(date).unwrap_or_default();
    form_page(&state, user, &repo, Some(id), m.title, m.description, due, None).await
}

#[derive(Deserialize)]
pub struct MilestoneForm {
    title: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    due_on: String,
}

async fn save(state: AppState, user: User, owner: String, name: String, id: Option<i64>, f: MilestoneForm) -> AppResult {
    let repo = writer_repo(&state, &user, &owner, &name).await?;
    if let Some(id) = id {
        state.core.milestone(repo.id, id).await?.ok_or(AppError::NotFound)?;
    }
    let result = match parse_due_date(&f.due_on) {
        Ok(due) => state.core.save_milestone(repo.id, id, &f.title, &f.description, due).await,
        Err(e) => Err(e),
    };
    match result {
        Ok(_) => Ok(Redirect::to(&format!("/{}/{}/milestones", repo.owner, repo.name)).into_response()),
        Err(keel_core::Error::Invalid(msg)) => {
            form_page(&state, user, &repo, id, f.title, f.description, f.due_on, Some(msg)).await
        }
        Err(e) => Err(e.into()),
    }
}

pub async fn create(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name)): Path<(String, String)>,
    Form(f): Form<MilestoneForm>,
) -> AppResult {
    save(state, user, owner, name, None, f).await
}

pub async fn update(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, id)): Path<(String, String, i64)>,
    Form(f): Form<MilestoneForm>,
) -> AppResult {
    save(state, user, owner, name, Some(id), f).await
}

#[derive(Deserialize)]
pub struct StateForm {
    /// `open` or `closed`.
    to: String,
}

pub async fn set_state(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, id)): Path<(String, String, i64)>,
    Form(f): Form<StateForm>,
) -> AppResult {
    let repo = writer_repo(&state, &user, &owner, &name).await?;
    let open = f.to == "open";
    state.core.set_milestone_open(repo.id, id, open).await?;
    let back = if open { "" } else { "?state=closed" };
    Ok(Redirect::to(&format!("/{}/{}/milestones{back}", repo.owner, repo.name)).into_response())
}

pub async fn delete(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> AppResult {
    let repo = writer_repo(&state, &user, &owner, &name).await?;
    state.core.delete_milestone(repo.id, id).await?;
    Ok(Redirect::to(&format!("/{}/{}/milestones", repo.owner, repo.name)).into_response())
}

// ---------------------------------------------------------------------------
// An issue's milestone
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct IssueMilestoneForm {
    /// A milestone id, or empty for none.
    #[serde(default)]
    milestone: String,
}

pub async fn set_issue_milestone(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Form(f): Form<IssueMilestoneForm>,
) -> AppResult {
    let repo = writer_repo(&state, &user, &owner, &name).await?;
    let issue = load_issue(&state, &repo, number).await?;
    let id = f.milestone.trim().parse().ok();
    match state.core.set_issue_milestone(&issue, &user, id).await {
        Ok(()) => Ok(Redirect::to(&issue_url(&repo, &issue)).into_response()),
        Err(keel_core::Error::Invalid(msg)) => Err(AppError::BadRequest(msg)),
        Err(e) => Err(e.into()),
    }
}
