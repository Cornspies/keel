//! Avatars and pinned repositories: serving avatars, and the pages where
//! people (and organization owners) change them.

use askama::Template;
use axum::Form;
use axum::extract::{Multipart, Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use keel_core::{MAX_PINS, Repo, User};
use serde::Deserialize;

use crate::error::{AppResult, html};
use crate::orgs::owned_org;
use crate::pages::RepoCard;
use crate::session::SignedIn;
use crate::{AppError, AppState, Page};

/// Avatars are rechecked after this long (and the ETag makes that cheap).
const AVATAR_MAX_AGE: &str = "public, max-age=300";

/// An avatar: the uploaded image, or a pattern from the name (also for
/// names without an account, like deleted ones).
pub async fn avatar(State(state): State<AppState>, Path(name): Path<String>, headers: HeaderMap) -> AppResult<Response> {
    let user = state.core.user_by_name(&name).await?;
    let image = match &user {
        Some(u) => state.core.avatar_image(u).await?,
        None => None,
    };
    let etag = match (&image, &user) {
        (Some(_), Some(u)) => format!("\"{}-{}\"", u.id, u.avatar_version),
        _ => format!("\"generated-{}\"", name.to_lowercase()),
    };
    if headers.get(header::IF_NONE_MATCH).is_some_and(|v| v.as_bytes() == etag.as_bytes()) {
        return Ok(StatusCode::NOT_MODIFIED.into_response());
    }
    let (content_type, body) = match image {
        Some((t, data)) => (t, data),
        None => ("image/svg+xml".to_owned(), keel_core::identicon(&name).into_bytes()),
    };
    let mut res = body.into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_str(&content_type).map_err(|e| AppError::Internal(e.into()))?);
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(AVATAR_MAX_AGE));
    h.insert(header::ETAG, HeaderValue::from_str(&etag).map_err(|e| AppError::Internal(e.into()))?);
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(crate::repo::RAW_CSP));
    Ok(res)
}

// ---------------------------------------------------------------------------
// Editing
// ---------------------------------------------------------------------------

pub(crate) struct PinChoice {
    pub id: i64,
    pub name: String,
    pub is_private: bool,
    pub pinned: bool,
}

#[derive(Template)]
#[template(path = "settings/profile.html")]
struct ProfilePage {
    page: Page,
    nav: &'static str,
    name: String,
    avatar_version: i64,
    has_avatar: bool,
    /// Where the forms post to, e.g. `/settings/profile`.
    base: String,
    /// For an organization: its name (the page is outside the settings).
    org: Option<String>,
    choices: Vec<PinChoice>,
    max_pins: usize,
    error: Option<String>,
    notice: Option<&'static str>,
    /// Name, bio (description), location and website.
    about: keel_core::Profile,
    max_bio: usize,
}

/// Whose profile `who` may change from `base`: their own, or an
/// organization they own.
async fn subject(state: &AppState, user: &User, org: Option<&str>) -> AppResult<User> {
    match org {
        Some(name) => owned_org(state, user, name).await,
        None => Ok(user.clone()),
    }
}

fn base_url(org: Option<&str>) -> String {
    match org {
        Some(name) => format!("/org/{name}/settings/profile"),
        None => "/settings/profile".into(),
    }
}

async fn profile_page(state: &AppState, user: User, who: &User, org: Option<&str>, error: Option<String>, notice: Option<&'static str>) -> AppResult {
    let about = state.core.profile(who.id).await?;
    profile_page_with(state, user, who, org, error, notice, about).await
}

async fn profile_page_with(
    state: &AppState,
    user: User,
    who: &User,
    org: Option<&str>,
    error: Option<String>,
    notice: Option<&'static str>,
    about: keel_core::Profile,
) -> AppResult {
    let pinned: Vec<i64> = state.core.pinned_repos(who.id, Some(who)).await?.iter().map(|r| r.id).collect();
    let mut repos: Vec<Repo> = state.core.repos_of(who.id, Some(&user)).await?;
    repos.sort_by_key(|r| r.name.to_lowercase());
    html(ProfilePage {
        page: Page::new(state, Some(user), "Profile"),
        nav: if org.is_some() { "org" } else { "profile" },
        name: who.username.clone(),
        avatar_version: who.avatar_version,
        has_avatar: who.avatar_type.is_some(),
        base: base_url(org),
        org: org.map(str::to_owned),
        choices: repos
            .into_iter()
            .map(|r| PinChoice { pinned: pinned.contains(&r.id), id: r.id, name: r.name, is_private: r.is_private })
            .collect(),
        max_pins: MAX_PINS,
        error,
        notice,
        about,
        max_bio: keel_core::MAX_BIO,
    })
}

#[derive(Deserialize)]
pub struct AboutForm {
    #[serde(default)]
    full_name: String,
    #[serde(default)]
    bio: String,
    #[serde(default)]
    location: String,
    #[serde(default)]
    website: String,
}

/// `POST /settings/profile/about`: name, bio, location and website.
pub async fn save_about(State(state): State<AppState>, SignedIn(user): SignedIn, Form(f): Form<AboutForm>) -> AppResult {
    about(&state, user, None, f).await
}

/// `POST /org/{org}/settings/profile/about`: the same for an organization.
pub async fn save_org_about(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path(org): Path<String>,
    Form(f): Form<AboutForm>,
) -> AppResult {
    about(&state, user, Some(&org), f).await
}

async fn about(state: &AppState, user: User, org: Option<&str>, f: AboutForm) -> AppResult {
    let who = subject(state, &user, org).await?;
    let about = keel_core::Profile { full_name: f.full_name, bio: f.bio, location: f.location, website: f.website };
    match state.core.save_profile(&who, &about).await {
        Ok(_) => Ok(Redirect::to(&format!("{}?saved=about", base_url(org))).into_response()),
        Err(keel_core::Error::Invalid(msg)) => profile_page_with(state, user, &who, org, Some(msg), None, about).await,
        Err(e) => Err(e.into()),
    }
}

