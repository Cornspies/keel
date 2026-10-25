//! The light/dark switch. The choice is kept in a cookie (so it works
//! signed out too) and, when signed in, with the account (so it follows
//! people to other devices).

use axum::Form;
use axum::extract::State;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use keel_core::User;
use serde::Deserialize;

use crate::AppState;
use crate::error::AppResult;
use crate::session::{Viewer, safe_next};

const COOKIE: &str = "keel_theme";

tokio::task_local! {
    /// The theme cookie of the request being handled.
    static COOKIE_THEME: &'static str;
}

fn known(theme: &str) -> &'static str {
    keel_core::THEMES.into_iter().find(|t| *t == theme).unwrap_or("auto")
}

/// The request's theme cookie.
pub(crate) fn from_cookie(headers: &HeaderMap) -> &'static str {
    let value = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|c| c.trim().strip_prefix(COOKIE)?.strip_prefix('='));
    known(value.unwrap_or_default())
}

/// Runs `f` (handling a request) so that [`current`] knows the request's
/// theme cookie (see [`from_cookie`]).
pub(crate) async fn with_cookie<F: Future>(cookie: &'static str, f: F) -> F::Output {
    COOKIE_THEME.scope(cookie, f).await
}

/// The theme to render with: the account's choice, else the cookie's.
pub(crate) fn current(viewer: Option<&User>) -> &'static str {
    match viewer.map(|v| known(&v.theme)) {
        Some(theme) if theme != "auto" => theme,
        _ => COOKIE_THEME.try_with(|t| *t).unwrap_or("auto"),
    }
}

#[derive(Deserialize)]
pub struct ThemeForm {
    theme: String,
    /// Sent by the page's script, which applies the theme itself.
    js: Option<String>,
}

pub async fn set(State(state): State<AppState>, Viewer(viewer): Viewer, headers: HeaderMap, Form(f): Form<ThemeForm>) -> AppResult<Response> {
    let theme = known(&f.theme);
    if let Some(user) = &viewer {
        state.core.set_theme(user.id, theme).await?;
    }
    let secure = if state.core.config.base_url.starts_with("https://") { "; Secure" } else { "" };
    let cookie = match theme {
        "auto" => format!("{COOKIE}=; Path=/; SameSite=Lax; Max-Age=0{secure}"),
        t => format!("{COOKIE}={t}; Path=/; SameSite=Lax; Max-Age=31536000{secure}"),
    };
    let mut res = if f.js.is_some() {
        StatusCode::NO_CONTENT.into_response()
    } else {
        // Back to the page the switch was on.
        let back = headers
            .get(header::REFERER)
            .and_then(|r| r.to_str().ok())
            .and_then(|r| r.strip_prefix(state.core.config.base_url.trim_end_matches('/')));
        Redirect::to(&safe_next(back)).into_response()
    };
    res.headers_mut().insert(header::SET_COOKIE, HeaderValue::from_str(&cookie).expect("plain ASCII"));
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie() {
        let mut h = HeaderMap::new();
        assert_eq!(from_cookie(&h), "auto");
        h.insert(header::COOKIE, HeaderValue::from_static("keel_session=x; keel_theme=dark"));
        assert_eq!(from_cookie(&h), "dark");
        h.insert(header::COOKIE, HeaderValue::from_static("keel_theme=\"><script>"));
        assert_eq!(from_cookie(&h), "auto");
    }
}
