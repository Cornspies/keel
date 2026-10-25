//! Project board pages. Cards move with forms (a menu on each card), or by
//! dragging them when scripts run.

use std::collections::HashMap;

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect};
use keel_core::{Access, Project, ProjectCard, Repo, User};
use serde::Deserialize;

use crate::error::{AppResult, html};
use crate::issues::{Renderer, issue_url, load_issue};
use crate::labels::LabelView;
use crate::render::relative_time;
use crate::repo::{RepoView, load};
use crate::session::{SignedIn, Viewer};
use crate::{AppError, AppState, Page};

fn project_url(repo: &Repo, number: i64) -> String {
    format!("/{}/{}/projects/{number}", repo.owner, repo.name)
}

pub(crate) struct ProjectView {
    pub title: String,
    pub description_html: String,
    pub is_open: bool,
    pub url: String,
    pub updated: String,
    pub open_items: i64,
    pub closed_items: i64,
    pub notes: i64,
    pub progress: i64,
}

impl ProjectView {
    fn new(repo: &Repo, p: &Project, description_html: String) -> Self {
        Self {
            title: p.title.clone(),
            description_html,
            is_open: p.is_open(),
            url: project_url(repo, p.number),
            updated: relative_time(p.updated_at),
            open_items: p.open_items,
            closed_items: p.closed_items,
            notes: p.notes,
            progress: p.progress(),
        }
    }
}

