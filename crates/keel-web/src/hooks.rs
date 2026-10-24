//! Webhook settings: adding, editing and testing hooks, and the delivery
//! log.

use askama::Template;
use axum::extract::{Path, RawForm, State};
use axum::response::{IntoResponse, Redirect};
use keel_core::{Access, Delivery, HookOwner, Repo, User, WEBHOOK_EVENTS, Webhook};

use crate::error::{AppResult, html};
use crate::labels::form_values;
use crate::render::{datetime, relative_time};
use crate::repo::{RepoView, load};
use crate::session::SignedIn;
use crate::{AppError, AppState, Page};

pub(crate) struct HookView {
    pub id: i64,
    pub url: String,
    pub events: String,
    pub active: bool,
    /// The latest delivery's outcome: ok, failed or pending.
    pub last: Option<&'static str>,
}

pub(crate) struct DeliveryView {
    pub id: i64,
    pub guid: String,
    pub event: String,
    pub status: String,
    pub response_status: Option<i64>,
    pub when: String,
    pub when_full: String,
    pub attempts: i64,
    pub duration_ms: Option<i64>,
    pub payload: String,
    pub response_body: String,
    pub error: Option<String>,
    pub next_attempt: Option<String>,
}

impl DeliveryView {
    fn new(d: Delivery) -> Self {
        let payload = serde_json::from_str::<serde_json::Value>(&d.payload)
            .and_then(|v| serde_json::to_string_pretty(&v))
            .unwrap_or(d.payload);
        let event = if d.action.is_empty() { d.event } else { format!("{}.{}", d.event, d.action) };
        Self {
            id: d.id,
            guid: d.guid,
            event,
            response_status: d.response_status,
            when: relative_time(d.created_at),
            when_full: datetime(d.created_at),
            attempts: d.attempts,
            duration_ms: d.duration_ms,
            payload,
            response_body: d.response_body.unwrap_or_default(),
            error: d.error,
            next_attempt: (d.status == "pending" && d.attempts > 0).then(|| relative_time(d.next_attempt_at)),
            status: d.status,
        }
    }
}

/// The event choices of the hook form.
pub(crate) struct EventChoices {
    /// `push`, `all` or `some`.
    pub which: &'static str,
    pub rows: Vec<(&'static str, &'static str, bool)>,
}

impl EventChoices {
    fn new(chosen: &[String]) -> Self {
        let which = match chosen {
            [only] if only == "push" => "push",
            _ if chosen.len() == WEBHOOK_EVENTS.len() => "all",
            _ => "some",
        };
        let rows = WEBHOOK_EVENTS.iter().map(|(e, about)| (*e, *about, chosen.iter().any(|c| c == e))).collect();
        Self { which, rows }
    }
}

/// Whose hooks a page manages: a repository's or an organization's.
pub(crate) enum Scope {
    Repo(Repo),
    Org(User),
}

impl Scope {
    fn owner(&self) -> HookOwner {
        match self {
            Self::Repo(r) => HookOwner::Repo(r.id),
            Self::Org(o) => HookOwner::Org(o.id),
        }
    }

    fn settings_url(&self) -> String {
        match self {
            Self::Repo(r) => format!("/{}/{}/settings", r.owner, r.name),
            Self::Org(o) => format!("/org/{}/settings", o.username),
        }
    }

    fn base(&self) -> String {
        format!("{}/hooks", self.settings_url())
    }

    fn name(&self) -> String {
        match self {
            Self::Repo(r) => format!("{}/{}", r.owner, r.name),
            Self::Org(o) => o.username.clone(),
        }
    }

    /// The repository header, or `None` for an organization.
    async fn view(&self, state: &AppState, user: &User) -> AppResult<Option<RepoView>> {
        Ok(match self {
            Self::Repo(r) => Some(RepoView::new(state, r, Access::Admin, Some(user), "settings", "").await?),
            Self::Org(_) => None,
        })
    }
}

/// The repository, if `user` may administer it.
async fn repo_scope(state: &AppState, user: &User, owner: &str, name: &str) -> AppResult<Scope> {
    let (repo, access) = load(state, Some(user), owner, name).await?;
    if access != Access::Admin {
        return Err(AppError::Forbidden);
    }
    Ok(Scope::Repo(repo))
}

/// The organization, if `user` owns it.
async fn org_scope(state: &AppState, user: &User, org: &str) -> AppResult<Scope> {
    Ok(Scope::Org(crate::orgs::owned_org(state, user, org).await?))
}

/// The form's fields: URL, secret, events and whether it's active.
struct HookForm {
    url: String,
    secret: String,
    events: Vec<String>,
    active: bool,
}

impl HookForm {
    fn parse(body: &[u8]) -> Self {
        let one = |k| form_values(body, k).into_iter().next().unwrap_or_default();
        let mut events = form_values(body, "events");
        if one("which") == "all" {
            events = WEBHOOK_EVENTS.iter().map(|(e, _)| (*e).to_owned()).collect();
        } else if one("which") == "push" {
            events = vec!["push".into()];
        }
        Self { url: one("url"), secret: one("secret"), events, active: !one("active").is_empty() }
    }
}

