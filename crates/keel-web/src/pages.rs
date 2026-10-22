//! Pages outside a repository: home, sign in/up, profiles, settings.

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, header};
use axum::response::{IntoResponse, Redirect};
use keel_core::{OrgRole, Repo, User};
use serde::Deserialize;

use crate::error::{AppResult, html};
use crate::render::{date, relative_time};
use crate::session::{SignedIn, Viewer, clear_cookie, safe_next, set_cookie, token_from};
use crate::{AppError, AppState, Page};

pub(crate) struct RepoCard {
    pub owner: String,
    pub name: String,
    pub description: String,
    pub is_private: bool,
    pub updated: String,
}

impl From<&Repo> for RepoCard {
    fn from(r: &Repo) -> Self {
        Self {
            owner: r.owner.clone(),
            name: r.name.clone(),
            description: r.description.clone(),
            is_private: r.is_private,
            updated: relative_time(r.updated_at),
        }
    }
}

fn cards(repos: &[Repo]) -> Vec<RepoCard> {
    repos.iter().map(RepoCard::from).collect()
}

#[derive(Template)]
#[template(path = "home.html")]
struct HomePage {
    page: Page,
    first_run: bool,
    mine: Vec<RepoCard>,
    recent: Vec<RepoCard>,
}

pub async fn home(State(state): State<AppState>, Viewer(viewer): Viewer) -> AppResult {
    let mine = match &viewer {
        Some(u) => cards(&state.core.repos_for(u).await?),
        None => Vec::new(),
    };
    let recent = cards(&state.core.recent_public_repos(30).await?);
    let first_run = !state.core.has_users().await?;
    let site = state.core.config.site_name.clone();
    html(HomePage {
        page: Page::new(&state, viewer, site),
        first_run,
        mine,
        recent,
    })
}

#[derive(Template)]
#[template(path = "login.html")]
struct LoginPage {
    page: Page,
    error: Option<String>,
    login: String,
    next: String,
}

#[derive(Deserialize)]
pub struct NextQuery {
    next: Option<String>,
}

pub async fn login_form(
    State(state): State<AppState>,
    Viewer(viewer): Viewer,
    Query(q): Query<NextQuery>,
) -> AppResult {
    let next = safe_next(q.next.as_deref()).to_owned();
    if viewer.is_some() {
        return Ok(Redirect::to(&next).into_response());
    }
    html(LoginPage {
        page: Page::new(&state, None, "Sign in"),
        error: None,
        login: String::new(),
        next,
    })
}

#[derive(Deserialize)]
pub struct LoginForm {
    login: String,
    password: String,
    next: Option<String>,
}

pub async fn login(State(state): State<AppState>, Form(f): Form<LoginForm>) -> AppResult {
    let next = safe_next(f.next.as_deref()).to_owned();
    match state.core.authenticate(&f.login, &f.password).await? {
        Some(user) => {
            let token = state.core.create_session(user.id).await?;
            Ok((
                [(header::SET_COOKIE, set_cookie(&state, &token))],
                Redirect::to(&next),
            )
                .into_response())
        }
        None => html(LoginPage {
            page: Page::new(&state, None, "Sign in"),
            error: Some("Incorrect username or password.".into()),
            login: f.login,
            next,
        }),
    }
}

pub async fn logout(State(state): State<AppState>, headers: HeaderMap) -> AppResult {
    if let Some(token) = token_from(&headers) {
        state.core.delete_session(token).await?;
    }
    Ok(([(header::SET_COOKIE, clear_cookie())], Redirect::to("/")).into_response())
}

#[derive(Template)]
#[template(path = "register.html")]
struct RegisterPage {
    page: Page,
    error: Option<String>,
    username: String,
    email: String,
    first_user: bool,
    allowed: bool,
}

async fn register_page(
    state: &AppState,
    error: Option<String>,
    username: String,
    email: String,
) -> AppResult {
    let first_user = !state.core.has_users().await?;
    html(RegisterPage {
        page: Page::new(state, None, "Create an account"),
        error,
        username,
        email,
        first_user,
        allowed: first_user || state.core.config.open_registration,
    })
}

pub async fn register_form(State(state): State<AppState>, Viewer(viewer): Viewer) -> AppResult {
    if viewer.is_some() {
        return Ok(Redirect::to("/").into_response());
    }
    register_page(&state, None, String::new(), String::new()).await
}

#[derive(Deserialize)]
pub struct RegisterForm {
    username: String,
    email: String,
    password: String,
}

