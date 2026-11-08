//! Project board pages, for a repository's boards (`/owner/name/projects`)
//! and an organization's (`/org/name/projects`, for its members). Cards
//! move with forms (a menu on each card), or by dragging them when scripts
//! run.

use std::collections::HashMap;

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Redirect};
use keel_core::{Access, Project, ProjectCard, ProjectOwner, Repo, User};
use serde::Deserialize;

use crate::error::{AppResult, html};
use crate::issues::{Renderer, issue_url, load_issue};
use crate::labels::LabelView;
use crate::render::relative_time;
use crate::repo::{RepoView, load};
use crate::session::{SignedIn, Viewer};
use crate::{AppError, AppState, Page};

/// The path of a board page: `owner` and `repo`, or `org`; then the
/// board's number and a column's or card's id.
#[derive(Deserialize)]
pub struct Where {
    owner: Option<String>,
    repo: Option<String>,
    org: Option<String>,
    number: Option<i64>,
    id: Option<i64>,
}

impl Where {
    fn number(&self) -> AppResult<i64> {
        self.number.ok_or(AppError::NotFound)
    }

    fn id(&self) -> AppResult<i64> {
        self.id.ok_or(AppError::NotFound)
    }
}

/// Whose boards a page is about.
pub(crate) struct Home {
    owner: ProjectOwner,
    /// A repository's boards: the repository and the viewer's access.
    repo: Option<(Repo, Access)>,
    /// An organization's.
    org: Option<User>,
    can_edit: bool,
}

impl Home {
    async fn load(state: &AppState, viewer: Option<&User>, w: &Where) -> AppResult<Self> {
        if let Some(name) = &w.org {
            let org = state.core.user_by_name(name).await?.filter(User::is_org).ok_or(AppError::NotFound)?;
            let access = state.core.org_board_access(&org, viewer).await?;
            if access == Access::None {
                return Err(AppError::NotFound);
            }
            return Ok(Self { owner: ProjectOwner::Org(org.id), repo: None, org: Some(org), can_edit: access >= Access::Write });
        }
        let (Some(owner), Some(name)) = (&w.owner, &w.repo) else { return Err(AppError::NotFound) };
        let (repo, access) = load(state, viewer, owner, name).await?;
        Ok(Self { owner: ProjectOwner::Repo(repo.id), can_edit: access >= Access::Write, repo: Some((repo, access)), org: None })
    }

    /// For changes: `user` must be able to edit the boards.
    async fn writable(state: &AppState, user: &User, w: &Where) -> AppResult<Self> {
        let home = Self::load(state, Some(user), w).await?;
        if !home.can_edit {
            return Err(AppError::Forbidden);
        }
        Ok(home)
    }

    async fn project(&self, state: &AppState, number: i64) -> AppResult<Project> {
        state.core.project(self.owner, number).await?.ok_or(AppError::NotFound)
    }

    /// The list of boards.
    fn url(&self) -> String {
        match (&self.repo, &self.org) {
            (Some((r, _)), _) => format!("/{}/{}/projects", r.owner, r.name),
            (_, Some(o)) => format!("/org/{}/projects", o.username),
            _ => String::new(),
        }
    }

    fn project_url(&self, number: i64) -> String {
        format!("{}/{number}", self.url())
    }

    /// `owner/name`, or the organization's name, for titles.
    fn name(&self) -> String {
        match (&self.repo, &self.org) {
            (Some((r, _)), _) => format!("{}/{}", r.owner, r.name),
            (_, Some(o)) => o.username.clone(),
            _ => String::new(),
        }
    }

    async fn renderer(&self, state: &AppState) -> AppResult<Renderer> {
        match &self.repo {
            Some((repo, _)) => Renderer::new(state, repo).await,
            None => Ok(Renderer::plain()),
        }
    }

    async fn view(&self, state: &AppState, viewer: Option<&User>) -> AppResult<HomeView> {
        let repo = match &self.repo {
            Some((repo, access)) => Some(RepoView::new(state, repo, *access, viewer, "projects", "").await?),
            None => None,
        };
        let org = match &self.org {
            Some(o) => Some(crate::orgs::org_head(state, o, viewer, "projects").await?),
            None => None,
        };
        Ok(HomeView { repo, org, url: self.url() })
    }
}

