//! The notification inbox, watching repositories and subscribing to issues.

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Redirect};
use serde::Deserialize;

use crate::error::{AppResult, html};
use crate::issues::{issue_url, load_issue};
use crate::render::{datetime, relative_time};
use crate::repo::load;
use crate::session::SignedIn;
use crate::{AppState, Page};

const PER_PAGE: i64 = 50;

pub(crate) struct NotificationView {
    pub id: i64,
    pub repo: String,
    pub url: String,
    pub number: i64,
    pub title: String,
    pub is_pull: bool,
    pub state: String,
    pub reason: &'static str,
    pub summary: String,
    pub when: String,
    pub when_full: String,
    pub unread: bool,
}

fn reason_label(reason: &str) -> &'static str {
    match reason {
        "assigned" => "Assigned",
        "mentioned" => "Mentioned",
        "author" => "Author",
        "participating" => "Participating",
        "subscribed" => "Subscribed",
        _ => "Watching",
    }
}

#[derive(Template)]
#[template(path = "notifications.html")]
struct InboxPage {
    page: Page,
    items: Vec<NotificationView>,
    show_all: bool,
    unread: i64,
    newer_url: Option<String>,
    older_url: Option<String>,
}

#[derive(Deserialize)]
pub struct InboxQuery {
    all: Option<String>,
    page: Option<i64>,
}

pub async fn inbox(State(state): State<AppState>, SignedIn(user): SignedIn, Query(q): Query<InboxQuery>) -> AppResult {
    let show_all = q.all.is_some();
    let page_no = q.page.unwrap_or(1).max(1);
    let mut rows = state
        .core
        .notifications(user.id, !show_all, (page_no - 1) * PER_PAGE, PER_PAGE + 1)
        .await?;
    let has_more = rows.len() as i64 > PER_PAGE;
    rows.truncate(PER_PAGE as usize);
    let here = if show_all { "/notifications?all=1&" } else { "/notifications?" };
    let items = rows
        .into_iter()
        .map(|n| {
            let kind = if n.is_pull { "pull" } else { "issues" };
            NotificationView {
                url: format!("/{}/{}/{kind}/{}", n.owner, n.repo, n.number),
                repo: format!("{}/{}", n.owner, n.repo),
                id: n.id,
                number: n.number,
                title: n.title,
                is_pull: n.is_pull,
                state: n.state,
                reason: reason_label(&n.reason),
                summary: match n.actor {
                    Some(actor) => format!("{actor} {}", n.activity),
                    None => n.activity,
                },
                when: relative_time(n.updated_at),
                when_full: datetime(n.updated_at),
                unread: n.unread,
            }
        })
        .collect();
    let unread = state.core.unread_notifications(user.id).await?;
    html(InboxPage {
        page: Page::new(&state, Some(user), "Notifications"),
        items,
        show_all,
        unread,
        newer_url: (page_no > 1).then(|| format!("{here}page={}", page_no - 1)),
        older_url: has_more.then(|| format!("{here}page={}", page_no + 1)),
    })
}

pub async fn mark_all_read(State(state): State<AppState>, SignedIn(user): SignedIn) -> AppResult {
    state.core.mark_all_read(user.id).await?;
    state.refresh_unread(user.id).await?;
    Ok(Redirect::to("/notifications").into_response())
}

pub async fn mark_read(State(state): State<AppState>, SignedIn(user): SignedIn, Path(id): Path<i64>) -> AppResult {
    state.core.mark_notification_read(user.id, id).await?;
    state.refresh_unread(user.id).await?;
    Ok(Redirect::to("/notifications").into_response())
}

#[derive(Deserialize)]
pub struct ToggleForm {
    /// `on` or `off`.
    to: String,
}

pub async fn watch(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name)): Path<(String, String)>,
    Form(f): Form<ToggleForm>,
) -> AppResult {
    let (repo, _) = load(&state, Some(&user), &owner, &name).await?;
    state.core.set_watching(user.id, repo.id, f.to == "on").await?;
    Ok(Redirect::to(&format!("/{}/{}", repo.owner, repo.name)).into_response())
}

pub async fn subscribe(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, number)): Path<(String, String, i64)>,
    Form(f): Form<ToggleForm>,
) -> AppResult {
    let (repo, _) = load(&state, Some(&user), &owner, &name).await?;
    let issue = load_issue(&state, &repo, number).await?;
    state.core.set_subscription(user.id, issue.id, f.to == "on").await?;
    Ok(Redirect::to(&issue_url(&repo, &issue)).into_response())
}