pub async fn register(State(state): State<AppState>, Form(f): Form<RegisterForm>) -> AppResult {
    match state.core.register(f.username.trim(), &f.email, &f.password).await {
        Ok(user) => {
            let token = state.core.create_session(user.id).await?;
            Ok((
                [(header::SET_COOKIE, set_cookie(&state, &token))],
                Redirect::to("/"),
            )
                .into_response())
        }
        Err(keel_core::Error::Invalid(msg)) => {
            register_page(&state, Some(msg), f.username, f.email).await
        }
        Err(e) => Err(e.into()),
    }
}

#[derive(Template)]
#[template(path = "new_repo.html")]
struct NewRepoPage {
    page: Page,
    error: Option<String>,
    /// Possible owners: the user, then their organizations.
    owners: Vec<String>,
    owner: String,
    name: String,
    description: String,
    is_private: bool,
}

pub(crate) async fn possible_owners(state: &AppState, user: &User) -> AppResult<Vec<String>> {
    let mut owners = vec![user.username.clone()];
    owners.extend(state.core.orgs_of(user.id).await?.into_iter().map(|(o, _)| o.username));
    Ok(owners)
}

#[derive(Deserialize)]
pub struct OwnerQuery {
    owner: Option<String>,
}

pub async fn new_repo_form(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Query(q): Query<OwnerQuery>,
) -> AppResult {
    let owners = possible_owners(&state, &user).await?;
    let owner = q
        .owner
        .and_then(|o| owners.iter().find(|x| x.eq_ignore_ascii_case(&o)).cloned())
        .unwrap_or_else(|| user.username.clone());
    html(NewRepoPage {
        page: Page::new(&state, Some(user), "New repository"),
        error: None,
        owners,
        owner,
        name: String::new(),
        description: String::new(),
        is_private: false,
    })
}

#[derive(Deserialize)]
pub struct NewRepoForm {
    owner: Option<String>,
    name: String,
    #[serde(default)]
    description: String,
    private: Option<String>,
}

pub async fn new_repo(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Form(f): Form<NewRepoForm>,
) -> AppResult {
    let is_private = f.private.is_some();
    let owner = match f.owner.as_deref() {
        Some(name) if !name.eq_ignore_ascii_case(&user.username) => {
            state.core.user_by_name(name).await?.ok_or(AppError::NotFound)?
        }
        _ => user.clone(),
    };
    if !state.core.can_create_repo_in(&owner, &user).await? {
        return Err(AppError::Forbidden);
    }
    match state.core.create_repo(&owner, &f.name, &f.description, is_private).await {
        Ok(repo) => Ok(Redirect::to(&format!("/{}/{}", repo.owner, repo.name)).into_response()),
        Err(keel_core::Error::Invalid(msg)) => html(NewRepoPage {
            owners: possible_owners(&state, &user).await?,
            owner: owner.username,
            page: Page::new(&state, Some(user), "New repository"),
            error: Some(msg),
            name: f.name,
            description: f.description,
            is_private,
        }),
        Err(e) => Err(e.into()),
    }
}

#[derive(Template)]
#[template(path = "user.html")]
struct UserPage {
    page: Page,
    username: String,
    joined: String,
    is_self: bool,
    repos: Vec<RepoCard>,
    orgs: Vec<String>,
}

#[derive(Template)]
#[template(path = "org.html")]
struct OrgPage {
    page: Page,
    name: String,
    created: String,
    /// The viewer may create repositories here.
    is_member: bool,
    is_owner: bool,
    repos: Vec<RepoCard>,
    members: Vec<(String, &'static str)>,
}

pub async fn user(
    State(state): State<AppState>,
    Viewer(viewer): Viewer,
    Path(owner): Path<String>,
) -> AppResult {
    let user = state.core.user_by_name(&owner).await?.ok_or(AppError::NotFound)?;
    if user.username != owner {
        return Ok(Redirect::permanent(&format!("/{}", user.username)).into_response());
    }
    let repos = cards(&state.core.repos_of(user.id, viewer.as_ref()).await?);
    if user.is_org() {
        let members = state.core.org_members(user.id).await?;
        let role = viewer.as_ref().and_then(|v| members.iter().find(|(m, _)| m.id == v.id).map(|(_, r)| *r));
        return html(OrgPage {
            is_member: role.is_some(),
            is_owner: role == Some(OrgRole::Owner) || viewer.as_ref().is_some_and(|v| v.is_admin),
            page: Page::new(&state, viewer, user.username.clone()),
            created: date(user.created_at),
            name: user.username,
            repos,
            members: members.into_iter().map(|(u, r)| (u.username, r.as_str())).collect(),
        });
    }
    let orgs = state.core.orgs_of(user.id).await?.into_iter().map(|(o, _)| o.username).collect();
    html(UserPage {
        is_self: viewer.as_ref().is_some_and(|v| v.id == user.id),
        page: Page::new(&state, viewer, user.username.clone()),
        joined: date(user.created_at),
        username: user.username,
        repos,
        orgs,
    })
}
