//! Importing a repository from GitHub or Gitea (see `keel_core::imports`).

use askama::Template;
use axum::Form;
use axum::extract::State;
use axum::response::{IntoResponse, Redirect};
use keel_core::ImportOptions;
use serde::Deserialize;

use crate::error::{AppResult, html};
use crate::pages::possible_owners;
use crate::session::SignedIn;
use crate::{AppError, AppState, Page};

#[derive(Template)]
#[template(path = "import.html")]
struct ImportPage {
    page: Page,
    error: Option<String>,
    owners: Vec<String>,
    owner: String,
    url: String,
    name: String,
    description: String,
    is_private: bool,
    issues: bool,
    releases: bool,
    wiki: bool,
}

pub async fn form(State(state): State<AppState>, SignedIn(user): SignedIn) -> AppResult {
    html(ImportPage {
        owners: possible_owners(&state, &user).await?,
        owner: user.username.clone(),
        page: Page::new(&state, Some(user), "Import a repository"),
        error: None,
        url: String::new(),
        name: String::new(),
        description: String::new(),
        is_private: false,
        issues: true,
        releases: true,
        wiki: true,
    })
}

#[derive(Deserialize)]
pub struct ImportForm {
    url: String,
    #[serde(default)]
    token: String,
    owner: Option<String>,
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
    private: Option<String>,
    issues: Option<String>,
    releases: Option<String>,
    wiki: Option<String>,
}

pub async fn create(State(state): State<AppState>, SignedIn(user): SignedIn, Form(f): Form<ImportForm>) -> AppResult {
    let owner = match f.owner.as_deref() {
        Some(name) if !name.eq_ignore_ascii_case(&user.username) => {
            state.core.user_by_name(name).await?.ok_or(AppError::NotFound)?
        }
        _ => user.clone(),
    };
    if !state.core.can_create_repo_in(&owner, &user).await? {
        return Err(AppError::Forbidden);
    }
    let options = ImportOptions { issues: f.issues.is_some(), releases: f.releases.is_some(), wiki: f.wiki.is_some() };
    let created = async {
        let source = keel_core::import_source(&f.url)?;
        let name = if f.name.trim().is_empty() { source.name.clone() } else { f.name.trim().to_owned() };
        let repo = state
            .core
            .create_import(&owner, &user, &name, &f.description, f.private.is_some(), &source, &f.token, options)
            .await?;
        state.core.set_watching(user.id, repo.id, true).await?;
        state.core.repo_created_event(repo.id, &user, None).await?;
        Ok::<_, keel_core::Error>(repo)
    }
    .await;
    match created {
        Ok(repo) => Ok(Redirect::to(&format!("/{}/{}", repo.owner, repo.name)).into_response()),
        Err(keel_core::Error::Invalid(msg)) => html(ImportPage {
            owners: possible_owners(&state, &user).await?,
            owner: owner.username,
            page: Page::new(&state, Some(user), "Import a repository"),
            error: Some(msg),
            url: f.url,
            name: f.name,
            description: f.description,
            is_private: f.private.is_some(),
            issues: options.issues,
            releases: options.releases,
            wiki: options.wiki,
        }),
        Err(e) => Err(e.into()),
    }
}