#[derive(Deserialize)]
pub struct SavedQuery {
    saved: Option<String>,
}

pub async fn edit(State(state): State<AppState>, SignedIn(user): SignedIn, axum::extract::Query(q): axum::extract::Query<SavedQuery>) -> AppResult {
    let who = user.clone();
    let notice = (q.saved.as_deref() == Some("about")).then_some("Saved.");
    profile_page(&state, user, &who, None, None, notice).await
}

pub async fn edit_org(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path(org): Path<String>,
    axum::extract::Query(q): axum::extract::Query<SavedQuery>,
) -> AppResult {
    let who = subject(&state, &user, Some(&org)).await?;
    let notice = (q.saved.as_deref() == Some("about")).then_some("Saved.");
    profile_page(&state, user, &who, Some(&org), None, notice).await
}

async fn upload(state: &AppState, user: User, org: Option<&str>, mut form: Multipart) -> AppResult {
    let who = subject(state, &user, org).await?;
    let mut data = None;
    while let Some(field) = form.next_field().await.map_err(|e| AppError::BadRequest(e.to_string()))? {
        if field.name() == Some("avatar") {
            data = Some(field.bytes().await.map_err(|e| AppError::BadRequest(e.to_string()))?);
        }
    }
    let result = match data.filter(|d| !d.is_empty()) {
        Some(d) => state.core.set_avatar(who.id, &d).await,
        None => Err(keel_core::Error::Invalid("Choose an image.".into())),
    };
    match result {
        Ok(()) => Ok(Redirect::to(&base_url(org)).into_response()),
        Err(keel_core::Error::Invalid(msg)) => profile_page(state, user, &who, org, Some(msg), None).await,
        Err(e) => Err(e.into()),
    }
}

pub async fn upload_avatar(State(state): State<AppState>, SignedIn(user): SignedIn, form: Multipart) -> AppResult {
    upload(&state, user, None, form).await
}

pub async fn upload_org_avatar(State(state): State<AppState>, SignedIn(user): SignedIn, Path(org): Path<String>, form: Multipart) -> AppResult {
    upload(&state, user, Some(&org), form).await
}

pub async fn remove_avatar(State(state): State<AppState>, SignedIn(user): SignedIn) -> AppResult {
    state.core.remove_avatar(user.id).await?;
    Ok(Redirect::to(&base_url(None)).into_response())
}

pub async fn remove_org_avatar(State(state): State<AppState>, SignedIn(user): SignedIn, Path(org): Path<String>) -> AppResult {
    let who = subject(&state, &user, Some(&org)).await?;
    state.core.remove_avatar(who.id).await?;
    Ok(Redirect::to(&base_url(Some(&org))).into_response())
}

/// The checked repositories (`repo` fields, repeated).
fn pin_ids(form: &[(String, String)]) -> Vec<i64> {
    form.iter().filter(|(k, _)| k == "repo").filter_map(|(_, v)| v.parse().ok()).collect()
}

async fn save_pins(state: &AppState, user: User, org: Option<&str>, form: Vec<(String, String)>) -> AppResult {
    let who = subject(state, &user, org).await?;
    match state.core.set_pins(&who, &pin_ids(&form)).await {
        Ok(()) => Ok(Redirect::to(&format!("/{}", who.username)).into_response()),
        Err(keel_core::Error::Invalid(msg)) => profile_page(state, user, &who, org, Some(msg), None).await,
        Err(e) => Err(e.into()),
    }
}

pub async fn pins(State(state): State<AppState>, SignedIn(user): SignedIn, Form(form): Form<Vec<(String, String)>>) -> AppResult {
    save_pins(&state, user, None, form).await
}

pub async fn org_pins(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path(org): Path<String>,
    Form(form): Form<Vec<(String, String)>>,
) -> AppResult {
    save_pins(&state, user, Some(&org), form).await
}

/// The pinned repositories of a profile, as cards.
pub(crate) async fn pinned_cards(state: &AppState, owner: &User, viewer: Option<&User>) -> AppResult<Vec<RepoCard>> {
    showcase(state, &state.core.pinned_repos(owner.id, viewer).await?).await
}

/// Cards for a few repositories, with each one's main language (from the
/// cache the front pages fill; worked out if missing).
pub(crate) async fn showcase(state: &AppState, repos: &[keel_core::Repo]) -> AppResult<Vec<RepoCard>> {
    cards_with_languages(state, repos, usize::MAX).await
}

/// Cards with each repository's main language, working out at most
/// `budget` that aren't cached yet: each takes a walk of the files, so
/// long lists fill in over a few visits instead of all at once.
pub(crate) async fn cards_with_languages(state: &AppState, repos: &[keel_core::Repo], mut budget: usize) -> AppResult<Vec<RepoCard>> {
    let mut out = Vec::with_capacity(repos.len());
    for r in repos {
        let mut card = RepoCard::from(r);
        let head = crate::repo::with_git(state, r, |g| Ok(g.default_branch()?.and_then(|b| g.resolve_rev(&b)))).await?;
        if let Some(head) = head {
            let langs = match state.languages.get(&head) {
                Some(hit) => Some(hit),
                None if budget > 0 => {
                    budget -= 1;
                    Some(crate::repo_side::languages(state, r, head).await?)
                }
                None => None,
            };
            card.language = langs.and_then(|l| l.first().map(|l| (l.name, l.color)));
        }
        out.push(card);
    }
    Ok(out)
}
