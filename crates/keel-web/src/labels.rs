//! The labels page, and setting the labels and assignees of an issue or
//! pull request.

use askama::Template;
use axum::extract::{Path, RawForm, State};
use axum::response::{IntoResponse, Redirect};
use keel_core::{Access, LABEL_COLORS, Label, Repo};

use crate::error::{AppResult, html};
use crate::issues::{issue_url, load_issue};
use crate::repo::{RepoView, encode_path, load};
use crate::session::{SignedIn, Viewer};
use crate::{AppError, AppState, Page};

/// A label as shown on a chip that links to the issues carrying it.
pub(crate) struct LabelView {
    pub id: i64,
    pub name: String,
    pub color: String,
    pub description: String,
    pub url: String,
}

impl LabelView {
    pub(crate) fn new(repo: &Repo, l: &Label, pulls: bool) -> Self {
        Self::at(&repo.owner, &repo.name, l, pulls)
    }

    /// A label of repository `owner/name`.
    pub(crate) fn at(owner: &str, name: &str, l: &Label, pulls: bool) -> Self {
        Self {
            id: l.id,
            url: label_url_at(owner, name, &l.name, pulls),
            name: l.name.clone(),
            color: l.color.clone(),
            description: l.description.clone(),
        }
    }
}

pub(crate) fn label_url(repo: &Repo, name: &str, pulls: bool) -> String {
    label_url_at(&repo.owner, &repo.name, name, pulls)
}

fn label_url_at(owner: &str, repo: &str, name: &str, pulls: bool) -> String {
    let kind = if pulls { "pulls" } else { "issues" };
    format!("/{owner}/{repo}/{kind}?label={}", encode_path(name).replace('/', "%2F"))
}

/// The values of every `key` in a form body (for checkbox lists, which
/// send one pair per checked box).
pub(crate) fn form_values(body: &[u8], key: &str) -> Vec<String> {
    form_urlencoded::parse(body)
        .filter(|(k, _)| k == key)
        .map(|(_, v)| v.into_owned())
        .collect()
}

fn form_ids(body: &[u8], key: &str) -> Vec<i64> {
    form_values(body, key).iter().filter_map(|v| v.parse().ok()).collect()
}

// ---------------------------------------------------------------------------
// Labels page
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "labels.html")]
struct LabelsPage {
    page: Page,
    repo: RepoView,
    labels: Vec<(LabelView, i64)>,
    colors: &'static [&'static str],
    can_edit: bool,
    error: Option<String>,
    /// The new-label form's values after an error.
    name: String,
    color: String,
    description: String,
}

pub async fn index(
    State(state): State<AppState>,
    Viewer(viewer): Viewer,
    Path((owner, name)): Path<(String, String)>,
) -> AppResult {
    let (repo, access) = load(&state, viewer.as_ref(), &owner, &name).await?;
    labels_page(&state, viewer, &repo, access, None, ("", "blue", "")).await
}

async fn labels_page(
    state: &AppState,
    viewer: Option<keel_core::User>,
    repo: &Repo,
    access: Access,
    error: Option<String>,
    (name, color, description): (&str, &str, &str),
) -> AppResult {
    let labels = state.core.labels(repo.id).await?;
    let counts = state.core.label_counts(repo.id).await?;
    html(LabelsPage {
        repo: RepoView::new(state, repo, access, viewer.as_ref(), "labels", "").await?,
        page: Page::new(state, viewer, format!("Labels · {}/{}", repo.owner, repo.name)),
        labels: labels
            .iter()
            .map(|l| (LabelView::new(repo, l, false), counts.get(&l.id).copied().unwrap_or(0)))
            .collect(),
        colors: LABEL_COLORS,
        can_edit: access >= Access::Write,
        error,
        name: name.to_owned(),
        color: color.to_owned(),
        description: description.to_owned(),
    })
}

