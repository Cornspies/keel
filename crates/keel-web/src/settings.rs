//! Account settings: email and password, SSH keys, access tokens.

use askama::Template;
use axum::Form;
use axum::extract::{Path, State};
use axum::http::header;
use axum::response::{IntoResponse, Redirect};
use keel_core::{AccessToken, GpgKey, SshKey, User};
use serde::Deserialize;

use crate::error::{AppResult, html};
use crate::render::{date, relative_time};
use crate::session::{SignedIn, set_cookie};
use crate::{AppState, Page};

pub async fn index() -> Redirect {
    Redirect::to("/settings/account")
}

// ---------------------------------------------------------------------------
// Account
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "settings/account.html")]
struct AccountPage {
    page: Page,
    nav: &'static str,
    email: String,
    verified: bool,
    mail_enabled: bool,
    email_error: Option<String>,
    password_error: Option<String>,
    notice: Option<&'static str>,
}

fn account_page(
    state: &AppState,
    user: User,
    email_error: Option<String>,
    password_error: Option<String>,
    notice: Option<&'static str>,
) -> AppResult {
    html(AccountPage {
        email: user.email.clone(),
        verified: user.email_verified,
        mail_enabled: state.core.mail_enabled(),
        page: Page::new(state, Some(user), "Account settings"),
        nav: "account",
        email_error,
        password_error,
        notice,
    })
}

#[derive(Deserialize)]
pub struct NoticeQuery {
    saved: Option<String>,
}

pub async fn account(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    axum::extract::Query(q): axum::extract::Query<NoticeQuery>,
) -> AppResult {
    let notice = match q.saved.as_deref() {
        Some("email") if state.core.mail_enabled() => {
            Some("Your email address was updated. We sent a link to it to verify it.")
        }
        Some("email") => Some("Your email address was updated."),
        Some("verify") => Some("We sent a verification link to your email address. It works for 7 days."),
        Some("verify-wait") => Some("We just sent a verification link; check your email (and spam folder)."),
        Some("password") => Some("Your password was changed. Other sessions were signed out."),
        _ => None,
    };
    account_page(&state, user, None, None, notice)
}

#[derive(Deserialize)]
pub struct EmailForm {
    email: String,
}

pub async fn change_email(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Form(f): Form<EmailForm>,
) -> AppResult {
    match state.core.change_email(user.id, &f.email).await {
        Ok(()) => {
            if let Some(user) = state.core.user_by_id(user.id).await? {
                state.core.send_verification(&user).await?;
            }
            Ok(Redirect::to("/settings/account?saved=email").into_response())
        }
        Err(keel_core::Error::Invalid(msg)) => account_page(&state, user, Some(msg), None, None),
        Err(e) => Err(e.into()),
    }
}

#[derive(Deserialize)]
pub struct PasswordForm {
    current: String,
    new: String,
    confirm: String,
}

pub async fn change_password(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Form(f): Form<PasswordForm>,
) -> AppResult {
    if f.new != f.confirm {
        let msg = "The new passwords don’t match.".to_owned();
        return account_page(&state, user, None, Some(msg), None);
    }
    match state.core.change_password(&user, &f.current, &f.new).await {
        Ok(()) => {
            // All sessions were revoked; keep this browser signed in.
            let token = state.core.create_session(user.id).await?;
            Ok((
                [(header::SET_COOKIE, set_cookie(&state, &token))],
                Redirect::to("/settings/account?saved=password"),
            )
                .into_response())
        }
        Err(keel_core::Error::Invalid(msg)) => account_page(&state, user, None, Some(msg), None),
        Err(e) => Err(e.into()),
    }
}

pub async fn send_verification(State(state): State<AppState>, SignedIn(user): SignedIn) -> AppResult {
    let to = if state.core.send_verification(&user).await? { "verify" } else { "verify-wait" };
    Ok(Redirect::to(&format!("/settings/account?saved={to}")).into_response())
}

