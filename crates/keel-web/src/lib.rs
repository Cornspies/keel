//! The web frontend: server-rendered HTML pages and the git smart HTTP
//! protocol, in one axum router.

mod admin;
mod api;
mod archive;
mod assets;
mod deploy_keys;
mod blame;
mod edit;
mod error;
mod explore;
mod feed;
mod forks;
mod githttp;
mod import;
mod insights;
mod issue_search;
mod issues;
mod labels;
mod lastcommit;
mod lfs;
mod notifications;
mod orgs;
mod hooks;
mod images;
mod pages;
mod password;
mod profile;
mod projects;
mod protection;
mod milestones;
mod pulls;
mod releases;
mod render;
mod repo;
mod search;
mod session;
mod stars;
mod settings;
mod signatures;
mod statuses;
mod theme;
mod twofactor;
mod wiki;

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
use error::AppResult;

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
    /// `None` if the index couldn't be opened; search is then unavailable.
    search: Option<Arc<keel_search::SearchIndex>>,
    /// Git objects are immutable, so caches keyed by object ids never need
    /// invalidation; they only need a size bound.
    last_commits: Cache<(Oid, String), Arc<HashMap<String, CommitSummary>>>,
    highlighted: Cache<(Oid, String), Arc<Vec<String>>>,
    markdown: Cache<(Oid, String), Arc<String>>,
    /// Blame per (commit, path).
    blames: Cache<(Oid, String), Arc<keel_git::Blame>>,
    /// Limits how many blames run at once.
    blame_slots: tokio::sync::Semaphore,
    /// Fetches images for [`images`], a few at a time.
    image_client: reqwest::Client,
    image_slots: Arc<tokio::sync::Semaphore>,
    /// Merge result per (base, head); `None` if head is already merged.
    mergeability: Cache<(Oid, Oid), Option<keel_git::Mergeability>>,
    /// Unread notifications per user id, looked up with the session on each
    /// request so that page headers can show it.
    unread: Cache<i64, i64>,
    started: std::time::Instant,
    /// Limits how many archives are built at once.
    archives: Arc<tokio::sync::Semaphore>,
}

impl Deref for AppState {
    type Target = Inner;
    fn deref(&self) -> &Inner {
        &self.0
    }
}

impl AppState {
    /// Creates the state and starts background tasks (needs a Tokio
    /// runtime).
    pub fn new(core: Core) -> Self {
        render::warm_up();
        let state = Self::build(core);
        lastcommit::warm_after_pushes(state.clone());
        search::start_indexer(state.clone());
        state
    }

    fn build(core: Core) -> Self {
        let mib = |n: u64| n * 1024 * 1024;
        images::set_key(core.image_proxy_key());
        Self(Arc::new(Inner {
            image_client: images::client(&core.config.outbound()),
            image_slots: Arc::new(tokio::sync::Semaphore::new(images::CONCURRENT_IMAGES)),
            search: search::open_index(&core.config.data_dir),
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
            blames: Cache::builder()
                .max_capacity(mib(64))
                .weigher(|_, v: &Arc<keel_git::Blame>| {
                    let commits: usize = v.commits.values().map(|c| c.summary.len() + c.author.len() + c.email.len() + 64).sum();
                    (v.hunks.iter().map(|h| h.orig_path.len() + 96).sum::<usize>() + commits).min(u32::MAX as usize) as u32
                })
                .build(),
            blame_slots: tokio::sync::Semaphore::new(std::thread::available_parallelism().map_or(2, |n| n.get().div_ceil(2))),
            mergeability: Cache::new(10_000),
            unread: Cache::new(100_000),
            started: std::time::Instant::now(),
            archives: Arc::new(tokio::sync::Semaphore::new(archive::CONCURRENT_ARCHIVES)),
        }))
    }