// ---------------------------------------------------------------------------
// List and create
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "repo/hooks.html")]
struct HooksPage {
    page: Page,
    repo: Option<RepoView>,
    /// The scope's name, e.g. `ada/engine` or `acme`.
    name: String,
    is_org: bool,
    settings_url: String,
    base_url: String,
    hooks: Vec<HookView>,
    events: EventChoices,
    error: Option<String>,
    url: String,
    allow_private: bool,
}

async fn hooks_page(
    state: &AppState,
    user: User,
    scope: &Scope,
    error: Option<String>,
    url: String,
    chosen: Vec<String>,
) -> AppResult {
    let mut hooks = Vec::new();
    for h in state.core.webhooks(scope.owner()).await? {
        let last = state.core.deliveries(h.id, 1).await?.into_iter().next().map(|d| match d.status.as_str() {
            "ok" => "ok",
            "failed" => "failed",
            _ if d.attempts > 0 => "failed",
            _ => "pending",
        });
        hooks.push(HookView {
            id: h.id,
            events: if h.events == "*" { "all events".into() } else { h.event_list().join(", ") },
            url: h.url,
            active: h.active,
            last,
        });
    }
    html(HooksPage {
        repo: scope.view(state, &user).await?,
        page: Page::new(state, Some(user), format!("Webhooks · {}", scope.name())),
        name: scope.name(),
        is_org: matches!(scope, Scope::Org(_)),
        settings_url: scope.settings_url(),
        base_url: scope.base(),
        hooks,
        events: EventChoices::new(&chosen),
        error,
        url,
        allow_private: state.core.config.webhook_allow_private,
    })
}

async fn index(state: AppState, user: User, scope: Scope) -> AppResult {
    hooks_page(&state, user, &scope, None, String::new(), vec!["push".into()]).await
}

async fn create(state: AppState, user: User, scope: Scope, body: &[u8]) -> AppResult {
    let f = HookForm::parse(body);
    match state.core.create_webhook(scope.owner(), &f.url, &f.secret, &f.events, f.active).await {
        Ok(hook) => Ok(Redirect::to(&format!("{}/{}", scope.base(), hook.id)).into_response()),
        Err(keel_core::Error::Invalid(msg)) => hooks_page(&state, user, &scope, Some(msg), f.url, f.events).await,
        Err(e) => Err(e.into()),
    }
}

// ---------------------------------------------------------------------------
// One hook
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "repo/hook.html")]
struct HookPage {
    page: Page,
    repo: Option<RepoView>,
    name: String,
    settings_url: String,
    hook: Webhook,
    events: EventChoices,
    deliveries: Vec<DeliveryView>,
    error: Option<String>,
    url: String,
    hooks_url: String,
}

async fn load_hook(state: &AppState, scope: &Scope, id: i64) -> AppResult<Webhook> {
    state.core.webhook(scope.owner(), id).await?.ok_or(AppError::NotFound)
}

async fn hook_page(state: &AppState, user: User, scope: &Scope, hook: Webhook, error: Option<String>) -> AppResult {
    let deliveries = state.core.deliveries(hook.id, 30).await?.into_iter().map(DeliveryView::new).collect();
    html(HookPage {
        repo: scope.view(state, &user).await?,
        page: Page::new(state, Some(user), format!("Webhook · {}", scope.name())),
        name: scope.name(),
        settings_url: scope.settings_url(),
        events: EventChoices::new(&hook.event_list()),
        url: format!("{}/{}", scope.base(), hook.id),
        hooks_url: scope.base(),
        hook,
        deliveries,
        error,
    })
}

async fn show(state: AppState, user: User, scope: Scope, id: i64) -> AppResult {
    let hook = load_hook(&state, &scope, id).await?;
    hook_page(&state, user, &scope, hook, None).await
}

async fn update(state: AppState, user: User, scope: Scope, id: i64, body: &[u8]) -> AppResult {
    let hook = load_hook(&state, &scope, id).await?;
    let f = HookForm::parse(body);
    // An empty secret field keeps the secret; "remove" clears it.
    let clear = !form_values(body, "clear_secret").is_empty();
    let secret = match (clear, f.secret.is_empty()) {
        (true, _) => Some(""),
        (false, true) => None,
        (false, false) => Some(f.secret.as_str()),
    };
    match state.core.update_webhook(&hook, &f.url, secret, &f.events, f.active).await {
        Ok(()) => Ok(Redirect::to(&format!("{}/{}", scope.base(), hook.id)).into_response()),
        Err(keel_core::Error::Invalid(msg)) => hook_page(&state, user, &scope, hook, Some(msg)).await,
        Err(e) => Err(e.into()),
    }
}