// ---------------------------------------------------------------------------
// Notifications
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "settings/notifications.html")]
struct NotificationsPage {
    page: Page,
    nav: &'static str,
    setting: String,
    email: String,
    verified: bool,
    mail_enabled: bool,
    notice: Option<&'static str>,
}

pub async fn notifications(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    axum::extract::Query(q): axum::extract::Query<NoticeQuery>,
) -> AppResult {
    html(NotificationsPage {
        setting: user.email_notifications.clone(),
        email: user.email.clone(),
        verified: user.email_verified,
        mail_enabled: state.core.mail_enabled(),
        notice: q.saved.is_some().then_some("Saved."),
        page: Page::new(&state, Some(user), "Notification settings"),
        nav: "notifications",
    })
}

#[derive(Deserialize)]
pub struct NotificationsForm {
    email: String,
}

pub async fn save_notifications(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Form(f): Form<NotificationsForm>,
) -> AppResult {
    state.core.set_email_notifications(user.id, &f.email).await?;
    Ok(Redirect::to("/settings/notifications?saved=1").into_response())
}

// ---------------------------------------------------------------------------
// SSH keys
// ---------------------------------------------------------------------------

struct KeyView {
    id: i64,
    name: String,
    fingerprint: String,
    added: String,
    last_used: Option<String>,
}

impl From<SshKey> for KeyView {
    fn from(k: SshKey) -> Self {
        Self {
            id: k.id,
            name: k.name,
            fingerprint: k.fingerprint,
            added: date(k.created_at),
            last_used: k.last_used_at.map(relative_time),
        }
    }
}

struct GpgKeyView {
    id: i64,
    key_id: String,
    emails: Vec<String>,
    added: String,
    expires: Option<String>,
}

impl From<GpgKey> for GpgKeyView {
    fn from(k: GpgKey) -> Self {
        Self {
            id: k.id,
            key_id: k.key_id().to_owned(),
            emails: k.emails().map(str::to_owned).collect(),
            added: date(k.created_at),
            expires: k.expires_at.map(date),
        }
    }
}

#[derive(Template)]
#[template(path = "settings/keys.html")]
struct KeysPage {
    page: Page,
    nav: &'static str,
    error: Option<String>,
    keys: Vec<KeyView>,
    name: String,
    key: String,
    ssh_enabled: bool,
    gpg_keys: Vec<GpgKeyView>,
    gpg_error: Option<String>,
    gpg_key: String,
}

/// What the forms held, to show them again with an error.
#[derive(Default)]
struct KeyForms {
    error: Option<String>,
    name: String,
    key: String,
    gpg_error: Option<String>,
    gpg_key: String,
}

async fn keys_page(state: &AppState, user: keel_core::User, forms: KeyForms) -> AppResult {
    let keys = state.core.ssh_keys(user.id).await?.into_iter().map(KeyView::from).collect();
    let gpg_keys = state.core.gpg_keys(user.id).await?.into_iter().map(GpgKeyView::from).collect();
    html(KeysPage {
        page: Page::new(state, Some(user), "SSH and GPG keys"),
        nav: "keys",
        error: forms.error,
        keys,
        name: forms.name,
        key: forms.key,
        ssh_enabled: state.core.config.ssh.is_some(),
        gpg_keys,
        gpg_error: forms.gpg_error,
        gpg_key: forms.gpg_key,
    })
}

pub async fn keys(State(state): State<AppState>, SignedIn(user): SignedIn) -> AppResult {
    keys_page(&state, user, KeyForms::default()).await
}

#[derive(Deserialize)]
pub struct GpgKeyForm {
    key: String,
}

pub async fn add_gpg_key(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Form(f): Form<GpgKeyForm>,
) -> AppResult {
    match state.core.add_gpg_key(user.id, &f.key).await {
        Ok(_) => Ok(Redirect::to("/settings/keys#gpg").into_response()),
        Err(keel_core::Error::Invalid(msg)) => {
            keys_page(&state, user, KeyForms { gpg_error: Some(msg), gpg_key: f.key, ..Default::default() }).await
        }
        Err(e) => Err(e.into()),
    }
}