    /// Updates the unread count shown in page headers.
    pub(crate) async fn refresh_unread(&self, user_id: i64) -> AppResult<()> {
        let n = self.core.unread_notifications(user_id).await?;
        self.unread.insert(user_id, n);
        Ok(())
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
    /// The viewer's unread notifications.
    pub unread: i64,
    /// Whether the server sends email (password resets).
    pub mail_enabled: bool,
    /// The administrators' notice for every page.
    pub banner: String,
    /// `auto` (follow the system), `light` or `dark`.
    pub theme: &'static str,
}

impl Page {
    pub fn new(state: &AppState, viewer: Option<User>, title: impl Into<String>) -> Self {
        Self {
            unread: viewer.as_ref().and_then(|v| state.unread.get(&v.id)).unwrap_or(0),
            theme: theme::current(viewer.as_ref()),
            mail_enabled: state.core.mail_enabled(),
            banner: state.core.site_settings().banner,
            title: title.into(),
            site_name: state.core.config.site_name.clone(),
            viewer,
            registration_open: state.core.registration_open(),
            css_url: state.assets.css_url,
            js_url: state.assets.js_url,
        }
    }
}

pub fn router(state: AppState) -> Router {
    let web = Router::new()
        .route("/", get(pages::home))
        .route("/login", get(pages::login_form).post(pages::login))
        .route("/login/2fa", get(twofactor::second_form).post(twofactor::second))
        .route("/settings/security", get(twofactor::security))
        .route("/settings/security/setup", post(twofactor::setup))
        .route("/settings/security/enable", post(twofactor::enable))
        .route("/settings/security/disable", post(twofactor::disable))
        .route("/settings/security/recovery", post(twofactor::recovery))
        .route("/register", get(pages::register_form).post(pages::register))
        .route("/logout", post(pages::logout))
        .route("/theme", post(theme::set))
        .route("/new", get(pages::new_repo_form).post(pages::new_repo))
        .route("/org/new", get(orgs::new_form).post(orgs::create))
        .route("/org/{org}/settings", get(orgs::settings))
        .route("/org/{org}/members", post(orgs::set_member))
        .route("/org/{org}/members/{user}/remove", post(orgs::remove_member))
        .route("/org/{org}/delete", post(orgs::delete))
        .route("/org/{org}/settings/hooks", get(hooks::org_index).post(hooks::org_create))
        .route("/org/{org}/settings/hooks/{id}", get(hooks::org_show).post(hooks::org_update))
        .route("/org/{org}/settings/hooks/{id}/delete", post(hooks::org_delete))
        .route("/org/{org}/settings/hooks/{id}/test", post(hooks::org_test))
        .route("/org/{org}/settings/hooks/{id}/deliveries/{delivery}/redeliver", post(hooks::org_redeliver))
        .route("/admin", get(admin::overview))
        .route("/admin/settings", post(admin::save_settings))
        .route("/admin/users", get(admin::users).post(admin::create_user))
        .route("/admin/users/{id}/admin", post(admin::set_admin))
        .route("/admin/users/{id}/disable", post(admin::set_disabled))
        .route("/admin/users/{id}/delete", post(admin::delete_user))
        .route("/admin/users/{id}/2fa", post(admin::disable_two_factor))
        .route("/admin/repos", get(admin::repos))
        .route("/search", get(search::global))
        .route("/explore", get(explore::index))
        .route("/new/import", get(import::form).post(import::create))
        .route("/issues", get(issue_search::issues))
        .route("/pulls", get(issue_search::pulls))
        .route("/notifications", get(notifications::inbox))
        .route("/notifications/read", post(notifications::mark_all_read))
        .route("/notifications/{id}/read", post(notifications::mark_read))
        .route("/settings", get(settings::index))
        .route("/settings/profile", get(profile::edit))
        .route("/settings/profile/avatar", post(profile::upload_avatar))
        .route("/settings/profile/avatar/delete", post(profile::remove_avatar))
        .route("/settings/profile/pins", post(profile::pins))
        .route("/org/{org}/settings/profile", get(profile::edit_org))
        .route("/org/{org}/settings/profile/avatar", post(profile::upload_org_avatar))
        .route("/org/{org}/settings/profile/avatar/delete", post(profile::remove_org_avatar))
        .route("/org/{org}/settings/profile/pins", post(profile::org_pins))
        .route("/avatars/{name}", get(profile::avatar))
        .route("/settings/account", get(settings::account))
        .route("/settings/account/email", post(settings::change_email))
        .route("/settings/account/password", post(settings::change_password))
        .route("/settings/account/verify", post(settings::send_verification))
        .route("/settings/notifications", get(settings::notifications).post(settings::save_notifications))
        .route("/password/forgot", get(password::forgot_form).post(password::forgot))
        .route("/password/reset", get(password::reset_form).post(password::reset))
        .route("/email/verify", get(password::verify_form).post(password::verify))
        .route("/settings/keys", get(settings::keys).post(settings::add_key))
        .route("/settings/keys/{id}/delete", post(settings::delete_key))
        .route("/settings/gpg-keys", post(settings::add_gpg_key))
        .route("/settings/gpg-keys/{id}/delete", post(settings::delete_gpg_key))
        .route("/settings/tokens", get(settings::tokens).post(settings::create_token))
        .route("/settings/tokens/{id}/delete", post(settings::delete_token))
        .route("/static/{file}", get(assets::serve))
        .route("/{owner}", get(pages::user))
        .route("/{owner}/{repo}", get(repo::home))
        .route("/{owner}/{repo}/tree/{*rest}", get(repo::tree))
        .route("/{owner}/{repo}/blob/{*rest}", get(repo::blob))
        .route("/{owner}/{repo}/raw/{*rest}", get(repo::raw))
        .route("/{owner}/{repo}/blame/{*rest}", get(blame::blame))
        .route("/{owner}/{repo}/archive/{*rest}", get(archive::download))
        .route("/{owner}/{repo}/last-commits/{*rest}", get(repo::last_commits_json))
        .route("/{owner}/{repo}/commits", get(repo::commits_default))
        .route("/{owner}/{repo}/commits/{*rest}", get(repo::commits))
        .route("/{owner}/{repo}/commit/{sha}", get(repo::commit))
        .route("/{owner}/{repo}/branches", get(repo::refs))
        .route("/{owner}/{repo}/settings", get(repo::settings).post(repo::update_settings))
        .route("/{owner}/{repo}/delete", post(repo::delete))
        .route("/{owner}/{repo}/rename", post(repo::rename))
        .route("/{owner}/{repo}/settings/mirror", post(repo::update_mirror))
        .route("/{owner}/{repo}/settings/mirror/stop", post(repo::stop_mirroring))
        .route("/{owner}/{repo}/mirror/sync", post(repo::sync_mirror))
        .route("/{owner}/{repo}/settings/push-mirrors", post(repo::add_push_mirror))
        .route("/{owner}/{repo}/settings/push-mirrors/{id}/push", post(repo::push_now))
        .route("/{owner}/{repo}/settings/push-mirrors/{id}/delete", post(repo::delete_push_mirror))
        .route("/{owner}/{repo}/settings/lfs-locks/{id}/unlock", post(repo::unlock_file))
        .route("/{owner}/{repo}/transfer", post(repo::transfer))
        .route("/{owner}/{repo}/archive", post(repo::archive))
        .route("/{owner}/{repo}/search", get(search::in_repo))
        .route("/{owner}/{repo}/pulls", get(pulls::list).post(pulls::create))
        .route("/{owner}/{repo}/compare", get(pulls::compare_index))
        .route("/{owner}/{repo}/compare/{*spec}", get(pulls::compare))
        .route("/{owner}/{repo}/graphs/contributors", get(insights::contributors))
        .route("/{owner}/{repo}/graphs/commit-activity", get(insights::commit_activity))
        .route("/{owner}/{repo}/pull/{number}", get(pulls::show))
        .route("/{owner}/{repo}/pull/{number}/commits", get(pulls::commits))
        .route("/{owner}/{repo}/pull/{number}/files", get(pulls::files))
        .route("/{owner}/{repo}/pull/{number}/merge", post(pulls::merge))
        .route("/{owner}/{repo}/pull/{number}/ready", post(pulls::ready))
        .route("/{owner}/{repo}/pull/{number}/draft", post(pulls::to_draft))
        .route("/{owner}/{repo}/pull/{number}/auto-merge", post(pulls::auto_merge))
        .route("/{owner}/{repo}/pull/{number}/auto-merge/disable", post(pulls::stop_auto_merge))
        .route("/{owner}/{repo}/pull/{number}/comments", post(pulls::inline_comment))
        .route("/{owner}/{repo}/pull/{number}/comments/{id}/reply", post(pulls::reply))
        .route("/{owner}/{repo}/pull/{number}/review", post(pulls::review))
        .route("/{owner}/{repo}/watch", post(notifications::watch))
        .route("/{owner}/{repo}/star", post(stars::star))
        .route("/{owner}/{repo}/stargazers", get(stars::stargazers))
        .route("/{owner}/{repo}/issues/{number}/subscription", post(notifications::subscribe))
        .route("/{owner}/{repo}/settings/hooks", get(hooks::repo_index).post(hooks::repo_create))
        .route("/{owner}/{repo}/settings/hooks/{id}", get(hooks::repo_show).post(hooks::repo_update))
        .route("/{owner}/{repo}/settings/hooks/{id}/delete", post(hooks::repo_delete))
        .route("/{owner}/{repo}/settings/hooks/{id}/test", post(hooks::repo_test))
        .route("/{owner}/{repo}/settings/hooks/{id}/deliveries/{delivery}/redeliver", post(hooks::repo_redeliver))
        .route("/{owner}/{repo}/settings/branches", get(protection::index).post(protection::create))
        .route("/{owner}/{repo}/settings/keys", get(deploy_keys::index).post(deploy_keys::create))
        .route("/{owner}/{repo}/settings/keys/{id}/delete", post(deploy_keys::delete))
        .route("/{owner}/{repo}/settings/branches/{id}", post(protection::update))
        .route("/{owner}/{repo}/settings/branches/{id}/delete", post(protection::delete))
        .route("/{owner}/{repo}/milestones", get(milestones::index).post(milestones::create))
        .route("/{owner}/{repo}/milestones/new", get(milestones::new_form))
        .route("/{owner}/{repo}/milestones/{id}/edit", get(milestones::edit_form).post(milestones::update))
        .route("/{owner}/{repo}/milestones/{id}/state", post(milestones::set_state))
        .route("/{owner}/{repo}/milestones/{id}/delete", post(milestones::delete))
        .route("/{owner}/{repo}/issues/{number}/milestone", post(milestones::set_issue_milestone))
        .route("/{owner}/{repo}/issues/{number}/projects", post(projects::add_issue))
        .route("/{owner}/{repo}/projects", get(projects::index).post(projects::create))
        .route("/{owner}/{repo}/projects/new", get(projects::new_form))
        .route("/{owner}/{repo}/projects/{number}", get(projects::board))
        .route("/{owner}/{repo}/projects/{number}/edit", get(projects::edit_form).post(projects::edit))
        .route("/{owner}/{repo}/projects/{number}/state", post(projects::set_state))
        .route("/{owner}/{repo}/projects/{number}/delete", post(projects::delete))
        .route("/{owner}/{repo}/projects/{number}/columns", post(projects::add_column))
        .route("/{owner}/{repo}/projects/{number}/columns/{id}", post(projects::edit_column))
        .route("/{owner}/{repo}/projects/{number}/columns/{id}/move", post(projects::move_column))
        .route("/{owner}/{repo}/projects/{number}/columns/{id}/delete", post(projects::delete_column))
        .route("/{owner}/{repo}/projects/{number}/cards", post(projects::add_card))
        .route("/{owner}/{repo}/projects/{number}/cards/{id}/move", post(projects::move_card))
        .route("/{owner}/{repo}/projects/{number}/cards/{id}/note", post(projects::edit_note))
        .route("/{owner}/{repo}/projects/{number}/cards/{id}/delete", post(projects::delete_card))
        .route("/org/{org}/projects", get(projects::index).post(projects::create))
        .route("/org/{org}/projects/new", get(projects::new_form))
        .route("/org/{org}/projects/{number}", get(projects::board))
        .route("/org/{org}/projects/{number}/edit", get(projects::edit_form).post(projects::edit))
        .route("/org/{org}/projects/{number}/state", post(projects::set_state))
        .route("/org/{org}/projects/{number}/delete", post(projects::delete))
        .route("/org/{org}/projects/{number}/columns", post(projects::add_column))
        .route("/org/{org}/projects/{number}/columns/{id}", post(projects::edit_column))
        .route("/org/{org}/projects/{number}/columns/{id}/move", post(projects::move_column))
        .route("/org/{org}/projects/{number}/columns/{id}/delete", post(projects::delete_column))
        .route("/org/{org}/projects/{number}/cards", post(projects::add_card))
        .route("/org/{org}/projects/{number}/cards/{id}/move", post(projects::move_card))
        .route("/org/{org}/projects/{number}/cards/{id}/note", post(projects::edit_note))
        .route("/org/{org}/projects/{number}/cards/{id}/delete", post(projects::delete_card))
        .route("/{owner}/{repo}/issues/{number}/reviewers", post(labels::set_reviewers))
        .route("/{owner}/{repo}/pull/{number}/comments/{id}/apply", post(pulls::apply_suggestion))
        .route("/{owner}/{repo}/releases", get(releases::list).post(releases::create))
        .route("/{owner}/{repo}/releases/new", get(releases::new_form))
        .route("/{owner}/{repo}/releases/tag/{*tag}", get(releases::show))
        .route("/{owner}/{repo}/releases/download/{*rest}", get(releases::download))
        .route("/{owner}/{repo}/releases/{id}/edit", get(releases::edit_form).post(releases::update))
        .route("/{owner}/{repo}/releases/{id}/delete", post(releases::delete))
        .route(
            "/{owner}/{repo}/releases/{id}/assets",
            post(releases::upload).layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route("/{owner}/{repo}/releases/{id}/assets/{asset}/delete", post(releases::delete_asset))
        .route("/{owner}/{repo}/wiki", get(wiki::index).post(wiki::create))
        .route("/{owner}/{repo}/wiki/_pages", get(wiki::pages))
        .route("/{owner}/{repo}/wiki/_new", get(wiki::new_form))
        .route("/{owner}/{repo}/wiki/_raw/{*path}", get(wiki::raw))
        .route("/{owner}/{repo}/wiki/{page}", get(wiki::show))
        .route("/{owner}/{repo}/wiki/{page}/edit", get(wiki::edit_form).post(wiki::update))
        .route("/{owner}/{repo}/wiki/{page}/delete", post(wiki::delete))
        .route("/{owner}/{repo}/wiki/{page}/history", get(wiki::history))
        .route("/{owner}/{repo}/edit/{*rest}", get(edit::edit_form).post(edit::save))
        .route("/{owner}/{repo}/new/{*rest}", get(edit::new_form).post(edit::create))
        .route("/{owner}/{repo}/delete/{*rest}", get(edit::delete_form).post(edit::delete))
        .route(
            "/{owner}/{repo}/upload/{*rest}",
            get(edit::upload_form).post(edit::upload).layer(axum::extract::DefaultBodyLimit::max(edit::MAX_UPLOAD + 1024 * 1024)),
        )
        .route("/{owner}/{repo}/fork", get(forks::form).post(forks::create))
        .route("/{owner}/{repo}/forks", get(forks::list))
        .route("/{owner}/{repo}/labels", get(labels::index).post(labels::create))
        .route("/{owner}/{repo}/labels/{id}", post(labels::update))
        .route("/{owner}/{repo}/labels/{id}/delete", post(labels::delete))
        .route("/{owner}/{repo}/issues/{number}/labels", post(labels::set_issue_labels))
        .route("/{owner}/{repo}/issues/{number}/assignees", post(labels::set_assignees))
        .route("/{owner}/{repo}/issues", get(issues::list).post(issues::create))
        .route("/{owner}/{repo}/issues/new", get(issues::new_form))
        .route("/{owner}/{repo}/issues/{number}", get(issues::show))
        .route("/{owner}/{repo}/issues/{number}/edit", get(issues::edit_form).post(issues::edit))
        .route("/{owner}/{repo}/issues/{number}/comments", post(issues::comment))
        .route("/{owner}/{repo}/issues/{number}/reactions", post(issues::react))
        .route(
            "/{owner}/{repo}/issues/{number}/comments/{id}/edit",
            get(issues::edit_comment_form).post(issues::edit_comment),
        )
        .route("/{owner}/{repo}/issues/{number}/comments/{id}/delete", post(issues::delete_comment))
        .route("/{owner}/{repo}/collaborators", post(repo::set_collaborator))
        .route("/{owner}/{repo}/collaborators/{user}/remove", post(repo::remove_collaborator))
        .fallback(|| async { AppError::NotFound })
        .layer(middleware::from_fn_with_state(state.clone(), web_guard))
        .layer(CompressionLayer::new());

    // Git traffic is already compressed and must not get browser headers.
    let git = Router::new()
        .route("/{owner}/{repo}/info/refs", get(githttp::info_refs))
        .route("/{owner}/{repo}/git-upload-pack", post(githttp::upload_pack))
        .route("/{owner}/{repo}/git-receive-pack", post(githttp::receive_pack))
        .route("/{owner}/{repo}/info/lfs/objects/batch", post(lfs::batch))
        .route("/{owner}/{repo}/info/lfs/objects/{oid}", get(lfs::download).put(lfs::upload))
        .route("/{owner}/{repo}/info/lfs/locks/verify", post(lfs::verify_locks))
        .route("/{owner}/{repo}/info/lfs/locks", get(lfs::list_locks).post(lfs::create_lock))
        .route("/{owner}/{repo}/info/lfs/locks/{id}/unlock", post(lfs::unlock))
        .route("/healthz", get(health))
        .route("/_image", get(images::image))
        .route("/metrics", get(metrics))
        .route("/signing-key.pub", get(signing_key));

    // The API authenticates with tokens only, so needs no CSRF check.
    let api = api::routes().layer(CompressionLayer::new());

    web.merge(git)
        .merge(api)
        .layer(middleware::from_fn_with_state(state.clone(), track))
        .with_state(state)
        .layer(TraceLayer::new_for_http())
}

/// Counts requests and their durations, by route pattern.
async fn track(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let started = std::time::Instant::now();
    let method = req.method().clone();
    let route = req
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map_or("unmatched", |p| p.as_str())
        .to_owned();
    let res = next.run(req).await;
    state.core.metrics().http_request(method.as_str(), &route, res.status().as_u16(), started.elapsed().as_secs_f64());
    res
}

/// Whether the request carries the configured metrics token
/// (`Authorization: Bearer …`).
fn has_metrics_token(state: &AppState, headers: &axum::http::HeaderMap) -> bool {
    let Some(token) = state.core.config.metrics_token.as_deref() else { return false };
    let given = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or_default();
    // Constant-time, so the token can't be guessed byte by byte.
    given.len() == token.len() && given.bytes().zip(token.bytes()).fold(0, |a, (x, y)| a | (x ^ y)) == 0
}

/// Prometheus metrics, for requests with the configured bearer token.
async fn metrics(State(state): State<AppState>, headers: axum::http::HeaderMap) -> Response {
    if state.core.config.metrics_token.is_none() {
        return StatusCode::NOT_FOUND.into_response();
    }
    if !has_metrics_token(&state, &headers) {
        return (StatusCode::UNAUTHORIZED, [(header::WWW_AUTHENTICATE, "Bearer")], "Unauthorized\n").into_response();
    }
    match state.core.metrics_text().await {
        Ok(text) => ([(header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")], text).into_response(),
        Err(e) => {
            tracing::error!("metrics: {e}");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// For monitoring and load balancers: 200 if the server can use its
/// database, 503 otherwise. The version and uptime only go to requests
/// with the metrics token: to others the version would say which known
/// vulnerabilities to try.
async fn health(State(state): State<AppState>, headers: axum::http::HeaderMap) -> Response {
    let database = state.core.database_ok().await;
    let mut body = serde_json::json!({ "status": if database { "ok" } else { "unavailable" } });
    if has_metrics_token(&state, &headers) {
        body["version"] = env!("CARGO_PKG_VERSION").into();
        body["database"] = database.into();
        body["uptime_seconds"] = state.started.elapsed().as_secs().into();
    }
    let status = if database { StatusCode::OK } else { StatusCode::SERVICE_UNAVAILABLE };
    (status, [(header::CACHE_CONTROL, "no-store")], axum::Json(body)).into_response()
}

/// The public key commits keel makes are signed with, for git's
/// `gpg.ssh.allowedSignersFile`.
async fn signing_key(State(state): State<AppState>) -> Response {
    let headers = [(header::CONTENT_TYPE, "text/plain; charset=utf-8")];
    (headers, format!("{}\n", state.core.signing_public_key())).into_response()
}

/// Security headers for HTML pages, plus CSRF protection: state-changing
/// requests must come from our own origin. Session cookies are also
/// `SameSite=Lax`, so this is defence in depth.
async fn web_guard(State(state): State<AppState>, req: Request, next: Next) -> Response {
    let safe = req.method() == Method::GET || req.method() == Method::HEAD;
    if !safe && !same_origin(&req, &state.core.config.base_url) {
        return (StatusCode::FORBIDDEN, "Cross-origin request blocked").into_response();
    }
    let path = req.uri().path().to_owned();
    let query = req.uri().query().map(str::to_owned);
    let session = session::token_from(req.headers()).map(str::to_owned);
    let theme = theme::from_cookie(req.headers());
    let mut res = theme::with_cookie(theme, next.run(req)).await;
    // Pages of renamed or transferred repositories move along.
    if safe
        && res.status() == StatusCode::NOT_FOUND
        && let Some(to) = moved(&state, &path, query.as_deref(), session.as_deref()).await
    {
        res = axum::response::Redirect::temporary(&to).into_response();
    }
    let headers = res.headers_mut();
    let defaults = [
        (
            header::CONTENT_SECURITY_POLICY,
            "default-src 'none'; style-src 'self'; script-src 'self'; connect-src 'self'; img-src 'self' data:; \
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

/// Where `path` is now if it's under the old name of a repository the
/// viewer can see.
async fn moved(state: &AppState, path: &str, query: Option<&str>, session: Option<&str>) -> Option<String> {
    let mut parts = path.trim_start_matches('/').splitn(3, '/');
    let (owner, name, rest) = (parts.next()?, parts.next()?, parts.next());
    let repo = state.core.repo_redirect(owner, name).await.ok()??;
    let viewer = match session {
        Some(token) => state.core.session_user(token).await.ok()?,
        None => None,
    };
    if state.core.access(&repo, viewer.as_ref()).await.ok()? == keel_core::Access::None {
        return None;
    }
    let mut to = format!("/{}/{}", repo.owner, repo.name);
    if let Some(rest) = rest {
        to.push('/');
        to.push_str(rest);
    }
    if let Some(q) = query {
        to.push('?');
        to.push_str(q);
    }
    Some(to)
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
        // Behind TLS, a page on plain HTTP (where anyone on the way can
        // change it) doesn't count.
        (Some(source), Some(host)) => source.split_once("://").is_some_and(|(scheme, rest)| {
            (scheme == "https" || !base_url.starts_with("https://")) && rest.split('/').next() == Some(host)
        }),
        (Some(_), None) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins() {
        let req = |host: &str, origin: Option<&str>| {
            let mut r = Request::builder().header(header::HOST, host);
            if let Some(o) = origin {
                r = r.header(header::ORIGIN, o);
            }
            r.body(axum::body::Body::empty()).unwrap()
        };
        let base = "https://git.example";
        assert!(same_origin(&req("git.example", None), base));
        assert!(same_origin(&req("git.example", Some("https://git.example")), base));
        assert!(same_origin(&req("internal:3000", Some("https://internal:3000")), base), "behind a proxy");
        assert!(!same_origin(&req("git.example", Some("http://git.example")), base), "plain HTTP");
        assert!(!same_origin(&req("git.example", Some("https://evil.example")), base));
        assert!(same_origin(&req("localhost:3000", Some("http://localhost:3000")), "http://localhost:3000"));
    }
}