/// The repository, if `user` may manage its labels and triage its issues.
async fn triage(state: &AppState, user: &keel_core::User, owner: &str, name: &str) -> AppResult<(Repo, Access)> {
    let (repo, access) = load(state, Some(user), owner, name).await?;
    if access < Access::Write {
        return Err(AppError::Forbidden);
    }
    Ok((repo, access))
}

fn field(body: &[u8], key: &str) -> String {
    form_values(body, key).into_iter().next().unwrap_or_default()
}

pub async fn create(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name)): Path<(String, String)>,
    RawForm(body): RawForm,
) -> AppResult {
    let (repo, access) = triage(&state, &user, &owner, &name).await?;
    let (n, c, d) = (field(&body, "name"), field(&body, "color"), field(&body, "description"));
    match state.core.save_label(repo.id, None, &n, &c, &d).await {
        Ok(()) => Ok(Redirect::to(&format!("/{}/{}/labels", repo.owner, repo.name)).into_response()),
        Err(keel_core::Error::Invalid(msg)) => {
            labels_page(&state, Some(user), &repo, access, Some(msg), (&n, &c, &d)).await
        }
        Err(e) => Err(e.into()),
    }
}

pub async fn update(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, id)): Path<(String, String, i64)>,
    RawForm(body): RawForm,
) -> AppResult {
    let (repo, access) = triage(&state, &user, &owner, &name).await?;
    let (n, c, d) = (field(&body, "name"), field(&body, "color"), field(&body, "description"));
    match state.core.save_label(repo.id, Some(id), &n, &c, &d).await {
        Ok(()) => Ok(Redirect::to(&format!("/{}/{}/labels", repo.owner, repo.name)).into_response()),
        Err(keel_core::Error::Invalid(msg)) => {
            labels_page(&state, Some(user), &repo, access, Some(msg), ("", "blue", "")).await
        }
        Err(e) => Err(e.into()),
    }
}

pub async fn delete(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> AppResult {
    let (repo, _) = triage(&state, &user, &owner, &name).await?;
    state.core.delete_label(repo.id, id).await?;
    Ok(Redirect::to(&format!("/{}/{}/labels", repo.owner, repo.name)).into_response())
}

// ---------------------------------------------------------------------------
// An issue's labels and assignees
// ---------------------------------------------------------------------------

pub async fn set_issue_labels(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, number)): Path<(String, String, i64)>,
    RawForm(body): RawForm,
) -> AppResult {
    let (repo, _) = triage(&state, &user, &owner, &name).await?;
    let issue = load_issue(&state, &repo, number).await?;
    state.core.set_issue_labels(&issue, &user, &form_ids(&body, "label")).await?;
    Ok(Redirect::to(&issue_url(&repo, &issue)).into_response())
}

pub async fn set_assignees(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, number)): Path<(String, String, i64)>,
    RawForm(body): RawForm,
) -> AppResult {
    let (repo, _) = triage(&state, &user, &owner, &name).await?;
    let issue = load_issue(&state, &repo, number).await?;
    match state.core.set_assignees(&repo, &issue, &user, &form_ids(&body, "user")).await {
        Ok(()) => Ok(Redirect::to(&issue_url(&repo, &issue)).into_response()),
        Err(keel_core::Error::Invalid(msg)) => Err(AppError::BadRequest(msg)),
        Err(e) => Err(e.into()),
    }
}

/// Asks people to review a pull request (writers and its author may).
pub async fn set_reviewers(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, number)): Path<(String, String, i64)>,
    RawForm(body): RawForm,
) -> AppResult {
    let (repo, access) = load(&state, Some(&user), &owner, &name).await?;
    let issue = load_issue(&state, &repo, number).await?;
    if access < Access::Write && !issue.is_author(&user) {
        return Err(AppError::Forbidden);
    }
    match state.core.set_review_requests(&repo, &issue, &user, &form_ids(&body, "user")).await {
        Ok(()) => Ok(Redirect::to(&issue_url(&repo, &issue)).into_response()),
        Err(keel_core::Error::Invalid(msg)) => Err(AppError::BadRequest(msg)),
        Err(e) => Err(e.into()),
    }
}
