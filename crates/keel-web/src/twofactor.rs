//! Two-factor authentication: the second sign-in step, and turning it on
//! and off in the settings.

use askama::Template;
use axum::Form;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::{IntoResponse, Redirect};
use base64::Engine;
use keel_core::{SecondStep, User, totp_uri};
use serde::Deserialize;

use crate::error::{AppResult, html};
use crate::session::{SignedIn, safe_next, set_cookie};
use crate::{AppState, Page};

const COOKIE: &str = "keel_2fa";

fn challenge_cookie(state: &AppState, token: &str, max_age: i64) -> HeaderValue {
    let secure = if state.core.config.base_url.starts_with("https://") { "; Secure" } else { "" };
    HeaderValue::from_str(&format!("{COOKIE}={token}; Path=/login; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}"))
        .expect("token is base64url")
}

fn challenge_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|c| c.trim().strip_prefix(COOKIE)?.strip_prefix('='))
}

/// After the password was right: signs in, or asks for the second factor.
pub(crate) async fn sign_in(state: &AppState, user: &User, next: &str) -> AppResult {
    if !user.totp_enabled {
        let token = state.core.create_session(user.id).await?;
        return Ok(([(header::SET_COOKIE, set_cookie(state, &token))], Redirect::to(next)).into_response());
    }
    let token = state.core.start_login_challenge(user.id).await?;
    let to = format!("/login/2fa?next={}", form_urlencoded::byte_serialize(next.as_bytes()).collect::<String>());
    Ok(([(header::SET_COOKIE, challenge_cookie(state, &token, 600))], Redirect::to(&to)).into_response())
}

// ---------------------------------------------------------------------------
// The second step of signing in
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "login_2fa.html")]
struct SecondStepPage {
    page: Page,
    next: String,
    error: Option<String>,
}

#[derive(Deserialize)]
pub struct NextQuery {
    next: Option<String>,
}

pub async fn second_form(State(state): State<AppState>, headers: HeaderMap, Query(q): Query<NextQuery>) -> AppResult {
    let valid = match challenge_token(&headers) {
        Some(t) => state.core.login_challenge_valid(t).await?,
        None => false,
    };
    if !valid {
        return Ok(Redirect::to("/login").into_response());
    }
    html(SecondStepPage {
        page: Page::new(&state, None, "Two-factor authentication"),
        next: safe_next(q.next.as_deref()),
        error: None,
    })
}

#[derive(Deserialize)]
pub struct CodeForm {
    code: String,
    next: Option<String>,
}

pub async fn second(State(state): State<AppState>, headers: HeaderMap, Form(f): Form<CodeForm>) -> AppResult {
    let next = safe_next(f.next.as_deref());
    let Some(token) = challenge_token(&headers) else {
        return Ok(Redirect::to("/login").into_response());
    };
    let error = match state.core.complete_login_challenge(token, &f.code).await? {
        SecondStep::SignedIn(user) => {
            let session = state.core.create_session(user.id).await?;
            let mut res = Redirect::to(&next).into_response();
            res.headers_mut().append(header::SET_COOKIE, set_cookie(&state, &session));
            res.headers_mut().append(header::SET_COOKIE, challenge_cookie(&state, "", 0));
            return Ok(res);
        }
        SecondStep::Expired => return Ok(Redirect::to("/login").into_response()),
        SecondStep::WrongCode => "That code isn't right. Try the current one from your app, or a recovery code.",
        SecondStep::Locked => "Too many wrong codes. Wait 15 minutes and try again.",
    };
    html(SecondStepPage {
        page: Page::new(&state, None, "Two-factor authentication"),
        next,
        error: Some(error.into()),
    })
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "settings/security.html")]
struct SecurityPage {
    page: Page,
    nav: &'static str,
    enabled: bool,
    /// Setting up: the secret and its QR code (an SVG data URI).
    setup: Option<(String, String)>,
    /// Recovery codes, shown once after they're made.
    codes: Vec<String>,
    codes_left: i64,
    error: Option<String>,
}

