//! Who is making the request: session cookies for browsers, HTTP Basic auth
//! for git clients.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::{IntoResponse, Redirect, Response};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use keel_core::User;

use crate::{AppError, AppState};

const COOKIE: &str = "keel_session";

fn session_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|c| c.trim().strip_prefix(COOKIE)?.strip_prefix('='))
}

/// The signed-in user, if any.
#[derive(Clone)]
pub struct Viewer(pub Option<User>);

impl FromRequestParts<AppState> for Viewer {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, AppError> {
        if let Some(v) = parts.extensions.get::<Viewer>() {
            return Ok(v.clone());
        }
        let user = match session_token(&parts.headers) {
            Some(token) => state.core.session_user(token).await?,
            None => None,
        };
        let viewer = Viewer(user);
        parts.extensions.insert(viewer.clone());
        Ok(viewer)
    }
}

/// A signed-in user; redirects to the login page otherwise.
pub struct SignedIn(pub User);

impl FromRequestParts<AppState> for SignedIn {
    type Rejection = Response;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Response> {
        let Viewer(user) = Viewer::from_request_parts(parts, state)
            .await
            .map_err(IntoResponse::into_response)?;
        match user {
            Some(u) => Ok(SignedIn(u)),
            None => {
                let next = parts.uri.path();
                Err(Redirect::to(&format!("/login?next={}", encode_component(next))).into_response())
            }
        }
    }
}

pub fn set_cookie(state: &AppState, token: &str) -> HeaderValue {
    let secure = if state.core.config.base_url.starts_with("https://") {
        "; Secure"
    } else {
        ""
    };
    let max_age = 30 * 24 * 3600;
    HeaderValue::from_str(&format!(
        "{COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}"
    ))
    .expect("token is base64url")
}

pub fn clear_cookie() -> HeaderValue {
    HeaderValue::from_static("keel_session=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0")
}

pub fn token_from(headers: &HeaderMap) -> Option<&str> {
    session_token(headers)
}

/// Credentials from an `Authorization: Basic` header.
pub fn basic_auth(headers: &HeaderMap) -> Option<(String, String)> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, encoded) = value.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("basic") {
        return None;
    }
    let decoded = String::from_utf8(STANDARD.decode(encoded.trim()).ok()?).ok()?;
    let (user, pass) = decoded.split_once(':')?;
    Some((user.to_owned(), pass.to_owned()))
}

/// Only allow redirects to local paths (not `//evil.example`).
pub fn safe_next(next: Option<&str>) -> &str {
    match next {
        Some(n) if n.starts_with('/') && !n.starts_with("//") && !n.starts_with("/\\") => n,
        _ => "/",
    }
}

fn encode_component(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cookie_and_basic_auth() {
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, HeaderValue::from_static("a=b; keel_session=tok123; c=d"));
        assert_eq!(session_token(&h), Some("tok123"));

        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Basic YWRhOnB3OmQ="));
        assert_eq!(basic_auth(&h), Some(("ada".into(), "pw:d".into())));
    }

    #[test]
    fn next_redirects_stay_local() {
        assert_eq!(safe_next(Some("/ada/engine")), "/ada/engine");
        assert_eq!(safe_next(Some("//evil.example")), "/");
        assert_eq!(safe_next(Some("https://evil.example")), "/");
        assert_eq!(safe_next(None), "/");
    }
}
