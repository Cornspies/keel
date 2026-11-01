//! Links sent by email: password resets and address verification.

use askama::Template;
use axum::Form;
use axum::extract::{Query, State};
use serde::Deserialize;

use crate::error::{AppResult, html};
use crate::session::Viewer;
use crate::{AppState, Page};

/// A short page with a heading, a message and a link onwards.
#[derive(Template)]
#[template(path = "message.html")]
struct MessagePage {
    page: Page,
    heading: String,
    message: String,
    link: (String, String),
}

fn message(state: &AppState, viewer: Option<keel_core::User>, heading: &str, text: &str, link: (&str, &str)) -> AppResult {
    html(MessagePage {
        page: Page::new(state, viewer, heading),
        heading: heading.into(),
        message: text.into(),
        link: (link.0.into(), link.1.into()),
    })
}

#[derive(Template)]
#[template(path = "password_forgot.html")]
struct ForgotPage {
    page: Page,
    error: Option<String>,
    email: String,
}

pub async fn forgot_form(State(state): State<AppState>) -> AppResult {
    html(ForgotPage { page: Page::new(&state, None, "Reset your password"), error: None, email: String::new() })
}

#[derive(Deserialize)]
pub struct ForgotForm {
    email: String,
}

pub async fn forgot(State(state): State<AppState>, Form(f): Form<ForgotForm>) -> AppResult {
    match state.core.request_password_reset(&f.email).await {
        Ok(()) => message(
            &state,
            None,
            "Check your email",
            "If an account uses that address, we sent it a link to choose a new password. The link works for an hour.",
            ("/login", "Back to sign in"),
        ),
        Err(keel_core::Error::Invalid(msg)) => {
            html(ForgotPage { page: Page::new(&state, None, "Reset your password"), error: Some(msg), email: f.email })
        }
        Err(e) => Err(e.into()),
    }
}

#[derive(Template)]
#[template(path = "password_reset.html")]
struct ResetPage {
    page: Page,
    /// Empty when it's in the link's fragment, for the page's script to
    /// fill in.
    token: String,
    error: Option<String>,
}

/// Links put the token in the fragment (`#token=…`), which browsers don't
/// send, so it never reaches the server's or a proxy's request log; a
/// script on the page moves it into the form. Links sent before that have
/// it in the query.
#[derive(Deserialize)]
pub struct TokenQuery {
    #[serde(default)]
    token: String,
}

pub async fn reset_form(State(state): State<AppState>, Query(q): Query<TokenQuery>) -> AppResult {
    if !q.token.is_empty() && !state.core.reset_token_valid(&q.token).await? {
        return message(
            &state,
            None,
            "This link has expired",
            "Links to choose a password work once: for an hour after asking for a reset, or a week after an invitation. Request a new one.",
            ("/password/forgot", "Reset your password"),
        );
    }
    html(ResetPage { page: Page::new(&state, None, "Choose a password"), token: q.token, error: None })
}

#[derive(Deserialize)]
pub struct ResetForm {
    token: String,
    new: String,
    confirm: String,
}

pub async fn reset(State(state): State<AppState>, Form(f): Form<ResetForm>) -> AppResult {
    let page = |error: String| {
        html(ResetPage { page: Page::new(&state, None, "Choose a password"), token: f.token.clone(), error: Some(error) })
    };
    if f.new != f.confirm {
        return page("The passwords don’t match.".into());
    }
    match state.core.reset_password(f.token.trim(), &f.new).await {
        // With two-factor authentication, the second step still applies.
        Ok(user) => crate::twofactor::sign_in(&state, &user, "/").await,
        Err(keel_core::Error::Invalid(msg)) => page(msg),
        Err(e) => Err(e.into()),
    }
}

#[derive(Template)]
#[template(path = "email_verify.html")]
struct VerifyPage {
    page: Page,
    token: String,
}

/// The page a verification link opens: verifying takes a click (a POST),
/// so mail scanners that open links don't use it up.
pub async fn verify_form(State(state): State<AppState>, Viewer(viewer): Viewer, Query(q): Query<TokenQuery>) -> AppResult {
    html(VerifyPage { page: Page::new(&state, viewer, "Verify your email address"), token: q.token })
}

#[derive(Deserialize)]
pub struct VerifyForm {
    #[serde(default)]
    token: String,
}

pub async fn verify(State(state): State<AppState>, Viewer(viewer): Viewer, Form(f): Form<VerifyForm>) -> AppResult {
    match state.core.verify_email(f.token.trim()).await {
        Ok(user) => {
            let text = format!("Thanks! {} is verified, so you can now get notification emails.", user.email);
            message(&state, viewer, "Email address verified", &text, ("/settings/notifications", "Email settings"))
        }
        Err(keel_core::Error::Invalid(msg)) => {
            message(&state, viewer, "This link doesn't work", &msg, ("/settings/account", "Account settings"))
        }
        Err(e) => Err(e.into()),
    }
}
