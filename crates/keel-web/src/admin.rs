//! The site administration pages: an overview with instance settings, and
//! lists of accounts and repositories.

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Redirect};
use keel_core::{AdminUser, Repo, SiteSettings, SiteStats, User};
use serde::Deserialize;

use crate::error::{AppResult, html};
use crate::render::{datetime, relative_time};
use crate::session::SignedIn;
use crate::{AppError, AppState, Page};

const PER_PAGE: i64 = 50;

fn require_admin(user: &User) -> AppResult<()> {
    if user.is_admin { Ok(()) } else { Err(AppError::NotFound) }
}

#[derive(Deserialize)]
pub struct ListQuery {
    #[serde(default)]
    q: String,
    page: Option<i64>,
    /// Set after saving the settings.
    saved: Option<String>,
    /// Set after inviting someone: their username.
    invited: Option<String>,
}

fn page_url(base: &str, q: &str, page: i64) -> String {
    let mut s = form_urlencoded::Serializer::new(String::new());
    if !q.is_empty() {
        s.append_pair("q", q);
    }
    s.append_pair("page", &page.to_string());
    format!("{base}?{}", s.finish())
}

// ---------------------------------------------------------------------------
// Overview and settings
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "admin/overview.html")]
struct OverviewPage {
    page: Page,
    nav: &'static str,
    stats: SiteStats,
    settings: SiteSettings,
    /// `--closed-registration` was given, so the setting has no effect.
    registration_forced_closed: bool,
    version: &'static str,
    mail_enabled: bool,
    ssh_enabled: bool,
    data_dir: String,
    error: Option<String>,
    notice: Option<String>,
}

async fn overview_page(state: &AppState, user: User, settings: SiteSettings, error: Option<String>, notice: Option<String>) -> AppResult {
    let config = &state.core.config;
    html(OverviewPage {
        stats: state.core.site_stats().await?,
        settings,
        registration_forced_closed: !config.open_registration,
        version: env!("CARGO_PKG_VERSION"),
        mail_enabled: state.core.mail_enabled(),
        ssh_enabled: config.ssh.is_some(),
        data_dir: config.data_dir.display().to_string(),
        page: Page::new(state, Some(user), "Site administration"),
        nav: "overview",
        error,
        notice,
    })
}

pub async fn overview(State(state): State<AppState>, SignedIn(user): SignedIn, Query(q): Query<ListQuery>) -> AppResult {
    require_admin(&user)?;
    let settings = state.core.site_settings();
    overview_page(&state, user, settings, None, q.saved.map(|_| "Saved.".to_owned())).await
}

#[derive(Deserialize)]
pub struct SettingsForm {
    registration_closed: Option<String>,
    #[serde(default)]
    banner: String,
}

pub async fn save_settings(State(state): State<AppState>, SignedIn(user): SignedIn, Form(f): Form<SettingsForm>) -> AppResult {
    require_admin(&user)?;
    let settings = SiteSettings { registration_closed: f.registration_closed.is_some(), banner: f.banner };
    match state.core.save_site_settings(&settings).await {
        Ok(()) => Ok(Redirect::to("/admin?saved=1").into_response()),
        Err(keel_core::Error::Invalid(msg)) => overview_page(&state, user, settings, Some(msg), None).await,
        Err(e) => Err(e.into()),
    }
}

// ---------------------------------------------------------------------------
// Accounts
// ---------------------------------------------------------------------------

pub(crate) struct UserRow {
    pub id: i64,
    pub username: String,
    pub email: String,
    pub verified: bool,
    pub is_admin: bool,
    pub disabled: bool,
    pub two_factor: bool,
    pub is_self: bool,
    pub repos: i64,
    pub joined: String,
    pub joined_full: String,
    pub last_seen: String,
}

#[derive(Template)]
#[template(path = "admin/users.html")]
struct UsersPage {
    page: Page,
    nav: &'static str,
    users: Vec<UserRow>,
    q: String,
    newer_url: Option<String>,
    older_url: Option<String>,
    error: Option<String>,
    notice: Option<String>,
    /// The create-account form after an error.
    new_username: String,
    new_email: String,
}

async fn users_page(
    state: &AppState,
    user: User,
    q: String,
    page_no: i64,
    error: Option<String>,
    new_username: String,
    new_email: String,
) -> AppResult {
    users_page_with(state, user, q, page_no, error, None, new_username, new_email).await
}

#[allow(clippy::too_many_arguments)]
async fn users_page_with(
    state: &AppState,
    user: User,
    q: String,
    page_no: i64,
    error: Option<String>,
    notice: Option<String>,
    new_username: String,
    new_email: String,
) -> AppResult {
    let (users, more) = state.core.admin_users(&q, (page_no - 1) * PER_PAGE, PER_PAGE).await?;
    let rows = users
        .into_iter()
        .map(|AdminUser { user: u, repos, last_seen }| UserRow {
            is_self: u.id == user.id,
            id: u.id,
            username: u.username,
            email: u.email,
            verified: u.email_verified,
            is_admin: u.is_admin,
            disabled: u.disabled,
            two_factor: u.totp_enabled,
            repos,
            joined: relative_time(u.created_at),
            joined_full: datetime(u.created_at),
            last_seen: last_seen.map(relative_time).unwrap_or_else(|| "never".into()),
        })
        .collect();
    html(UsersPage {
        page: Page::new(state, Some(user), "Accounts · Site administration"),
        nav: "users",
        users: rows,
        newer_url: (page_no > 1).then(|| page_url("/admin/users", &q, page_no - 1)),
        older_url: more.then(|| page_url("/admin/users", &q, page_no + 1)),
        q,
        error,
        notice,
        new_username,
        new_email,
    })
}

