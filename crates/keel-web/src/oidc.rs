//! Single sign-on with an OpenID Connect provider (`--oidc-issuer`): the
//! authorization code flow with PKCE, for a confidential client.
//!
//! `/login/oidc` sends the browser to the provider with a fresh `state`,
//! `nonce` and PKCE challenge, kept in a signed cookie for ten minutes (no
//! server-side storage for anonymous visitors to fill up). The provider
//! sends it back to `/login/oidc/callback` with a code, which keel exchanges
//! for an ID token directly with the provider's token endpoint, over HTTPS.
//! The ID token's issuer, audience, expiry and nonce are checked; its
//! signature isn't, which OpenID Connect allows for a token received this
//! way, from the provider's own endpoint over TLS (Core 1.0, section
//! 3.1.3.7, step 6). keel-core decides which account it signs in to.

use std::time::{Duration, Instant};

use askama::Template;
use axum::extract::{Form, Query, State};
use axum::http::{HeaderMap, HeaderValue, header};
use axum::response::{IntoResponse, Redirect, Response};
use base64::Engine;
use base64::engine::general_purpose::{URL_SAFE, URL_SAFE_NO_PAD};
use keel_core::{Claims, OidcConfig, SsoOutcome};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::error::{AppResult, html};
use crate::session::{SignedIn, Viewer, safe_next};
use crate::{AppState, Page};

const COOKIE: &str = "keel_oidc";
/// How long a sign-in at the provider may take.
const FLOW_SECS: i64 = 600;
/// How long the provider's configuration is used before it's fetched again.
const DISCOVERY_TTL: Duration = Duration::from_secs(3600);
/// Clock difference with the provider that's tolerated.
const SKEW_SECS: i64 = 60;

/// The provider's configuration (`.well-known/openid-configuration`).
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Discovery {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    #[serde(default)]
    userinfo_endpoint: Option<String>,
    #[serde(default)]
    token_endpoint_auth_methods_supported: Vec<String>,
}

/// The provider, with its configuration once fetched.
pub(crate) struct Provider {
    pub config: OidcConfig,
    client: reqwest::Client,
    discovery: tokio::sync::Mutex<Option<(Discovery, Instant)>>,
}

impl Provider {
    pub(crate) fn new(config: OidcConfig) -> Self {
        let client = reqwest::Client::builder()
            .user_agent(concat!("keel/", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(15))
            .build()
            .expect("valid HTTP client configuration");
        Self { config, client, discovery: tokio::sync::Mutex::new(None) }
    }

    async fn discovery(&self) -> Result<Discovery, String> {
        let mut cached = self.discovery.lock().await;
        if let Some((d, at)) = cached.as_ref()
            && at.elapsed() < DISCOVERY_TTL
        {
            return Ok(d.clone());
        }
        let url = format!("{}/.well-known/openid-configuration", self.config.issuer);
        let res = self.client.get(&url).send().await.map_err(|e| format!("fetching {url}: {e}"))?;
        if !res.status().is_success() {
            return Err(format!("fetching {url}: HTTP {}", res.status()));
        }
        let d: Discovery = res.json().await.map_err(|e| format!("reading {url}: {e}"))?;
        if d.issuer.trim_end_matches('/') != self.config.issuer {
            return Err(format!("the provider calls itself {}, not {}", d.issuer, self.config.issuer));
        }
        for endpoint in [&d.authorization_endpoint, &d.token_endpoint].into_iter().chain(d.userinfo_endpoint.as_ref()) {
            if !secure_url(endpoint) {
                return Err(format!("{endpoint} isn't https://"));
            }
        }
        *cached = Some((d.clone(), Instant::now()));
        Ok(d)
    }
}

/// Whether `url` is https, or http to this machine (for trying things out).
pub fn secure_url(url: &str) -> bool {
    let Ok(u) = reqwest::Url::parse(url) else { return false };
    let host = u.host_str().unwrap_or_default().trim_start_matches('[').trim_end_matches(']');
    match u.scheme() {
        "https" => true,
        "http" => host == "localhost" || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback()),
        _ => false,
    }
}