pub async fn delete_gpg_key(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path(id): Path<i64>,
) -> AppResult {
    state.core.delete_gpg_key(user.id, id).await?;
    Ok(Redirect::to("/settings/keys#gpg").into_response())
}

#[derive(Deserialize)]
pub struct KeyForm {
    #[serde(default)]
    name: String,
    key: String,
}

pub async fn add_key(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Form(f): Form<KeyForm>,
) -> AppResult {
    match state.core.add_ssh_key(user.id, &f.name, &f.key).await {
        Ok(_) => Ok(Redirect::to("/settings/keys").into_response()),
        Err(keel_core::Error::Invalid(msg)) => {
            keys_page(&state, user, KeyForms { error: Some(msg), name: f.name, key: f.key, ..Default::default() }).await
        }
        Err(e) => Err(e.into()),
    }
}

pub async fn delete_key(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path(id): Path<i64>,
) -> AppResult {
    state.core.delete_ssh_key(user.id, id).await?;
    Ok(Redirect::to("/settings/keys").into_response())
}


// ---------------------------------------------------------------------------
// Access tokens
// ---------------------------------------------------------------------------

struct TokenView {
    id: i64,
    name: String,
    hint: String,
    can_write: bool,
    created: String,
    last_used: Option<String>,
    expires: Option<String>,
    expired: bool,
}

impl From<AccessToken> for TokenView {
    fn from(t: AccessToken) -> Self {
        Self {
            expired: t.is_expired(),
            id: t.id,
            name: t.name,
            hint: t.hint,
            can_write: t.can_write,
            created: date(t.created_at),
            last_used: t.last_used_at.map(relative_time),
            expires: t.expires_at.map(date),
        }
    }
}

#[derive(Template)]
#[template(path = "settings/tokens.html")]
struct TokensPage {
    page: Page,
    nav: &'static str,
    tokens: Vec<TokenView>,
    error: Option<String>,
    /// A token just created, shown once.
    created: Option<String>,
    username: String,
    clone_example: String,
}

async fn tokens_page(state: &AppState, user: User, error: Option<String>, created: Option<String>) -> AppResult {
    let tokens = state.core.tokens(user.id).await?.into_iter().map(TokenView::from).collect();
    html(TokensPage {
        username: user.username.clone(),
        clone_example: state.core.config.http_clone_url(&user.username, "<repo>"),
        page: Page::new(state, Some(user), "Access tokens"),
        nav: "tokens",
        tokens,
        error,
        created,
    })
}

pub async fn tokens(State(state): State<AppState>, SignedIn(user): SignedIn) -> AppResult {
    tokens_page(&state, user, None, None).await
}

#[derive(Deserialize)]
pub struct TokenForm {
    name: String,
    write: Option<String>,
    /// Days until expiry; empty for never.
    #[serde(default)]
    expires: String,
}

pub async fn create_token(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Form(f): Form<TokenForm>,
) -> AppResult {
    let expires = f.expires.trim().parse::<i64>().ok().filter(|d| *d > 0);
    match state.core.create_token(user.id, &f.name, f.write.is_some(), expires).await {
        // Rendered directly rather than redirecting: the secret must not
        // end up in a URL or be retrievable again.
        Ok((_, secret)) => tokens_page(&state, user, None, Some(secret)).await,
        Err(keel_core::Error::Invalid(msg)) => tokens_page(&state, user, Some(msg), None).await,
        Err(e) => Err(e.into()),
    }
}

pub async fn delete_token(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path(id): Path<i64>,
) -> AppResult {
    state.core.delete_token(user.id, id).await?;
    Ok(Redirect::to("/settings/tokens").into_response())
}