// ---------------------------------------------------------------------------
// List
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "projects/list.html")]
struct ListPage {
    page: Page,
    repo: RepoView,
    projects: Vec<ProjectView>,
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
    let projects = state.core.projects(repo.id, show_open).await?;
    let (open_count, closed_count) = state.core.project_counts(repo.id).await?;
    html(ListPage {
        repo: RepoView::new(&state, &repo, access, viewer.as_ref(), "projects", "").await?,
        page: Page::new(&state, viewer, format!("Projects · {}/{}", repo.owner, repo.name)),
        projects: projects
            .iter()
            .map(|p| {
                let description = if p.description.is_empty() { String::new() } else { renderer.render(&p.description) };
                ProjectView::new(&repo, p, description)
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
#[template(path = "projects/form.html")]
struct FormPage {
    page: Page,
    repo: RepoView,
    /// Editing this one; `None` for a new project.
    number: Option<i64>,
    title: String,
    description: String,
    kanban: bool,
    error: Option<String>,
}

async fn writer_repo(state: &AppState, user: &User, owner: &str, name: &str) -> AppResult<(Repo, Access)> {
    let (repo, access) = load(state, Some(user), owner, name).await?;
    if access < Access::Write {
        return Err(AppError::Forbidden);
    }
    Ok((repo, access))
}

async fn writer_project(state: &AppState, user: &User, owner: &str, name: &str, number: i64) -> AppResult<(Repo, Project)> {
    let (repo, _) = writer_repo(state, user, owner, name).await?;
    let project = state.core.project(repo.id, number).await?.ok_or(AppError::NotFound)?;
    Ok((repo, project))
}

/// What the project form holds.
#[derive(Default)]
struct Fields {
    number: Option<i64>,
    title: String,
    description: String,
    kanban: bool,
    error: Option<String>,
}

async fn form_page(state: &AppState, user: User, repo: &Repo, f: Fields) -> AppResult {
    let heading = if f.number.is_some() { "Edit project" } else { "New project" };
    html(FormPage {
        repo: RepoView::new(state, repo, Access::Write, Some(&user), "projects", "").await?,
        page: Page::new(state, Some(user), format!("{heading} · {}/{}", repo.owner, repo.name)),
        number: f.number,
        title: f.title,
        description: f.description,
        kanban: f.kanban,
        error: f.error,
    })
}

pub async fn new_form(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name)): Path<(String, String)>,
) -> AppResult {
    let (repo, _) = writer_repo(&state, &user, &owner, &name).await?;
    form_page(&state, user, &repo, Fields { kanban: true, ..Default::default() }).await
}

#[derive(Deserialize)]
pub struct ProjectForm {
    title: String,
    #[serde(default)]
    description: String,
    /// New projects: start with To do, In progress and Done.
    kanban: Option<String>,
}

pub async fn create(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name)): Path<(String, String)>,
    Form(f): Form<ProjectForm>,
) -> AppResult {
    let (repo, _) = writer_repo(&state, &user, &owner, &name).await?;
    match state.core.create_project(repo.id, &user, &f.title, &f.description, f.kanban.is_some()).await {
        Ok(p) => Ok(Redirect::to(&project_url(&repo, p.number)).into_response()),
        Err(keel_core::Error::Invalid(msg)) => {
            let kanban = f.kanban.is_some();
            let fields = Fields { title: f.title, description: f.description, kanban, error: Some(msg), ..Default::default() };
            form_page(&state, user, &repo, fields).await
        }
        Err(e) => Err(e.into()),
    }
}

pub async fn edit_form(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> AppResult {
    let (repo, p) = writer_project(&state, &user, &owner, &name, number).await?;
    let fields = Fields { number: Some(p.number), title: p.title, description: p.description, ..Default::default() };
    form_page(&state, user, &repo, fields).await
}

pub async fn edit(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Form(f): Form<ProjectForm>,
) -> AppResult {
    let (repo, p) = writer_project(&state, &user, &owner, &name, number).await?;
    match state.core.edit_project(&p, &f.title, &f.description).await {
        Ok(()) => Ok(Redirect::to(&project_url(&repo, p.number)).into_response()),
        Err(keel_core::Error::Invalid(msg)) => {
            let fields =
                Fields { number: Some(p.number), title: f.title, description: f.description, error: Some(msg), ..Default::default() };
            form_page(&state, user, &repo, fields).await
        }
        Err(e) => Err(e.into()),
    }
}

#[derive(Deserialize)]
pub struct StateForm {
    /// `open` or `closed`.
    to: String,
}

pub async fn set_state(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Form(f): Form<StateForm>,
) -> AppResult {
    let (repo, p) = writer_project(&state, &user, &owner, &name, number).await?;
    state.core.set_project_open(&p, f.to == "open").await?;
    Ok(Redirect::to(&project_url(&repo, p.number)).into_response())
}

pub async fn delete(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> AppResult {
    let (repo, p) = writer_project(&state, &user, &owner, &name, number).await?;
    state.core.delete_project(&p).await?;
    Ok(Redirect::to(&format!("/{}/{}/projects", repo.owner, repo.name)).into_response())
}

// ---------------------------------------------------------------------------
// The board
// ---------------------------------------------------------------------------

pub(crate) struct CardView {
    pub id: i64,
    pub position: i64,
    /// Issue cards: where it is, and its state (`open`, `closed`, `merged`
    /// or `draft`, as in lists).
    pub url: String,
    pub number: i64,
    pub title: String,
    pub state: String,
    pub is_pull: bool,
    pub labels: Vec<LabelView>,
    pub assignees: Vec<String>,
    /// Note cards.
    pub note: Option<String>,
    pub note_html: String,
}

pub(crate) struct ColumnView {
    pub id: i64,
    pub name: String,
    pub is_done: bool,
    pub position: i64,
    pub cards: Vec<CardView>,
}

#[derive(Template)]
#[template(path = "projects/board.html")]
struct BoardPage {
    page: Page,
    repo: RepoView,
    project: ProjectView,
    columns: Vec<ColumnView>,
    can_edit: bool,
    error: Option<String>,
}

async fn board_page(
    state: &AppState,
    viewer: Option<User>,
    repo: &Repo,
    access: Access,
    project: &Project,
    error: Option<String>,
) -> AppResult {
    let renderer = Renderer::new(state, repo).await?;
    let cards = state.core.project_cards(project.id).await?;
    let issue_ids: Vec<i64> = cards.iter().filter_map(|c| c.issue_id).collect();
    let mut labels = state.core.labels_of(&issue_ids).await?;
    let mut assignees = state.core.assignees_of(&issue_ids).await?;
    let mut by_column: HashMap<i64, Vec<CardView>> = HashMap::new();
    for c in cards {
        let view = card_view(repo, &renderer, &c, &mut labels, &mut assignees);
        by_column.entry(c.column_id).or_default().push(view);
    }
    let columns = state
        .core
        .project_columns(project.id)
        .await?
        .into_iter()
        .map(|c| ColumnView {
            cards: by_column.remove(&c.id).unwrap_or_default(),
            id: c.id,
            name: c.name,
            is_done: c.is_done,
            position: c.position,
        })
        .collect();
    let description = if project.description.is_empty() { String::new() } else { renderer.render(&project.description) };
    html(BoardPage {
        repo: RepoView::new(state, repo, access, viewer.as_ref(), "projects", "").await?,
        page: Page::new(state, viewer, format!("{} · Projects · {}/{}", project.title, repo.owner, repo.name)),
        project: ProjectView::new(repo, project, description),
        columns,
        can_edit: access >= Access::Write,
        error,
    })
}

fn card_view(
    repo: &Repo,
    renderer: &Renderer,
    c: &ProjectCard,
    labels: &mut HashMap<i64, Vec<keel_core::Label>>,
    assignees: &mut HashMap<i64, Vec<String>>,
) -> CardView {
    let is_pull = c.issue_is_pull.unwrap_or_default();
    let state = match c.issue_state.as_deref() {
        Some("open") if c.issue_is_draft.unwrap_or_default() => "draft",
        Some(s) => s,
        None => "",
    };
    let number = c.issue_number.unwrap_or_default();
    CardView {
        id: c.id,
        position: c.position,
        url: format!("/{}/{}/{}/{number}", repo.owner, repo.name, if is_pull { "pull" } else { "issues" }),
        number,
        title: c.issue_title.clone().unwrap_or_default(),
        state: state.to_owned(),
        is_pull,
        labels: c
            .issue_id
            .and_then(|id| labels.remove(&id))
            .unwrap_or_default()
            .iter()
            .map(|l| LabelView::new(repo, l, is_pull))
            .collect(),
        assignees: c.issue_id.and_then(|id| assignees.remove(&id)).unwrap_or_default(),
        note_html: c.note.as_deref().map(|n| renderer.render(n)).unwrap_or_default(),
        note: c.note.clone(),
    }
}

pub async fn board(
    State(state): State<AppState>,
    Viewer(viewer): Viewer,
    Path((owner, name, number)): Path<(String, String, i64)>,
) -> AppResult {
    let (repo, access) = load(&state, viewer.as_ref(), &owner, &name).await?;
    let project = state.core.project(repo.id, number).await?.ok_or(AppError::NotFound)?;
    board_page(&state, viewer, &repo, access, &project, None).await
}

/// After a change from the board: back to it, or the board with what went
/// wrong.
async fn back(state: &AppState, user: User, repo: &Repo, project: &Project, done: keel_core::Result<()>) -> AppResult {
    match done {
        Ok(()) => Ok(Redirect::to(&project_url(repo, project.number)).into_response()),
        Err(keel_core::Error::Invalid(msg)) => {
            let access = state.core.access(repo, Some(&user)).await?;
            board_page(state, Some(user), repo, access, project, Some(msg)).await
        }
        Err(e) => Err(e.into()),
    }
}

#[derive(Deserialize)]
pub struct ColumnForm {
    name: String,
    done: Option<String>,
}

pub async fn add_column(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Form(f): Form<ColumnForm>,
) -> AppResult {
    let (repo, p) = writer_project(&state, &user, &owner, &name, number).await?;
    let done = state.core.add_project_column(&p, &f.name).await.map(|_| ());
    back(&state, user, &repo, &p, done).await
}

pub async fn edit_column(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, number, id)): Path<(String, String, i64, i64)>,
    Form(f): Form<ColumnForm>,
) -> AppResult {
    let (repo, p) = writer_project(&state, &user, &owner, &name, number).await?;
    let done = state.core.edit_project_column(&p, id, &f.name, f.done.is_some()).await;
    back(&state, user, &repo, &p, done).await
}

#[derive(Deserialize)]
pub struct PositionForm {
    position: i64,
    /// For cards: the column to move to.
    column: Option<i64>,
    /// Sent by the board's script, which moved the card itself.
    js: Option<String>,
}

pub async fn move_column(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, number, id)): Path<(String, String, i64, i64)>,
    Form(f): Form<PositionForm>,
) -> AppResult {
    let (repo, p) = writer_project(&state, &user, &owner, &name, number).await?;
    let done = state.core.move_project_column(&p, id, f.position).await;
    back(&state, user, &repo, &p, done).await
}