/// What the browser carries to the provider and back, signed.
#[derive(Serialize, Deserialize)]
struct Flow {
    state: String,
    nonce: String,
    verifier: String,
    next: String,
    /// Connecting to this signed-in account rather than signing in.
    link: Option<i64>,
    expires: i64,
}

fn random(bytes: usize) -> String {
    let mut buf = vec![0u8; bytes];
    getrandom::fill(&mut buf).expect("system randomness");
    URL_SAFE_NO_PAD.encode(buf)
}

fn mac(state: &AppState, payload: &str) -> String {
    let full = keel_core::sign(&state.core.derived_secret("oidc flow"), payload.as_bytes());
    full.trim_start_matches("sha256=").to_owned()
}

fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn flow_cookie(state: &AppState, value: &str, max_age: i64) -> HeaderValue {
    let secure = if state.core.config.base_url.starts_with("https://") { "; Secure" } else { "" };
    HeaderValue::from_str(&format!("{COOKIE}={value}; Path=/login/oidc; HttpOnly; SameSite=Lax; Max-Age={max_age}{secure}"))
        .expect("cookie is ASCII")
}

fn read_flow(state: &AppState, headers: &HeaderMap) -> Option<Flow> {
    let value = headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .find_map(|c| c.trim().strip_prefix(COOKIE)?.strip_prefix('='))?;
    let (payload, sig) = value.rsplit_once('.')?;
    if !same(&mac(state, payload), sig) {
        return None;
    }
    let flow: Flow = serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).ok()?).ok()?;
    (flow.expires > crate::now()).then_some(flow)
}

#[derive(Template)]
#[template(path = "message.html")]
struct MessagePage {
    page: Page,
    heading: String,
    message: String,
    link: (String, String),
}

fn failed(state: &AppState, viewer: Option<keel_core::User>, message: String) -> Response {
    let page = MessagePage {
        page: Page::new(state, viewer, "Signing in didn't work"),
        heading: "Signing in didn't work".into(),
        message,
        link: ("/login".into(), "Back to sign in".into()),
    };
    match html(page) {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

fn provider(state: &AppState) -> Result<&Provider, crate::AppError> {
    state.oidc.as_ref().ok_or(crate::AppError::NotFound)
}

#[derive(Deserialize)]
pub struct StartQuery {
    next: Option<String>,
}

/// `GET /login/oidc`: off to the provider.
pub async fn start(State(state): State<AppState>, Viewer(viewer): Viewer, Query(q): Query<StartQuery>) -> AppResult {
    provider(&state)?;
    Ok(begin(&state, viewer, safe_next(q.next.as_deref()), None).await)
}

#[derive(Deserialize)]
pub struct LinkForm {
    #[serde(default)]
    password: String,
}

/// `POST /settings/security/sso`: connects the provider to this account.
/// A new way to sign in, so confirmed like adding a key: otherwise a few
/// minutes at someone's signed-in browser would be enough to keep signing
/// in as them, past a password change.
pub async fn link(State(state): State<AppState>, SignedIn(user): SignedIn, headers: HeaderMap, Form(f): Form<LinkForm>) -> AppResult {
    provider(&state)?;
    if let Some(refused) = crate::twofactor::confirmed(&state, &user, &headers, &f.password).await? {
        return Ok(refused);
    }
    let id = user.id;
    Ok(begin(&state, Some(user), "/settings/security".into(), Some(id)).await)
}

async fn begin(state: &AppState, viewer: Option<keel_core::User>, next: String, link: Option<i64>) -> Response {
    let provider = state.oidc.as_ref().expect("checked by callers");
    let d = match provider.discovery().await {
        Ok(d) => d,
        Err(e) => {
            tracing::warn!("oidc: {e}");
            return failed(state, viewer, format!("{} can't be reached right now.", provider.config.name));
        }
    };
    let flow = Flow {
        state: random(24),
        nonce: random(24),
        verifier: random(48),
        next,
        link,
        expires: crate::now() + FLOW_SECS,
    };
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(flow.verifier.as_bytes()));
    let query = form_urlencoded::Serializer::new(String::new())
        .append_pair("response_type", "code")
        .append_pair("client_id", &provider.config.client_id)
        .append_pair("redirect_uri", &redirect_uri(state))
        .append_pair("scope", "openid email profile")
        .append_pair("state", &flow.state)
        .append_pair("nonce", &flow.nonce)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256")
        .finish();
    let sep = if d.authorization_endpoint.contains('?') { '&' } else { '?' };
    let payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&flow).expect("serializable"));
    let cookie = format!("{payload}.{}", mac(state, &payload));
    let to = format!("{}{sep}{query}", d.authorization_endpoint);
    ([(header::SET_COOKIE, flow_cookie(state, &cookie, FLOW_SECS))], Redirect::to(&to)).into_response()
}