fn qr_data_uri(text: &str) -> String {
    let svg = qrcode::QrCode::new(text.as_bytes())
        .map(|code| {
            code.render::<qrcode::render::svg::Color<'_>>()
                .min_dimensions(200, 200)
                .dark_color(qrcode::render::svg::Color("#000000"))
                .light_color(qrcode::render::svg::Color("#ffffff"))
                .build()
        })
        .unwrap_or_default();
    format!("data:image/svg+xml;base64,{}", base64::engine::general_purpose::STANDARD.encode(svg))
}

async fn security_page(state: &AppState, user: User, codes: Vec<String>, error: Option<String>) -> AppResult {
    // The user's flag may predate what just happened.
    let fresh = state.core.user_by_id(user.id).await?.unwrap_or(user);
    let setup = if fresh.totp_enabled {
        None
    } else {
        state.core.pending_totp(fresh.id).await?.map(|secret| {
            let uri = totp_uri(&state.core.config.site_name, &fresh.username, &secret);
            // Grouped for typing it in by hand.
            let spaced = secret.as_bytes().chunks(4).map(|c| String::from_utf8_lossy(c).into_owned()).collect::<Vec<_>>().join(" ");
            (spaced, qr_data_uri(&uri))
        })
    };
    html(SecurityPage {
        enabled: fresh.totp_enabled,
        codes_left: state.core.recovery_codes_left(fresh.id).await?,
        page: Page::new(state, Some(fresh), "Two-factor authentication"),
        nav: "security",
        setup,
        codes,
        error,
    })
}

pub async fn security(State(state): State<AppState>, SignedIn(user): SignedIn) -> AppResult {
    security_page(&state, user, Vec::new(), None).await
}

pub async fn setup(State(state): State<AppState>, SignedIn(user): SignedIn) -> AppResult {
    if !user.totp_enabled {
        state.core.begin_totp_setup(&user).await?;
    }
    Ok(Redirect::to("/settings/security").into_response())
}

#[derive(Deserialize)]
pub struct EnableForm {
    code: String,
}

pub async fn enable(State(state): State<AppState>, SignedIn(user): SignedIn, Form(f): Form<EnableForm>) -> AppResult {
    match state.core.enable_totp(&user, &f.code).await {
        Ok(codes) => security_page(&state, user, codes, None).await,
        Err(keel_core::Error::Invalid(msg)) => security_page(&state, user, Vec::new(), Some(msg)).await,
        Err(e) => Err(e.into()),
    }
}

#[derive(Deserialize)]
pub struct PasswordForm {
    password: String,
}

/// Whether `password` is the user's (to confirm sensitive changes).
async fn password_ok(state: &AppState, user: &User, password: &str) -> AppResult<bool> {
    Ok(state.core.authenticate(&user.username, password).await?.is_some_and(|u| u.id == user.id))
}

pub async fn disable(State(state): State<AppState>, SignedIn(user): SignedIn, Form(f): Form<PasswordForm>) -> AppResult {
    if !password_ok(&state, &user, &f.password).await? {
        return security_page(&state, user, Vec::new(), Some("That password isn't right.".into())).await;
    }
    state.core.disable_totp(user.id).await?;
    Ok(Redirect::to("/settings/security").into_response())
}

pub async fn recovery(State(state): State<AppState>, SignedIn(user): SignedIn, Form(f): Form<PasswordForm>) -> AppResult {
    if !user.totp_enabled {
        return Ok(Redirect::to("/settings/security").into_response());
    }
    if !password_ok(&state, &user, &f.password).await? {
        return security_page(&state, user, Vec::new(), Some("That password isn't right.".into())).await;
    }
    let codes = state.core.new_recovery_codes(user.id).await?;
    security_page(&state, user, codes, None).await
}
