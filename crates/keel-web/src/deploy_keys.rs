//! Deploy keys: a repository's SSH keys for servers and CI.

use askama::Template;
use axum::Form;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Redirect};
use keel_core::{Access, DeployKey, Repo, User};
use serde::Deserialize;

use crate::error::{AppResult, html};
use crate::render::{date, relative_time};
use crate::repo::RepoView;
use crate::session::SignedIn;
use crate::{AppState, Page};

pub(crate) struct KeyView {
    pub id: i64,
    pub title: String,
    pub fingerprint: String,
    pub can_write: bool,
    pub added: String,
    pub added_by: Option<String>,
    pub last_used: Option<String>,
}

#[derive(Template)]
#[template(path = "repo/deploy_keys.html")]
struct KeysPage {
    page: Page,
    repo: RepoView,
    keys: Vec<KeyView>,
    error: Option<String>,
    /// The form, refilled after an error.
    title: String,
    key: String,
    write: bool,
    /// How to clone with a key, if SSH is on.
    ssh_url: Option<String>,
}

async fn page(state: &AppState, user: User, repo: &Repo, error: Option<String>, form: Option<KeyForm>) -> AppResult {
    let mut keys = Vec::new();
    for k in state.core.deploy_keys(repo.id).await? {
        keys.push(view(state, k).await?);
    }
    let form = form.unwrap_or_default();
    html(KeysPage {
        repo: RepoView::new(state, repo, Access::Admin, Some(&user), "settings", "").await?,
        page: Page::new(state, Some(user), format!("Deploy keys · {}/{}", repo.owner, repo.name)),
        keys,
        error,
        title: form.title,
        key: form.key,
        write: form.write.is_some(),
        ssh_url: state.core.config.ssh_clone_url(&repo.owner, &repo.name),
    })
}

async fn view(state: &AppState, k: DeployKey) -> AppResult<KeyView> {
    let added_by = match k.creator_id {
        Some(id) => state.core.user_by_id(id).await?.map(|u| u.username),
        None => None,
    };
    Ok(KeyView {
        id: k.id,
        title: k.title,
        fingerprint: k.fingerprint,
        can_write: k.can_write,
        added: date(k.created_at),
        added_by,
        last_used: k.last_used_at.map(relative_time),
    })
}

pub async fn index(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name)): Path<(String, String)>,
) -> AppResult {
    let repo = crate::repo::load_for_admin(&state, &user, &owner, &name).await?;
    page(&state, user, &repo, None, None).await
}

#[derive(Deserialize, Default)]
pub struct KeyForm {
    #[serde(default)]
    title: String,
    #[serde(default)]
    key: String,
    write: Option<String>,
}

pub async fn create(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name)): Path<(String, String)>,
    Form(f): Form<KeyForm>,
) -> AppResult {
    let repo = crate::repo::load_for_admin(&state, &user, &owner, &name).await?;
    match state.core.add_deploy_key(&repo, &user, &f.title, &f.key, f.write.is_some()).await {
        Ok(_) => Ok(Redirect::to(&format!("/{}/{}/settings/keys", repo.owner, repo.name)).into_response()),
        Err(keel_core::Error::Invalid(msg)) => page(&state, user, &repo, Some(msg), Some(f)).await,
        Err(e) => Err(e.into()),
    }
}

pub async fn delete(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> AppResult {
    let repo = crate::repo::load_for_admin(&state, &user, &owner, &name).await?;
    state.core.delete_deploy_key(repo.id, id).await?;
    Ok(Redirect::to(&format!("/{}/{}/settings/keys", repo.owner, repo.name)).into_response())
}