fn redirect_uri(state: &AppState) -> String {
    format!("{}/login/oidc/callback", state.core.config.base_url)
}

#[derive(Deserialize)]
pub struct CallbackQuery {
    code: Option<String>,
    state: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

/// `GET /login/oidc/callback`: back from the provider.
pub async fn callback(
    State(state): State<AppState>,
    Viewer(viewer): Viewer,
    headers: HeaderMap,
    Query(q): Query<CallbackQuery>,
) -> AppResult {
    let provider = provider(&state)?;
    let clear = (header::SET_COOKIE, flow_cookie(&state, "", 0));
    let fail = |viewer, msg: String| ([clear.clone()], failed(&state, viewer, msg)).into_response();
    let Some(flow) = read_flow(&state, &headers) else {
        return Ok(fail(viewer, "This sign-in took too long or was started in another browser. Try again.".into()));
    };
    // Before anything else, errors included (they carry `state` too, RFC
    // 6749 4.1.2.1): otherwise any page could send a visitor here with an
    // error message of its own, shown as the provider's.
    if !q.state.as_deref().is_some_and(|s| same(s, &flow.state)) {
        return Ok(fail(viewer, "This sign-in doesn't match the one started here. Try again.".into()));
    }
    if let Some(e) = q.error {
        let why = q.error_description.unwrap_or(e);
        return Ok(fail(viewer, format!("{} said: {why}", provider.config.name)));
    }
    let Some(code) = q.code else {
        return Ok(fail(viewer, "The provider sent back no code.".into()));
    };
    let claims = match exchange(&state, provider, &code, &flow).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("oidc: {e}");
            return Ok(fail(viewer, format!("{} didn't confirm the sign-in: {e}", provider.config.name)));
        }
    };
    if let Some(user_id) = flow.link {
        // Only for the account that started it, still signed in.
        let Some(user) = viewer.as_ref().filter(|v| v.id == user_id) else {
            return Ok(fail(viewer, "Sign in to keel first, then connect your account.".into()));
        };
        return Ok(match state.core.link_identity(user, &claims).await {
            Ok(()) => ([clear], Redirect::to("/settings/security?saved=sso")).into_response(),
            Err(keel_core::Error::Invalid(msg)) => fail(viewer, msg),
            Err(e) => return Err(e.into()),
        });
    }
    match state.core.sso_sign_in(&claims, provider.config.allow_signup, provider.config.link_by_email).await {
        Ok(outcome) => {
            let user = outcome.user();
            match &outcome {
                SsoOutcome::Created(u) => tracing::info!(user = %u.username, "account created with single sign-on"),
                SsoOutcome::LinkedByEmail(u) => tracing::info!(user = %u.username, "single sign-on connected by email"),
                SsoOutcome::Linked(_) => {}
            }
            let mut res = crate::twofactor::sign_in(&state, user, &flow.next).await?;
            res.headers_mut().append(clear.0, clear.1);
            Ok(res)
        }
        Err(keel_core::Error::Invalid(msg)) => Ok(fail(viewer, msg)),
        Err(e) => Err(e.into()),
    }
}