pub async fn delete_column(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, number, id)): Path<(String, String, i64, i64)>,
) -> AppResult {
    let (repo, p) = writer_project(&state, &user, &owner, &name, number).await?;
    let done = state.core.delete_project_column(&p, id).await;
    back(&state, user, &repo, &p, done).await
}

#[derive(Deserialize)]
pub struct CardForm {
    column: i64,
    /// `#12` (or `12`) for an issue or pull request, else a note.
    text: String,
}

/// The issue number `text` names, if it's just `#12` (or `12`).
fn issue_number(text: &str) -> Option<i64> {
    let t = text.trim();
    t.strip_prefix('#').unwrap_or(t).parse().ok().filter(|n| *n > 0)
}

pub async fn add_card(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Form(f): Form<CardForm>,
) -> AppResult {
    let (repo, p) = writer_project(&state, &user, &owner, &name, number).await?;
    let done = match issue_number(&f.text) {
        Some(n) => match state.core.issue(repo.id, n).await? {
            Some(issue) => state.core.add_issue_card(&p, f.column, &issue, &user).await.map(|_| ()),
            None => Err(keel_core::Error::Invalid(format!("There's no issue or pull request #{n} here."))),
        },
        None => state.core.add_note_card(&p, f.column, &f.text, &user).await.map(|_| ()),
    };
    back(&state, user, &repo, &p, done).await
}

