//! Forking repositories.

use askama::Template;
use axum::Form;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Redirect};
use serde::Deserialize;

use crate::error::{AppResult, html};
use crate::pages::{RepoCard, possible_owners};
use crate::repo::{RepoView, load};
use crate::session::{SignedIn, Viewer};
use crate::{AppError, AppState, Page};

#[derive(Template)]
#[template(path = "repo/fork.html")]
struct ForkPage {
    page: Page,
    repo: RepoView,
    owners: Vec<String>,
    owner: String,
    name: String,
    error: Option<String>,
    /// The viewer's existing forks of this repository.
    existing: Vec<RepoCard>,
}

pub async fn form(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name)): Path<(String, String)>,
) -> AppResult {
    let (repo, access) = load(&state, Some(&user), &owner, &name).await?;
    let owners: Vec<String> = possible_owners(&state, &user)
        .await?
        .into_iter()
        .filter(|o| *o != repo.owner)
        .collect();
    let existing = state
        .core
        .forks_of(repo.id, Some(&user))
        .await?
        .iter()
        .filter(|f| owners.contains(&f.owner))
        .map(RepoCard::from)
        .collect();
    html(ForkPage {
        repo: RepoView::new(&state, &repo, access, Some(&user), "code", "").await?,
        page: Page::new(&state, Some(user), format!("Fork {}/{}", repo.owner, repo.name)),
        owner: owners.first().cloned().unwrap_or_default(),
        owners,
        name: repo.name.clone(),
        error: None,
        existing,
    })
}

#[derive(Deserialize)]
pub struct ForkForm {
    owner: String,
    name: String,
}

pub async fn create(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name)): Path<(String, String)>,
    Form(f): Form<ForkForm>,
) -> AppResult {
    let (repo, access) = load(&state, Some(&user), &owner, &name).await?;
    let target = state.core.user_by_name(&f.owner).await?.ok_or(AppError::NotFound)?;
    if !state.core.can_create_repo_in(&target, &user).await? {
        return Err(AppError::Forbidden);
    }
    match state.core.fork_repo(&repo, &target, &f.name).await {
        Ok(fork) => {
            state.core.set_watching(user.id, fork.id, true).await?;
            // Everyone who can read the fork reads this in the feed, so a
            // private parent isn't named in it.
            let parent = (!repo.is_private).then(|| format!("{}/{}", repo.owner, repo.name));
            state.core.repo_created_event(fork.id, &user, parent.as_deref()).await?;
            Ok(Redirect::to(&format!("/{}/{}", fork.owner, fork.name)).into_response())
        }
        Err(keel_core::Error::Invalid(msg)) => html(ForkPage {
            repo: RepoView::new(&state, &repo, access, Some(&user), "code", "").await?,
            owners: possible_owners(&state, &user).await?.into_iter().filter(|o| *o != repo.owner).collect(),
            page: Page::new(&state, Some(user), format!("Fork {}/{}", repo.owner, repo.name)),
            owner: f.owner,
            name: f.name,
            error: Some(msg),
            existing: Vec::new(),
        }),
        Err(e) => Err(e.into()),
    }
}

#[derive(Template)]
#[template(path = "repo/forks.html")]
struct ForksPage {
    page: Page,
    repo: RepoView,
    forks: Vec<RepoCard>,
}

pub async fn list(
    State(state): State<AppState>,
    Viewer(viewer): Viewer,
    Path((owner, name)): Path<(String, String)>,
) -> AppResult {
    let (repo, access) = load(&state, viewer.as_ref(), &owner, &name).await?;
    let forks = state.core.forks_of(repo.id, viewer.as_ref()).await?;
    html(ForksPage {
        repo: RepoView::new(&state, &repo, access, viewer.as_ref(), "code", "").await?,
        page: Page::new(&state, viewer, format!("Forks · {}/{}", repo.owner, repo.name)),
        forks: forks.iter().map(RepoCard::from).collect(),
    })
}