/// Exchanges the code for an ID token and checks it.
async fn exchange(state: &AppState, provider: &Provider, code: &str, flow: &Flow) -> Result<Claims, String> {
    let d = provider.discovery().await?;
    let cfg = &provider.config;
    let mut form = vec![
        ("grant_type", "authorization_code".to_owned()),
        ("code", code.to_owned()),
        ("redirect_uri", redirect_uri(state)),
        ("code_verifier", flow.verifier.clone()),
    ];
    let methods = &d.token_endpoint_auth_methods_supported;
    let basic = methods.is_empty() || methods.iter().any(|m| m == "client_secret_basic");
    let mut req = provider.client.post(&d.token_endpoint).header(header::ACCEPT, "application/json");
    if basic {
        // The ID and secret are form-encoded first (RFC 6749, 2.3.1).
        let enc = |s: &str| form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>();
        req = req.basic_auth(enc(&cfg.client_id), Some(enc(&cfg.client_secret)));
    } else if methods.iter().any(|m| m == "client_secret_post") {
        form.push(("client_id", cfg.client_id.clone()));
        form.push(("client_secret", cfg.client_secret.clone()));
    } else {
        return Err("the provider supports neither client_secret_basic nor client_secret_post".into());
    }
    let res = req.form(&form).send().await.map_err(|e| format!("token request: {e}"))?;
    let status = res.status();
    let body: Value = res.json().await.map_err(|e| format!("token response: {e}"))?;
    if !status.is_success() {
        let why = body["error_description"].as_str().or(body["error"].as_str()).unwrap_or("refused");
        return Err(format!("token request: {why}"));
    }
    let id_token = body["id_token"].as_str().ok_or("no ID token")?;
    let mut claims = id_token_claims(id_token, &d.issuer, &cfg.client_id, &flow.nonce, crate::now())?;
    // Some providers only tell the address through userinfo.
    if claims.email.is_empty()
        && let (Some(url), Some(access)) = (&d.userinfo_endpoint, body["access_token"].as_str())
    {
        let info: Value = provider
            .client
            .get(url)
            .bearer_auth(access)
            .send()
            .await
            .map_err(|e| format!("userinfo: {e}"))?
            .json()
            .await
            .map_err(|e| format!("userinfo: {e}"))?;
        if info["sub"].as_str() == Some(claims.subject.as_str()) {
            merge_profile(&mut claims, &info);
        }
    }
    Ok(claims)
}

/// The claims of an ID token from the token endpoint, after checking the
/// issuer, audience, expiry and nonce.
fn id_token_claims(token: &str, issuer: &str, client_id: &str, nonce: &str, now: i64) -> Result<Claims, String> {
    let mut parts = token.split('.');
    let (Some(_header), Some(payload), Some(_signature), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return Err("the ID token isn't a JWT".into());
    };
    let bytes = URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .or_else(|_| URL_SAFE.decode(payload))
        .map_err(|_| "the ID token can't be read")?;
    let c: Value = serde_json::from_slice(&bytes).map_err(|_| "the ID token can't be read")?;
    if c["iss"].as_str().map(|i| i.trim_end_matches('/')) != Some(issuer.trim_end_matches('/')) {
        return Err("the ID token is from another issuer".into());
    }
    let audiences: Vec<&str> = match &c["aud"] {
        Value::String(a) => vec![a.as_str()],
        Value::Array(a) => a.iter().filter_map(Value::as_str).collect(),
        _ => Vec::new(),
    };
    if !audiences.contains(&client_id) || (audiences.len() > 1 && c["azp"].as_str() != Some(client_id)) {
        return Err("the ID token is for another client".into());
    }
    if c["exp"].as_i64().is_none_or(|exp| exp + SKEW_SECS < now) {
        return Err("the ID token has expired".into());
    }
    if c["nonce"].as_str().is_none_or(|n| !same(n, nonce)) {
        return Err("the ID token is for another sign-in".into());
    }
    let subject = c["sub"].as_str().filter(|s| !s.is_empty()).ok_or("the ID token names nobody")?;
    let mut claims = Claims { issuer: issuer.trim_end_matches('/').to_owned(), subject: subject.to_owned(), ..Claims::default() };
    merge_profile(&mut claims, &c);
    Ok(claims)
}

