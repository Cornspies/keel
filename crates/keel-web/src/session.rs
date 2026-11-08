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

/// Whether the site is served over `https://`. Cookies are then `Secure`
/// and named `__Host-…`, which only this host can set: other sites under
/// the same domain (and anyone on plain HTTP) can't plant a session or
/// sign-in attempt of their own.
pub(crate) fn secure(state: &AppState) -> bool {
    state.core.config.base_url.starts_with("https://")
}

/// The name of keel's cookie `name` (see [`secure`]).
pub(crate) fn cookie_name(state: &AppState, name: &str) -> String {
    if secure(state) { format!("__Host-{name}") } else { name.to_owned() }
}

/// The value of cookie `name` in a request.
pub(crate) fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|c| c.trim().strip_prefix(name)?.strip_prefix('='))
}

fn session_token<'a>(state: &AppState, headers: &'a HeaderMap) -> Option<&'a str> {
    cookie(headers, &cookie_name(state, COOKIE))
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
        let user = match session_token(state, &parts.headers) {
            Some(token) => state.core.session_user(token).await?,
            None => None,
        };
        if let Some(u) = &user {
            state.refresh_unread(u.id).await?;
        }
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

/// A cookie for the whole site (`__Host-` ones must be), `Secure` behind
/// `https://`.
pub(crate) fn site_cookie(state: &AppState, name: &str, value: &str, max_age: i64) -> HeaderValue {
    let secure = if secure(state) { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "{}={value}; Path=/; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}",
        cookie_name(state, name)
    ))
    .expect("cookies are ASCII")
}

pub fn set_cookie(state: &AppState, token: &str) -> HeaderValue {
    site_cookie(state, COOKIE, token, 30 * 24 * 3600)
}

pub fn clear_cookie(state: &AppState) -> HeaderValue {
    site_cookie(state, COOKIE, "", 0)
}

pub fn token_from<'a>(state: &AppState, headers: &'a HeaderMap) -> Option<&'a str> {
    session_token(state, headers)
}

/// How long after signing in accounts without a password may make the
/// changes that otherwise ask for one.
pub(crate) const RECENT_SIGN_IN_SECS: i64 = 600;

/// Confirms a sensitive change by the signed-in `user` (adding keys and
/// tokens, changing the address): their password; or, for an account
/// without one (made with single sign-on), that this browser signed in in
/// the last ten minutes. `Invalid`, saying what to do, otherwise.
pub(crate) async fn confirm(state: &AppState, user: &User, headers: &HeaderMap, password: &str) -> keel_core::Result<()> {
    if user.has_password() {
        return if state.core.confirm_password(user, password).await? {
            Ok(())
        } else {
            Err(keel_core::Error::Invalid("Your password is incorrect.".into()))
        };
    }
    let started = match token_from(state, headers) {
        Some(t) => state.core.session_started(t).await?,
        None => None,
    };
    if started.is_some_and(|t| crate::now() - t <= RECENT_SIGN_IN_SECS) {
        return Ok(());
    }
    let name = state.core.config.oidc.as_ref().map_or("single sign-on", |o| o.name.as_str());
    Err(keel_core::Error::Invalid(format!(
        "To confirm this, sign out and sign in again with {name}: changes like this work for ten minutes after signing in."
    )))
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

/// Only allow redirects to local paths: not `//evil.example`, nor what
/// browsers turn into that (they drop tabs and newlines, and read `\` as
/// `/`). Non-ASCII characters are percent-encoded, since header values
/// can't have them.
pub fn safe_next(next: Option<&str>) -> String {
    match next {
        Some(n)
            if n.starts_with('/')
                && !n.starts_with("//")
                && !n.chars().any(|c| c.is_control() || c == ' ' || c == '\\') =>
        {
            let mut out = String::with_capacity(n.len());
            for c in n.chars() {
                if c.is_ascii() {
                    out.push(c);
                } else {
                    for b in c.encode_utf8(&mut [0; 4]).bytes() {
                        out.push_str(&format!("%{b:02X}"));
                    }
                }
            }
            out
        }
        _ => "/".to_owned(),
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
        assert_eq!(cookie(&h, COOKIE), Some("tok123"));
        // Other sites under the same domain can set a `keel_session`
        // cookie, but not a `__Host-` one: behind https, only that counts.
        h.insert(header::COOKIE, HeaderValue::from_static("keel_session=theirs; __Host-keel_session=ours"));
        assert_eq!(cookie(&h, "__Host-keel_session"), Some("ours"));

        h.insert(header::AUTHORIZATION, HeaderValue::from_static("Basic YWRhOnB3OmQ="));
        assert_eq!(basic_auth(&h), Some(("ada".into(), "pw:d".into())));
    }

    #[test]
    fn next_redirects_stay_local() {
        assert_eq!(safe_next(Some("/ada/engine")), "/ada/engine");
        assert_eq!(safe_next(Some("//evil.example")), "/");
        assert_eq!(safe_next(Some("https://evil.example")), "/");
        assert_eq!(safe_next(Some("/\\evil.example")), "/");
        assert_eq!(safe_next(Some("/a\\b")), "/");
        assert_eq!(safe_next(Some("/\t/evil.example")), "/");
        assert_eq!(safe_next(Some("/\n/evil.example")), "/");
        assert_eq!(safe_next(Some("/\r/evil.example")), "/");
        assert_eq!(safe_next(Some("/\u{7f}/x")), "/");
        assert_eq!(safe_next(Some("/ /x")), "/");
        assert_eq!(safe_next(Some("/ada/é")), "/ada/%C3%A9");
        assert_eq!(safe_next(Some("/ada/engine?tab=1#x")), "/ada/engine?tab=1#x");
        assert_eq!(safe_next(None), "/");
    }
}