pub async fn move_card(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, number, id)): Path<(String, String, i64, i64)>,
    Form(f): Form<PositionForm>,
) -> AppResult {
    let (repo, p) = writer_project(&state, &user, &owner, &name, number).await?;
    let column = f.column.ok_or_else(|| AppError::BadRequest("Which column?".into()))?;
    let done = state.core.move_card(&p, id, column, f.position).await;
    if f.js.is_some() {
        done?;
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    back(&state, user, &repo, &p, done).await
}

#[derive(Deserialize)]
pub struct NoteForm {
    note: String,
}

pub async fn edit_note(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, number, id)): Path<(String, String, i64, i64)>,
    Form(f): Form<NoteForm>,
) -> AppResult {
    let (repo, p) = writer_project(&state, &user, &owner, &name, number).await?;
    let done = state.core.edit_note_card(&p, id, &f.note).await;
    back(&state, user, &repo, &p, done).await
}

pub async fn delete_card(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, number, id)): Path<(String, String, i64, i64)>,
) -> AppResult {
    let (repo, p) = writer_project(&state, &user, &owner, &name, number).await?;
    let done = state.core.delete_card(&p, id).await;
    back(&state, user, &repo, &p, done).await
}

#[derive(Deserialize)]
pub struct AddToProjectForm {
    project: i64,
}

/// From an issue's sidebar: onto the first column of a board.
pub async fn add_issue(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Form(f): Form<AddToProjectForm>,
) -> AppResult {
    let (repo, _) = writer_repo(&state, &user, &owner, &name).await?;
    let issue = load_issue(&state, &repo, number).await?;
    let project = state.core.project(repo.id, f.project).await?.ok_or(AppError::NotFound)?;
    let first = state.core.project_columns(project.id).await?.into_iter().next();
    let Some(first) = first else {
        return Err(AppError::BadRequest(format!("{} has no columns yet.", project.title)));
    };
    match state.core.add_issue_card(&project, first.id, &issue, &user).await {
        Ok(_) | Err(keel_core::Error::Invalid(_)) => Ok(Redirect::to(&issue_url(&repo, &issue)).into_response()),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_numbers() {
        assert_eq!(issue_number("#12"), Some(12));
        assert_eq!(issue_number(" 7 "), Some(7));
        assert_eq!(issue_number("#0"), None);
        assert_eq!(issue_number("Fix #12"), None);
    }
}