/// Takes the address and a username from ID token claims or userinfo.
fn merge_profile(claims: &mut Claims, c: &Value) {
    if let Some(email) = c["email"].as_str() {
        claims.email = email.to_owned();
        // Some providers send it as a string.
        claims.email_verified = c["email_verified"].as_bool().unwrap_or(c["email_verified"].as_str() == Some("true"));
    }
    if claims.username.is_empty() {
        claims.username = c["preferred_username"].as_str().or(c["nickname"].as_str()).or(c["name"].as_str()).unwrap_or("").to_owned();
    }
}

/// `POST /settings/security/sso/{n}/delete`: disconnects the provider.
pub async fn unlink(State(state): State<AppState>, SignedIn(user): SignedIn) -> AppResult {
    let provider = provider(&state)?;
    match state.core.unlink_identity(&user, &provider.config.issuer).await {
        Ok(()) => Ok(Redirect::to("/settings/security?saved=sso-off").into_response()),
        Err(keel_core::Error::Invalid(msg)) => Ok(failed(&state, Some(user), msg)),
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token(claims: Value) -> String {
        format!("eyJhbGciOiJSUzI1NiJ9.{}.sig", URL_SAFE_NO_PAD.encode(claims.to_string()))
    }

    #[test]
    fn checks_id_tokens() {
        let good = serde_json::json!({
            "iss": "https://id.example.com/", "aud": "keel", "exp": 1000, "nonce": "n1", "sub": "42",
            "email": "sam@example.com", "email_verified": "true", "preferred_username": "sam",
        });
        let c = id_token_claims(&token(good.clone()), "https://id.example.com", "keel", "n1", 900).unwrap();
        assert_eq!((c.subject.as_str(), c.email.as_str(), c.email_verified, c.username.as_str()), ("42", "sam@example.com", true, "sam"));
        assert_eq!(c.issuer, "https://id.example.com");
        let with = |k: &str, v: Value| {
            let mut t = good.clone();
            t[k] = v;
            id_token_claims(&token(t), "https://id.example.com", "keel", "n1", 900)
        };
        assert!(with("iss", "https://evil.example.com".into()).is_err());
        assert!(with("aud", "someone-else".into()).is_err());
        assert!(with("aud", serde_json::json!(["keel", "other"])).is_err(), "several audiences need azp");
        assert!(with("aud", serde_json::json!(["keel"])).is_ok());
        assert!(with("exp", 800.into()).is_err());
        assert!(with("exp", 850.into()).is_ok(), "within the allowed skew");
        assert!(with("nonce", "n2".into()).is_err());
        assert!(with("sub", "".into()).is_err());
        assert!(id_token_claims("nonsense", "https://id.example.com", "keel", "n1", 900).is_err());
    }

    #[test]
    fn only_secure_urls() {
        assert!(secure_url("https://id.example.com"));
        assert!(secure_url("http://127.0.0.1:8080/realms/x"));
        assert!(secure_url("http://localhost:8080"));
        assert!(!secure_url("http://id.example.com"));
        assert!(!secure_url("ftp://id.example.com"));
    }
}