/// [`Home`] for templates.
pub(crate) struct HomeView {
    /// A repository's boards: its header.
    pub repo: Option<RepoView>,
    /// An organization's: the top of its pages.
    pub org: Option<crate::orgs::OrgHead>,
    /// The list of boards.
    pub url: String,
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
    fn new(home: &Home, p: &Project, description_html: String) -> Self {
        Self {
            title: p.title.clone(),
            description_html,
            is_open: p.is_open(),
            url: home.project_url(p.number),
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
    home: HomeView,
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
    Path(w): Path<Where>,
    Query(q): Query<ListQuery>,
) -> AppResult {
    let home = Home::load(&state, viewer.as_ref(), &w).await?;
    let show_open = q.state.as_deref() != Some("closed");
    let renderer = home.renderer(&state).await?;
    let projects = state.core.projects(home.owner, show_open).await?;
    let (open_count, closed_count) = state.core.project_counts(home.owner).await?;
    html(ListPage {
        home: home.view(&state, viewer.as_ref()).await?,
        page: Page::new(&state, viewer, format!("Projects · {}", home.name())),
        projects: projects
            .iter()
            .map(|p| {
                let description = if p.description.is_empty() { String::new() } else { renderer.render(&p.description) };
                ProjectView::new(&home, p, description)
            })
            .collect(),
        show_open,
        open_count,
        closed_count,
        can_edit: home.can_edit,
    })
}

// ---------------------------------------------------------------------------
// New and edit
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "projects/form.html")]
struct FormPage {
    page: Page,
    home: HomeView,
    /// Editing this one; `None` for a new project.
    number: Option<i64>,
    title: String,
    description: String,
    kanban: bool,
    error: Option<String>,
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

async fn form_page(state: &AppState, user: User, home: &Home, f: Fields) -> AppResult {
    let heading = if f.number.is_some() { "Edit project" } else { "New project" };
    html(FormPage {
        home: home.view(state, Some(&user)).await?,
        page: Page::new(state, Some(user), format!("{heading} · {}", home.name())),
        number: f.number,
        title: f.title,
        description: f.description,
        kanban: f.kanban,
        error: f.error,
    })
}

pub async fn new_form(State(state): State<AppState>, SignedIn(user): SignedIn, Path(w): Path<Where>) -> AppResult {
    let home = Home::writable(&state, &user, &w).await?;
    form_page(&state, user, &home, Fields { kanban: true, ..Default::default() }).await
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
    Path(w): Path<Where>,
    Form(f): Form<ProjectForm>,
) -> AppResult {
    let home = Home::writable(&state, &user, &w).await?;
    match state.core.create_project(home.owner, &user, &f.title, &f.description, f.kanban.is_some()).await {
        Ok(p) => Ok(Redirect::to(&home.project_url(p.number)).into_response()),
        Err(keel_core::Error::Invalid(msg)) => {
            let kanban = f.kanban.is_some();
            let fields = Fields { title: f.title, description: f.description, kanban, error: Some(msg), ..Default::default() };
            form_page(&state, user, &home, fields).await
        }
        Err(e) => Err(e.into()),
    }
}

pub async fn edit_form(State(state): State<AppState>, SignedIn(user): SignedIn, Path(w): Path<Where>) -> AppResult {
    let home = Home::writable(&state, &user, &w).await?;
    let p = home.project(&state, w.number()?).await?;
    let fields = Fields { number: Some(p.number), title: p.title, description: p.description, ..Default::default() };
    form_page(&state, user, &home, fields).await
}

pub async fn edit(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path(w): Path<Where>,
    Form(f): Form<ProjectForm>,
) -> AppResult {
    let home = Home::writable(&state, &user, &w).await?;
    let p = home.project(&state, w.number()?).await?;
    match state.core.edit_project(&p, &f.title, &f.description, &user).await {
        Ok(()) => Ok(Redirect::to(&home.project_url(p.number)).into_response()),
        Err(keel_core::Error::Invalid(msg)) => {
            let fields =
                Fields { number: Some(p.number), title: f.title, description: f.description, error: Some(msg), ..Default::default() };
            form_page(&state, user, &home, fields).await
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
    Path(w): Path<Where>,
    Form(f): Form<StateForm>,
) -> AppResult {
    let home = Home::writable(&state, &user, &w).await?;
    let p = home.project(&state, w.number()?).await?;
    state.core.set_project_open(&p, f.to == "open", &user).await?;
    Ok(Redirect::to(&home.project_url(p.number)).into_response())
}

pub async fn delete(State(state): State<AppState>, SignedIn(user): SignedIn, Path(w): Path<Where>) -> AppResult {
    let home = Home::writable(&state, &user, &w).await?;
    let p = home.project(&state, w.number()?).await?;
    state.core.delete_project(&p, &user).await?;
    Ok(Redirect::to(&home.url()).into_response())
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
    /// On organization boards, the repository's name.
    pub repo: Option<String>,
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
    home: HomeView,
    project: ProjectView,
    columns: Vec<ColumnView>,
    can_edit: bool,
    /// What to type to add an issue: `#12`, or `repo#12` on organization
    /// boards.
    issue_hint: &'static str,
    error: Option<String>,
}

async fn board_page(state: &AppState, viewer: Option<User>, home: &Home, project: &Project, error: Option<String>) -> AppResult {
    let renderer = home.renderer(state).await?;
    let mut cards = state.core.project_cards(project.id).await?;
    if home.org.is_some() {
        readable_cards(state, viewer.as_ref(), &mut cards).await?;
    }
    let issue_ids: Vec<i64> = cards.iter().filter_map(|c| c.issue_id).collect();
    let mut labels = state.core.labels_of(&issue_ids).await?;
    let mut assignees = state.core.assignees_of(&issue_ids).await?;
    let mut by_column: HashMap<i64, Vec<CardView>> = HashMap::new();
    for c in cards {
        let view = card_view(home, &renderer, &c, &mut labels, &mut assignees);
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
        home: home.view(state, viewer.as_ref()).await?,
        page: Page::new(state, viewer, format!("{} · Projects · {}", project.title, home.name())),
        project: ProjectView::new(home, project, description),
        columns,
        can_edit: home.can_edit,
        issue_hint: if home.org.is_some() { "repo#12" } else { "#12" },
        error,
    })
}

/// Leaves out the cards of repositories `viewer` can't read: on
/// organization boards, of repositories that left the organization since.
pub(crate) async fn readable_cards(state: &AppState, viewer: Option<&User>, cards: &mut Vec<ProjectCard>) -> AppResult<()> {
    let repos: std::collections::BTreeSet<i64> = cards.iter().filter_map(|c| c.issue_repo_id).collect();
    let mut readable = std::collections::BTreeSet::new();
    for id in repos {
        if let Some(r) = state.core.repo_by_id(id).await?
            && state.core.access(&r, viewer).await? >= Access::Read
        {
            readable.insert(id);
        }
    }
    cards.retain(|c| c.issue_repo_id.is_none_or(|id| readable.contains(&id)));
    Ok(())
}

fn card_view(
    home: &Home,
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
    let (owner, repo) = keel_core::card_repo(c, home.repo.as_ref().map(|(r, _)| r)).unwrap_or_default();
    CardView {
        id: c.id,
        position: c.position,
        url: format!("/{owner}/{repo}/{}/{number}", if is_pull { "pull" } else { "issues" }),
        number,
        repo: home.org.is_some().then(|| repo.clone()),
        title: c.issue_title.clone().unwrap_or_default(),
        state: state.to_owned(),
        is_pull,
        labels: c
            .issue_id
            .and_then(|id| labels.remove(&id))
            .unwrap_or_default()
            .iter()
            .map(|l| LabelView::at(&owner, &repo, l, is_pull))
            .collect(),
        assignees: c.issue_id.and_then(|id| assignees.remove(&id)).unwrap_or_default(),
        note_html: c.note.as_deref().map(|n| renderer.render(n)).unwrap_or_default(),
        note: c.note.clone(),
    }
}

pub async fn board(State(state): State<AppState>, Viewer(viewer): Viewer, Path(w): Path<Where>) -> AppResult {
    let home = Home::load(&state, viewer.as_ref(), &w).await?;
    let project = home.project(&state, w.number()?).await?;
    board_page(&state, viewer, &home, &project, None).await
}

/// After a change from the board: back to it, or the board with what went
/// wrong.
async fn back(state: &AppState, user: User, home: &Home, project: &Project, done: keel_core::Result<()>) -> AppResult {
    match done {
        Ok(()) => Ok(Redirect::to(&home.project_url(project.number)).into_response()),
        Err(keel_core::Error::Invalid(msg)) => board_page(state, Some(user), home, project, Some(msg)).await,
        Err(e) => Err(e.into()),
    }
}

/// For changes to a board: who may make them, whose board, and which.
async fn writable_project(state: &AppState, user: &User, w: &Where) -> AppResult<(Home, Project)> {
    let home = Home::writable(state, user, w).await?;
    let project = home.project(state, w.number()?).await?;
    Ok((home, project))
}

#[derive(Deserialize)]
pub struct ColumnForm {
    name: String,
    done: Option<String>,
}

pub async fn add_column(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path(w): Path<Where>,
    Form(f): Form<ColumnForm>,
) -> AppResult {
    let (home, p) = writable_project(&state, &user, &w).await?;
    let done = state.core.add_project_column(&p, &f.name, &user).await.map(|_| ());
    back(&state, user, &home, &p, done).await
}

pub async fn edit_column(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path(w): Path<Where>,
    Form(f): Form<ColumnForm>,
) -> AppResult {
    let (home, p) = writable_project(&state, &user, &w).await?;
    let done = state.core.edit_project_column(&p, w.id()?, &f.name, f.done.is_some(), &user).await;
    back(&state, user, &home, &p, done).await
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
    Path(w): Path<Where>,
    Form(f): Form<PositionForm>,
) -> AppResult {
    let (home, p) = writable_project(&state, &user, &w).await?;
    let done = state.core.move_project_column(&p, w.id()?, f.position, &user).await;
    back(&state, user, &home, &p, done).await
}

pub async fn delete_column(State(state): State<AppState>, SignedIn(user): SignedIn, Path(w): Path<Where>) -> AppResult {
    let (home, p) = writable_project(&state, &user, &w).await?;
    let done = state.core.delete_project_column(&p, w.id()?, &user).await;
    back(&state, user, &home, &p, done).await
}

#[derive(Deserialize)]
pub struct CardForm {
    column: i64,
    /// `#12` (or `12`) for an issue or pull request, `repo#12` on
    /// organization boards; else a note.
    text: String,
}

/// The issue `text` names, if it's just `#12` (or `12`), or `repo#12`
/// (or `owner/repo#12`): the repository (if named) and number.
fn issue_ref(text: &str) -> Option<(Option<&str>, i64)> {
    let t = text.trim();
    let (repo, number) = match t.split_once('#') {
        Some((repo, n)) => (Some(repo).filter(|r| !r.is_empty()), n),
        None => (None, t),
    };
    let valid = repo.is_none_or(|r| !r.is_empty() && r.split('/').count() <= 2 && !r.contains(char::is_whitespace));
    let number = number.parse().ok().filter(|n| *n > 0)?;
    valid.then_some((repo, number))
}

pub async fn add_card(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path(w): Path<Where>,
    Form(f): Form<CardForm>,
) -> AppResult {
    let (home, p) = writable_project(&state, &user, &w).await?;
    let done = match issue_ref(&f.text) {
        Some((named, n)) => {
            // Which repository: the board's, or one of the organization's.
            let repo = match (&home.repo, &home.org, named) {
                (Some((repo, _)), _, None) => Some(repo.clone()),
                (Some((repo, _)), _, Some(name)) if name == repo.name || name == format!("{}/{}", repo.owner, repo.name) => {
                    Some(repo.clone())
                }
                (_, Some(org), Some(name)) => {
                    let name = name.strip_prefix(&format!("{}/", org.username)).unwrap_or(name);
                    match state.core.repo(&org.username, name).await? {
                        Some(r) if state.core.access(&r, Some(&user)).await? > Access::None => Some(r),
                        _ => None,
                    }
                }
                _ => None,
            };
            let issue = match repo {
                Some(r) => state.core.issue(r.id, n).await?,
                None => None,
            };
            match issue {
                Some(issue) => state.core.add_issue_card(&p, f.column, &issue, &user).await.map(|_| ()),
                None if home.org.is_some() && named.is_none() => {
                    Err(keel_core::Error::Invalid(format!("Which repository's #{n}? Write it like repo#{n}.")))
                }
                None => Err(keel_core::Error::Invalid(format!("There's no issue or pull request {}.", f.text.trim()))),
            }
        }
        None => state.core.add_note_card(&p, f.column, &f.text, &user).await.map(|_| ()),
    };
    back(&state, user, &home, &p, done).await
}

pub async fn move_card(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path(w): Path<Where>,
    Form(f): Form<PositionForm>,
) -> AppResult {
    let (home, p) = writable_project(&state, &user, &w).await?;
    let column = f.column.ok_or_else(|| AppError::BadRequest("Which column?".into()))?;
    let done = state.core.move_card(&p, w.id()?, column, f.position, &user).await;
    if f.js.is_some() {
        done?;
        return Ok(StatusCode::NO_CONTENT.into_response());
    }
    back(&state, user, &home, &p, done).await
}

#[derive(Deserialize)]
pub struct NoteForm {
    note: String,
}

pub async fn edit_note(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path(w): Path<Where>,
    Form(f): Form<NoteForm>,
) -> AppResult {
    let (home, p) = writable_project(&state, &user, &w).await?;
    let done = state.core.edit_note_card(&p, w.id()?, &f.note, &user).await;
    back(&state, user, &home, &p, done).await
}

pub async fn delete_card(State(state): State<AppState>, SignedIn(user): SignedIn, Path(w): Path<Where>) -> AppResult {
    let (home, p) = writable_project(&state, &user, &w).await?;
    let done = state.core.delete_card(&p, w.id()?, &user).await;
    back(&state, user, &home, &p, done).await
}

#[derive(Deserialize)]
pub struct AddToProjectForm {
    /// `12` for one of the repository's boards, `org:12` for one of its
    /// organization's.
    project: String,
}

/// From an issue's sidebar: onto the first column of a board.
pub async fn add_issue(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Form(f): Form<AddToProjectForm>,
) -> AppResult {
    let (repo, access) = load(&state, Some(&user), &owner, &name).await?;
    if access < Access::Write {
        return Err(AppError::Forbidden);
    }
    let issue = load_issue(&state, &repo, number).await?;
    let board = match f.project.strip_prefix("org:") {
        Some(n) => {
            let org = state.core.user_by_id(repo.owner_id).await?.filter(User::is_org).ok_or(AppError::NotFound)?;
            if state.core.org_board_access(&org, Some(&user)).await? < Access::Write {
                return Err(AppError::Forbidden);
            }
            (ProjectOwner::Org(org.id), n)
        }
        None => (ProjectOwner::Repo(repo.id), f.project.as_str()),
    };
    let number = board.1.parse().map_err(|_| AppError::NotFound)?;
    let project = state.core.project(board.0, number).await?.ok_or(AppError::NotFound)?;
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
    fn issue_refs() {
        assert_eq!(issue_ref("#12"), Some((None, 12)));
        assert_eq!(issue_ref(" 7 "), Some((None, 7)));
        assert_eq!(issue_ref("tool#3"), Some((Some("tool"), 3)));
        assert_eq!(issue_ref("acme/tool#3"), Some((Some("acme/tool"), 3)));
        assert_eq!(issue_ref("#0"), None);
        assert_eq!(issue_ref("Fix #12"), None);
        assert_eq!(issue_ref("a/b/c#1"), None);
    }
}
