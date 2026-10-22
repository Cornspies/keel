//! The web frontend: server-rendered HTML pages and the git smart HTTP
//! protocol, in one axum router.

mod assets;
mod error;
mod githttp;
mod pages;
mod render;
mod repo;
mod session;

use std::collections::HashMap;
use std::ops::Deref;
use std::sync::Arc;

use axum::Router;
use axum::extract::{Request, State};
use axum::http::{HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use keel_core::{Core, User};
use keel_git::{CommitSummary, Oid};
use moka::sync::Cache;
use tower_http::compression::CompressionLayer;
use tower_http::trace::TraceLayer;

pub use error::AppError;

pub(crate) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

#[derive(Clone)]
pub struct AppState(Arc<Inner>);

pub struct Inner {
    pub core: Core,
    assets: assets::Assets,
    /// Git objects are immutable, so caches keyed by object ids never need
    /// invalidation; they only need a size bound.
    last_commits: Cache<(Oid, String), Arc<HashMap<String, CommitSummary>>>,
    highlighted: Cache<(Oid, String), Arc<Vec<String>>>,
    markdown: Cache<(Oid, String), Arc<String>>,
}

impl Deref for AppState {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}

impl AppState {
    pub fn new(core: Core) -> Self {
        render::warm_up();
        let mib = |n: u64| n * 1024 * 1024;
        Self(Arc::new(Inner {
            core,
            assets: assets::Assets::new(),
            last_commits: Cache::builder()
                .max_capacity(20_000)
                .weigher(|_, v: &Arc<HashMap<String, CommitSummary>>| v.len().max(1) as u32)
                .build(),
            highlighted: Cache::builder()
                .max_capacity(mib(128))
                .weigher(|_, v: &Arc<Vec<String>>| {
                    v.iter().map(|l| l.len() + 24).sum::<usize>().min(u32::MAX as usize) as u32
                })
                .build(),
            markdown: Cache::builder()
                .max_capacity(mib(32))
                .weigher(|_, v: &Arc<String>| v.len().min(u32::MAX as usize) as u32)
                .build(),
        }))
    }
}

/// Values every page template needs.
pub(crate) struct Page {
    pub title: String,
    pub site_name: String,
    pub viewer: Option<User>,
    pub registration_open: bool,
    pub css_url: &'static str,
    pub js_url: &'static str,
}

impl Page {
    pub fn new(state: &AppState, viewer: Option<User>, title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            site_name: state.core.config.site_name.clone(),
            viewer,
            registration_open: state.core.config.open_registration,
            css_url: state.assets.css_url,
            js_url: state.assets.js_url,
        }
    }
}

pub fn router(state: AppState) -> Router {
    let web = Router::new()
        .route("/", get(pages::home))
        .route("/login", get(pages::login_form).post(pages::login))
        .route("/register", get(pages::register_form).post(pages::register))
        .route("/logout", post(pages::logout))
        .route("/new", get(pages::new_repo_form).post(pages::new_repo))
        .route("/settings/keys", get(pages::keys).post(pages::add_key))
        .route("/settings/keys/{id}/delete", post(pages::delete_key))
        .route("/static/{file}", get(assets::serve))
        .route("/{owner}", get(pages::user))
        .route("/{owner}/{repo}", get(repo::home))
        .route("/{owner}/{repo}/tree/{*rest}", get(repo::tree))
        .route("/{owner}/{repo}/blob/{*rest}", get(repo::blob))
        .route("/{owner}/{repo}/raw/{*rest}", get(repo::raw))
        .route("/{owner}/{repo}/last-commits/{*rest}", get(repo::last_commits_json))
        .route("/{owner}/{repo}/commits", get(repo::commits_default))
        .route("/{owner}/{repo}/commits/{*rest}", get(repo::commits))
        .route("/{owner}/{repo}/commit/{sha}", get(repo::commit))
        .route("/{owner}/{repo}/branches", get(repo::refs))
        .route("/{owner}/{repo}/settings", get(repo::settings).post(repo::update_settings))
        .route("/{owner}/{repo}/delete", post(repo::delete))
        .fallback(|| async { AppError::NotFound })
        .layer(middleware::from_fn_with_state(state.clone(), web_guard))
        .layer(CompressionLayer::new());

    // Git traffic is already compressed and must not get browser headers.
    let git = Router::new()
        .route("/{owner}/{repo}/info/refs", get(githttp::info_refs))
        .route("/{owner}/{repo}/git-upload-pack", post(githttp::upload_pack))
        .route("/{owner}/{repo}/git-receive-pack", post(githttp::receive_pack));

    web.merge(git)
        .with_state(state)
        .layer(TraceLayer::new_for_http())
}

/// Security headers for HTML pages, plus CSRF protection: state-changing
/// requests must come from our own origin. Session cookies are also
/// `SameSite=Lax`, so this is defence in depth.
async fn web_guard(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let safe = req.method() == Method::GET || req.method() == Method::HEAD;
    if !safe && !same_origin(&req, &state.core.config.base_url) {
        return (StatusCode::FORBIDDEN, "Cross-origin request blocked").into_response();
    }
    let mut res = next.run(req).await;
    let headers = res.headers_mut();
    let defaults = [
        (
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; style-src 'self'; script-src 'self'; connect-src 'self'; img-src * data:; \
             form-action 'self'; frame-ancestors 'none'; base-uri 'none'",
        ),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::REFERRER_POLICY, "same-origin"),
        (header::X_FRAME_OPTIONS, "DENY"),
    ];
    for (name, value) in defaults {
        headers.entry(name).or_insert(HeaderValue::from_static(value));
    }
    res
}

fn same_origin(req: &Request, base_url: &str) -> bool {
    let headers = req.headers();
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok());
    let source = headers
        .get(header::ORIGIN)
        .or_else(|| headers.get(header::REFERER))
        .and_then(|h| h.to_str().ok());
    match (source, host) {
        // Non-browser clients send neither; without a cookie they can't act
        // as anyone anyway.
        (None, _) => true,
        // Behind a reverse proxy, Host may differ from the public URL.
        (Some(source), _) if source == base_url || source.starts_with(&format!("{base_url}/")) => {
            true
        }
        (Some(source), Some(host)) => source
            .split_once("://")
            .map(|(_, rest)| rest.split('/').next() == Some(host))
            .unwrap_or(false),
        (Some(_), None) => false,
    }
}