pub async fn users(State(state): State<AppState>, SignedIn(user): SignedIn, Query(q): Query<ListQuery>) -> AppResult {
    require_admin(&user)?;
    let notice = q.invited.map(|name| format!("Invited {name}: they got an email with a link to choose their password, which works for a week."));
    users_page_with(&state, user, q.q, q.page.unwrap_or(1).max(1), None, notice, String::new(), String::new()).await
}

#[derive(Deserialize)]
pub struct NewUserForm {
    username: String,
    email: String,
    password: String,
}

pub async fn create_user(State(state): State<AppState>, SignedIn(user): SignedIn, Form(f): Form<NewUserForm>) -> AppResult {
    require_admin(&user)?;
    match state.core.admin_create_user(f.username.trim(), &f.email, &f.password).await {
        Ok(created) => {
            state.core.send_verification(&created).await?;
            Ok(Redirect::to("/admin/users").into_response())
        }
        Err(keel_core::Error::Invalid(msg)) => {
            users_page(&state, user, String::new(), 1, Some(msg), f.username, f.email).await
        }
        Err(e) => Err(e.into()),
    }
}

#[derive(Deserialize)]
pub struct InviteForm {
    username: String,
    email: String,
}

pub async fn invite_user(State(state): State<AppState>, SignedIn(user): SignedIn, Form(f): Form<InviteForm>) -> AppResult {
    require_admin(&user)?;
    match state.core.invite_user(&user, f.username.trim(), &f.email).await {
        Ok(invited) => Ok(Redirect::to(&format!("/admin/users?invited={}", invited.username)).into_response()),
        Err(keel_core::Error::Invalid(msg)) => {
            users_page(&state, user, String::new(), 1, Some(msg), f.username, f.email).await
        }
        Err(e) => Err(e.into()),
    }
}

#[derive(Deserialize)]
pub struct ToggleForm {
    /// `on` or `off`.
    to: String,
}

async fn target(state: &AppState, id: i64) -> AppResult<User> {
    state.core.user_by_id(id).await?.filter(|u| !u.is_org()).ok_or(AppError::NotFound)
}

/// Goes back to the list after an account action, or explains why it
/// couldn't be done.
async fn act(state: &AppState, user: User, result: keel_core::Result<()>) -> AppResult {
    match result {
        Ok(()) => Ok(Redirect::to("/admin/users").into_response()),
        Err(keel_core::Error::Invalid(msg)) => {
            users_page(state, user, String::new(), 1, Some(msg), String::new(), String::new()).await
        }
        Err(e) => Err(e.into()),
    }
}

pub async fn set_admin(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path(id): Path<i64>,
    Form(f): Form<ToggleForm>,
) -> AppResult {
    require_admin(&user)?;
    let t = target(&state, id).await?;
    let result = state.core.set_site_admin(&t, f.to == "on").await;
    act(&state, user, result).await
}

pub async fn set_disabled(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path(id): Path<i64>,
    Form(f): Form<ToggleForm>,
) -> AppResult {
    require_admin(&user)?;
    let t = target(&state, id).await?;
    let result = state.core.set_disabled(&user, &t, f.to == "on").await;
    act(&state, user, result).await
}

/// For people who lost their authenticator and recovery codes.
pub async fn disable_two_factor(State(state): State<AppState>, SignedIn(user): SignedIn, Path(id): Path<i64>) -> AppResult {
    require_admin(&user)?;
    let t = target(&state, id).await?;
    let result = state.core.disable_totp(t.id).await;
    act(&state, user, result).await
}

pub async fn delete_user(State(state): State<AppState>, SignedIn(user): SignedIn, Path(id): Path<i64>) -> AppResult {
    require_admin(&user)?;
    let t = target(&state, id).await?;
    let result = state.core.delete_user(&user, &t).await;
    act(&state, user, result).await
}

// ---------------------------------------------------------------------------
// Repositories
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "admin/repos.html")]
struct ReposPage {
    page: Page,
    nav: &'static str,
    repos: Vec<(Repo, String, String)>,
    q: String,
    newer_url: Option<String>,
    older_url: Option<String>,
}

pub async fn repos(State(state): State<AppState>, SignedIn(user): SignedIn, Query(q): Query<ListQuery>) -> AppResult {
    require_admin(&user)?;
    let page_no = q.page.unwrap_or(1).max(1);
    let (repos, more) = state.core.admin_repos(&q.q, (page_no - 1) * PER_PAGE, PER_PAGE).await?;
    html(ReposPage {
        page: Page::new(&state, Some(user), "Repositories · Site administration"),
        nav: "repos",
        repos: repos
            .into_iter()
            .map(|r| {
                let (when, full) = (relative_time(r.updated_at), datetime(r.updated_at));
                (r, when, full)
            })
            .collect(),
        newer_url: (page_no > 1).then(|| page_url("/admin/repos", &q.q, page_no - 1)),
        older_url: more.then(|| page_url("/admin/repos", &q.q, page_no + 1)),
        q: q.q,
    })
}
