//! Webhook settings: adding, editing and testing hooks, and the delivery
//! log.

use askama::Template;
use axum::extract::{Path, RawForm, State};
use axum::response::{IntoResponse, Redirect};
use keel_core::{Access, Delivery, Repo, User, WEBHOOK_EVENTS, Webhook};

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

/// The repository, if `user` may administer it.
async fn admin(state: &AppState, user: &User, owner: &str, name: &str) -> AppResult<Repo> {
    let (repo, access) = load(state, Some(user), owner, name).await?;
    if access != Access::Admin {
        return Err(AppError::Forbidden);
    }
    Ok(repo)
}

fn base(repo: &Repo) -> String {
    format!("/{}/{}/settings/hooks", repo.owner, repo.name)
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
    repo: RepoView,
    hooks: Vec<HookView>,
    events: EventChoices,
    error: Option<String>,
    url: String,
    allow_private: bool,
}

pub async fn index(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name)): Path<(String, String)>,
) -> AppResult {
    let repo = admin(&state, &user, &owner, &name).await?;
    hooks_page(&state, user, &repo, None, String::new(), vec!["push".into()]).await
}

async fn hooks_page(
    state: &AppState,
    user: User,
    repo: &Repo,
    error: Option<String>,
    url: String,
    chosen: Vec<String>,
) -> AppResult {
    let mut hooks = Vec::new();
    for h in state.core.webhooks(repo.id).await? {
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
        repo: RepoView::new(state, repo, Access::Admin, Some(&user), "settings", "").await?,
        page: Page::new(state, Some(user), format!("Webhooks · {}/{}", repo.owner, repo.name)),
        hooks,
        events: EventChoices::new(&chosen),
        error,
        url,
        allow_private: state.core.config.webhook_allow_private,
    })
}

pub async fn create(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name)): Path<(String, String)>,
    RawForm(body): RawForm,
) -> AppResult {
    let repo = admin(&state, &user, &owner, &name).await?;
    let f = HookForm::parse(&body);
    match state.core.create_webhook(repo.id, &f.url, &f.secret, &f.events, f.active).await {
        Ok(hook) => Ok(Redirect::to(&format!("{}/{}", base(&repo), hook.id)).into_response()),
        Err(keel_core::Error::Invalid(msg)) => hooks_page(&state, user, &repo, Some(msg), f.url, f.events).await,
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
    repo: RepoView,
    hook: Webhook,
    events: EventChoices,
    deliveries: Vec<DeliveryView>,
    error: Option<String>,
    url: String,
    hooks_url: String,
}

async fn load_hook(state: &AppState, user: &User, owner: &str, name: &str, id: i64) -> AppResult<(Repo, Webhook)> {
    let repo = admin(state, user, owner, name).await?;
    let hook = state.core.webhook(repo.id, id).await?.ok_or(AppError::NotFound)?;
    Ok((repo, hook))
}

async fn hook_page(state: &AppState, user: User, repo: &Repo, hook: Webhook, error: Option<String>) -> AppResult {
    let deliveries = state.core.deliveries(hook.id, 30).await?.into_iter().map(DeliveryView::new).collect();
    html(HookPage {
        repo: RepoView::new(state, repo, Access::Admin, Some(&user), "settings", "").await?,
        page: Page::new(state, Some(user), format!("Webhook · {}/{}", repo.owner, repo.name)),
        events: EventChoices::new(&hook.event_list()),
        url: format!("{}/{}", base(repo), hook.id),
        hooks_url: base(repo),
        hook,
        deliveries,
        error,
    })
}

pub async fn show(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> AppResult {
    let (repo, hook) = load_hook(&state, &user, &owner, &name, id).await?;
    hook_page(&state, user, &repo, hook, None).await
}

pub async fn update(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, id)): Path<(String, String, i64)>,
    RawForm(body): RawForm,
) -> AppResult {
    let (repo, hook) = load_hook(&state, &user, &owner, &name, id).await?;
    let f = HookForm::parse(&body);
    // An empty secret field keeps the secret; "remove" clears it.
    let clear = !form_values(&body, "clear_secret").is_empty();
    let secret = match (clear, f.secret.is_empty()) {
        (true, _) => Some(""),
        (false, true) => None,
        (false, false) => Some(f.secret.as_str()),
    };
    match state.core.update_webhook(&hook, &f.url, secret, &f.events, f.active).await {
        Ok(()) => Ok(Redirect::to(&format!("{}/{}", base(&repo), hook.id)).into_response()),
        Err(keel_core::Error::Invalid(msg)) => hook_page(&state, user, &repo, hook, Some(msg)).await,
        Err(e) => Err(e.into()),
    }
}

pub async fn delete(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> AppResult {
    let (repo, hook) = load_hook(&state, &user, &owner, &name, id).await?;
    state.core.delete_webhook(&hook).await?;
    Ok(Redirect::to(&base(&repo)).into_response())
}

pub async fn test(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> AppResult {
    let (repo, hook) = load_hook(&state, &user, &owner, &name, id).await?;
    state.core.ping_webhook(&hook).await?;
    Ok(Redirect::to(&format!("{}/{}#deliveries", base(&repo), hook.id)).into_response())
}

pub async fn redeliver(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, id, delivery)): Path<(String, String, i64, i64)>,
) -> AppResult {
    let (repo, hook) = load_hook(&state, &user, &owner, &name, id).await?;
    let d = state.core.delivery(hook.id, delivery).await?.ok_or(AppError::NotFound)?;
    state.core.redeliver(&d).await?;
    Ok(Redirect::to(&format!("{}/{}#deliveries", base(&repo), hook.id)).into_response())
}