async fn delete(state: AppState, scope: Scope, id: i64) -> AppResult {
    let hook = load_hook(&state, &scope, id).await?;
    state.core.delete_webhook(&hook).await?;
    Ok(Redirect::to(&scope.base()).into_response())
}

async fn test(state: AppState, scope: Scope, id: i64) -> AppResult {
    let hook = load_hook(&state, &scope, id).await?;
    state.core.ping_webhook(&hook).await?;
    Ok(Redirect::to(&format!("{}/{}#deliveries", scope.base(), hook.id)).into_response())
}

async fn redeliver(state: AppState, scope: Scope, id: i64, delivery: i64) -> AppResult {
    let hook = load_hook(&state, &scope, id).await?;
    let d = state.core.delivery(hook.id, delivery).await?.ok_or(AppError::NotFound)?;
    state.core.redeliver(&d).await?;
    Ok(Redirect::to(&format!("{}/{}#deliveries", scope.base(), hook.id)).into_response())
}

// ---------------------------------------------------------------------------
// Routes: the same pages for repositories and organizations
// ---------------------------------------------------------------------------

type RepoPath = Path<(String, String)>;
type RepoHookPath = Path<(String, String, i64)>;

pub async fn repo_index(State(s): State<AppState>, SignedIn(u): SignedIn, Path((o, n)): RepoPath) -> AppResult {
    let scope = repo_scope(&s, &u, &o, &n).await?;
    index(s, u, scope).await
}

pub async fn repo_create(State(s): State<AppState>, SignedIn(u): SignedIn, Path((o, n)): RepoPath, RawForm(b): RawForm) -> AppResult {
    let scope = repo_scope(&s, &u, &o, &n).await?;
    create(s, u, scope, &b).await
}

pub async fn repo_show(State(s): State<AppState>, SignedIn(u): SignedIn, Path((o, n, id)): RepoHookPath) -> AppResult {
    let scope = repo_scope(&s, &u, &o, &n).await?;
    show(s, u, scope, id).await
}

pub async fn repo_update(
    State(s): State<AppState>,
    SignedIn(u): SignedIn,
    Path((o, n, id)): RepoHookPath,
    RawForm(b): RawForm,
) -> AppResult {
    let scope = repo_scope(&s, &u, &o, &n).await?;
    update(s, u, scope, id, &b).await
}

pub async fn repo_delete(State(s): State<AppState>, SignedIn(u): SignedIn, Path((o, n, id)): RepoHookPath) -> AppResult {
    let scope = repo_scope(&s, &u, &o, &n).await?;
    delete(s, scope, id).await
}

pub async fn repo_test(State(s): State<AppState>, SignedIn(u): SignedIn, Path((o, n, id)): RepoHookPath) -> AppResult {
    let scope = repo_scope(&s, &u, &o, &n).await?;
    test(s, scope, id).await
}

pub async fn repo_redeliver(
    State(s): State<AppState>,
    SignedIn(u): SignedIn,
    Path((o, n, id, d)): Path<(String, String, i64, i64)>,
) -> AppResult {
    let scope = repo_scope(&s, &u, &o, &n).await?;
    redeliver(s, scope, id, d).await
}

pub async fn org_index(State(s): State<AppState>, SignedIn(u): SignedIn, Path(org): Path<String>) -> AppResult {
    let scope = org_scope(&s, &u, &org).await?;
    index(s, u, scope).await
}

pub async fn org_create(State(s): State<AppState>, SignedIn(u): SignedIn, Path(org): Path<String>, RawForm(b): RawForm) -> AppResult {
    let scope = org_scope(&s, &u, &org).await?;
    create(s, u, scope, &b).await
}

pub async fn org_show(State(s): State<AppState>, SignedIn(u): SignedIn, Path((org, id)): Path<(String, i64)>) -> AppResult {
    let scope = org_scope(&s, &u, &org).await?;
    show(s, u, scope, id).await
}

pub async fn org_update(
    State(s): State<AppState>,
    SignedIn(u): SignedIn,
    Path((org, id)): Path<(String, i64)>,
    RawForm(b): RawForm,
) -> AppResult {
    let scope = org_scope(&s, &u, &org).await?;
    update(s, u, scope, id, &b).await
}

pub async fn org_delete(State(s): State<AppState>, SignedIn(u): SignedIn, Path((org, id)): Path<(String, i64)>) -> AppResult {
    let scope = org_scope(&s, &u, &org).await?;
    delete(s, scope, id).await
}

pub async fn org_test(State(s): State<AppState>, SignedIn(u): SignedIn, Path((org, id)): Path<(String, i64)>) -> AppResult {
    let scope = org_scope(&s, &u, &org).await?;
    test(s, scope, id).await
}

pub async fn org_redeliver(
    State(s): State<AppState>,
    SignedIn(u): SignedIn,
    Path((org, id, d)): Path<(String, i64, i64)>,
) -> AppResult {
    let scope = org_scope(&s, &u, &org).await?;
    redeliver(s, scope, id, d).await
}
