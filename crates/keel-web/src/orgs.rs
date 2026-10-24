//! Creating and administering organizations.

use askama::Template;
use axum::Form;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Redirect};
use keel_core::{OrgRole, User};
use serde::Deserialize;

use crate::error::{AppResult, html};
use crate::session::SignedIn;
use crate::{AppError, AppState, Page};

#[derive(Template)]
#[template(path = "org_new.html")]
struct NewOrgPage {
    page: Page,
    error: Option<String>,
    name: String,
}

pub async fn new_form(State(state): State<AppState>, SignedIn(user): SignedIn) -> AppResult {
    html(NewOrgPage {
        page: Page::new(&state, Some(user), "New organization"),
        error: None,
        name: String::new(),
    })
}

#[derive(Deserialize)]
pub struct NewOrgForm {
    name: String,
}

pub async fn create(State(state): State<AppState>, SignedIn(user): SignedIn, Form(f): Form<NewOrgForm>) -> AppResult {
    match state.core.create_org(&user, &f.name).await {
        Ok(org) => Ok(Redirect::to(&format!("/{}", org.username)).into_response()),
        Err(keel_core::Error::Invalid(msg)) => html(NewOrgPage {
            page: Page::new(&state, Some(user), "New organization"),
            error: Some(msg),
            name: f.name,
        }),
        Err(e) => Err(e.into()),
    }
}

/// The organization, if `user` owns it (or is a site admin).
pub(crate) async fn owned_org(state: &AppState, user: &User, name: &str) -> AppResult<User> {
    let org = state
        .core
        .user_by_name(name)
        .await?
        .filter(User::is_org)
        .ok_or(AppError::NotFound)?;
    let role = state.core.org_role(org.id, user.id).await?;
    if role != Some(OrgRole::Owner) && !user.is_admin {
        return Err(AppError::Forbidden);
    }
    Ok(org)
}

struct MemberView {
    id: i64,
    username: String,
    role: &'static str,
}

#[derive(Template)]
#[template(path = "org_settings.html")]
struct OrgSettingsPage {
    page: Page,
    name: String,
    members: Vec<MemberView>,
    error: Option<String>,
}

async fn settings_page(state: &AppState, user: User, org: &User, error: Option<String>) -> AppResult {
    let members = state
        .core
        .org_members(org.id)
        .await?
        .into_iter()
        .map(|(u, r)| MemberView {
            id: u.id,
            username: u.username,
            role: r.as_str(),
        })
        .collect();
    html(OrgSettingsPage {
        page: Page::new(state, Some(user), format!("Settings · {}", org.username)),
        name: org.username.clone(),
        members,
        error,
    })
}

pub async fn settings(State(state): State<AppState>, SignedIn(user): SignedIn, Path(name): Path<String>) -> AppResult {
    let org = owned_org(&state, &user, &name).await?;
    settings_page(&state, user, &org, None).await
}

#[derive(Deserialize)]
pub struct MemberForm {
    username: String,
    role: String,
}

pub async fn set_member(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path(name): Path<String>,
    Form(f): Form<MemberForm>,
) -> AppResult {
    let org = owned_org(&state, &user, &name).await?;
    let role = OrgRole::parse(&f.role).ok_or_else(|| AppError::BadRequest("Unknown role".into()))?;
    match state.core.set_org_member(&org, &f.username, role).await {
        Ok(()) => Ok(Redirect::to(&format!("/org/{}/settings", org.username)).into_response()),
        Err(keel_core::Error::Invalid(msg)) => settings_page(&state, user, &org, Some(msg)).await,
        Err(e) => Err(e.into()),
    }
}

pub async fn remove_member(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((name, member_id)): Path<(String, i64)>,
) -> AppResult {
    let org = owned_org(&state, &user, &name).await?;
    match state.core.remove_org_member(&org, member_id).await {
        Ok(()) if member_id == user.id => Ok(Redirect::to(&format!("/{}", org.username)).into_response()),
        Ok(()) => Ok(Redirect::to(&format!("/org/{}/settings", org.username)).into_response()),
        Err(keel_core::Error::Invalid(msg)) => settings_page(&state, user, &org, Some(msg)).await,
        Err(e) => Err(e.into()),
    }
}

pub async fn delete(State(state): State<AppState>, SignedIn(user): SignedIn, Path(name): Path<String>) -> AppResult {
    let org = owned_org(&state, &user, &name).await?;
    match state.core.delete_org(&org).await {
        Ok(()) => Ok(Redirect::to("/").into_response()),
        Err(keel_core::Error::Invalid(msg)) => settings_page(&state, user, &org, Some(msg)).await,
        Err(e) => Err(e.into()),
    }
}
