//! Organizations: their profiles, creating them and administering them.

use std::collections::HashMap;

use askama::Template;
use axum::Form;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Redirect};
use keel_core::{OrgRole, Repo, User};
use serde::Deserialize;

use crate::error::{AppResult, html};
use crate::pages::{ProfileQuery, RepoCard, RepoFilter};
use crate::session::SignedIn;
use crate::{AppError, AppState, Page};

#[derive(Template)]
#[template(path = "org_new.html")]
struct NewOrgPage {
    page: Page,
    error: Option<String>,
    name: String,
}

pub async fn new_form(State(state): State<AppState>, SignedIn(user): SignedIn) -> AppResult {
    html(NewOrgPage {
        page: Page::new(&state, Some(user), "New organization"),
        error: None,
        name: String::new(),
    })
}

#[derive(Deserialize)]
pub struct NewOrgForm {
    name: String,
}

pub async fn create(State(state): State<AppState>, SignedIn(user): SignedIn, Form(f): Form<NewOrgForm>) -> AppResult {
    match state.core.create_org(&user, &f.name).await {
        Ok(org) => Ok(Redirect::to(&format!("/{}", org.username)).into_response()),
        Err(keel_core::Error::Invalid(msg)) => html(NewOrgPage {
            page: Page::new(&state, Some(user), "New organization"),
            error: Some(msg),
            name: f.name,
        }),
        Err(e) => Err(e.into()),
    }
}

/// The organization, if `user` owns it (or is a site admin).
pub(crate) async fn owned_org(state: &AppState, user: &User, name: &str) -> AppResult<User> {
    let org = state
        .core
        .user_by_name(name)
        .await?
        .filter(User::is_org)
        .ok_or(AppError::NotFound)?;
    let role = state.core.org_role(org.id, user.id).await?;
    if role != Some(OrgRole::Owner) && !user.is_admin {
        return Err(AppError::Forbidden);
    }
    Ok(org)
}

struct MemberView {
    id: i64,
    username: String,
    role: &'static str,
}

#[derive(Template)]
#[template(path = "org_settings.html")]
struct OrgSettingsPage {
    page: Page,
    name: String,
    members: Vec<MemberView>,
    error: Option<String>,
}

async fn settings_page(state: &AppState, user: User, org: &User, error: Option<String>) -> AppResult {
    let members = state
        .core
        .org_members(org.id)
        .await?
        .into_iter()
        .map(|(u, r)| MemberView {
            id: u.id,
            username: u.username,
            role: r.as_str(),
        })
        .collect();
    html(OrgSettingsPage {
        page: Page::new(state, Some(user), format!("Settings · {}", org.username)),
        name: org.username.clone(),
        members,
        error,
    })
}

pub async fn settings(State(state): State<AppState>, SignedIn(user): SignedIn, Path(name): Path<String>) -> AppResult {
    let org = owned_org(&state, &user, &name).await?;
    settings_page(&state, user, &org, None).await
}

#[derive(Deserialize)]
pub struct MemberForm {
    username: String,
    role: String,
}

pub async fn set_member(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path(name): Path<String>,
    Form(f): Form<MemberForm>,
) -> AppResult {
    let org = owned_org(&state, &user, &name).await?;
    let role = OrgRole::parse(&f.role).ok_or_else(|| AppError::BadRequest("Unknown role".into()))?;
    match state.core.set_org_member(&org, &f.username, role).await {
        Ok(()) => Ok(Redirect::to(&format!("/org/{}/settings", org.username)).into_response()),
        Err(keel_core::Error::Invalid(msg)) => settings_page(&state, user, &org, Some(msg)).await,
        Err(e) => Err(e.into()),
    }
}

pub async fn remove_member(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((name, member_id)): Path<(String, i64)>,
) -> AppResult {
    let org = owned_org(&state, &user, &name).await?;
    match state.core.remove_org_member(&org, member_id).await {
        Ok(()) if member_id == user.id => Ok(Redirect::to(&format!("/{}", org.username)).into_response()),
        Ok(()) => Ok(Redirect::to(&format!("/org/{}/settings", org.username)).into_response()),
        Err(keel_core::Error::Invalid(msg)) => settings_page(&state, user, &org, Some(msg)).await,
        Err(e) => Err(e.into()),
    }
}

pub async fn delete(State(state): State<AppState>, SignedIn(user): SignedIn, Path(name): Path<String>) -> AppResult {
    let org = owned_org(&state, &user, &name).await?;
    match state.core.delete_org(&org).await {
        Ok(()) => Ok(Redirect::to("/").into_response()),
        Err(keel_core::Error::Invalid(msg)) => settings_page(&state, user, &org, Some(msg)).await,
        Err(e) => Err(e.into()),
    }
}

// ---------------------------------------------------------------------------
// The organization's profile
// ---------------------------------------------------------------------------

/// The top of an organization's pages (its profile and its projects): who
/// it is, and the tabs.
pub(crate) struct OrgHead {
    pub name: String,
    pub avatar_version: i64,
    pub profile: keel_core::Profile,
    /// The website without its scheme, to show.
    pub website_label: String,
    /// The viewer is a member (may create repositories and see projects).
    pub is_member: bool,
    /// The viewer may change its settings.
    pub is_owner: bool,
    pub repo_count: usize,
    pub people_count: usize,
    /// Open projects, for members.
    pub project_count: Option<i64>,
    /// `overview`, `repositories`, `projects` or `people`.
    pub tab: &'static str,
}

/// [`OrgHead`] for `org` as `viewer` sees it, on `tab`.
pub(crate) async fn org_head(state: &AppState, org: &User, viewer: Option<&User>, tab: &'static str) -> AppResult<OrgHead> {
    let members = state.core.org_members(org.id).await?;
    let role = viewer.and_then(|v| members.iter().find(|(m, _)| m.id == v.id).map(|(_, r)| *r));
    let is_member = role.is_some() || viewer.is_some_and(|v| v.is_admin);
    let project_count = match is_member {
        true => Some(state.core.project_counts(keel_core::ProjectOwner::Org(org.id)).await?.0),
        false => None,
    };
    let profile = state.core.profile(org.id).await?;
    Ok(OrgHead {
        name: org.username.clone(),
        avatar_version: org.avatar_version,
        website_label: website_label(&profile.website),
        profile,
        is_member,
        is_owner: role == Some(OrgRole::Owner) || viewer.is_some_and(|v| v.is_admin),
        repo_count: state.core.repos_of(org.id, viewer).await?.len(),
        people_count: members.len(),
        project_count,
        tab,
    })
}

/// `https://example.com/` → `example.com`.
pub(crate) fn website_label(url: &str) -> String {
    url.split_once("://").map_or(url, |(_, rest)| rest).trim_end_matches('/').to_owned()
}

pub(crate) struct PersonRow {
    pub username: String,
    pub full_name: String,
    pub is_owner: bool,
}

/// How often a language or topic comes up among the repositories.
pub(crate) struct Tally {
    pub name: String,
    /// Languages: their colour.
    pub color: &'static str,
    pub count: usize,
}

#[derive(Template)]
#[template(path = "org.html")]
struct OrgPage {
    page: Page,
    head: OrgHead,
    /// Overview: pinned repositories, or the most starred ones.
    showcase: Vec<RepoCard>,
    showcase_pinned: bool,
    /// Overview: the recently updated ones.
    recent: Vec<RepoCard>,
    languages: Vec<Tally>,
    topics: Vec<Tally>,
    /// Overview: some of the people; the People tab: all of them.
    people: Vec<PersonRow>,
    /// Repositories tab.
    repos: Vec<RepoCard>,
    filter: RepoFilter,
}

/// Shown on the overview.
const POPULAR: usize = 6;
const RECENT: usize = 5;
const PEOPLE_SHOWN: usize = 24;
/// How many repositories' main languages "Top languages" counts, and how
/// many main languages are worked out at most for one page of a list.
pub(crate) const LANGUAGE_SAMPLE: usize = 12;
const TALLIES: usize = 8;

/// `GET /{org}`: the organization's profile.
pub(crate) async fn profile(state: &AppState, viewer: Option<User>, org: User, q: &ProfileQuery) -> AppResult {
    let tab = match q.tab.as_deref() {
        Some("repositories") => "repositories",
        Some("people") => "people",
        _ => "overview",
    };
    let head = org_head(state, &org, viewer.as_ref(), tab).await?;
    let mut page = OrgPage {
        page: Page::new(state, viewer.clone(), org.username.clone()),
        head,
        showcase: Vec::new(),
        showcase_pinned: false,
        recent: Vec::new(),
        languages: Vec::new(),
        topics: Vec::new(),
        people: Vec::new(),
        repos: Vec::new(),
        filter: RepoFilter::from_query(q),
    };
    let viewer = viewer.as_ref();
    match tab {
        "repositories" => {
            let mut repos = state.core.repos_of(org.id, viewer).await?;
            page.filter.apply(&mut repos);
            page.repos = crate::profile::cards_with_languages(state, &repos, LANGUAGE_SAMPLE).await?;
        }
        _ => {
            let mut members = state.core.org_members(org.id).await?;
            members.sort_by(|(a, ar), (b, br)| (*br == OrgRole::Owner).cmp(&(*ar == OrgRole::Owner)).then(a.username.cmp(&b.username)));
            if tab == "overview" {
                members.truncate(PEOPLE_SHOWN);
            }
            for (u, role) in members {
                let full_name = if tab == "people" { state.core.profile(u.id).await?.full_name } else { String::new() };
                page.people.push(PersonRow { username: u.username, full_name, is_owner: role == OrgRole::Owner });
            }
            if tab == "overview" {
                overview(state, &org, viewer, &mut page).await?;
            }
        }
    }
    html(page)
}

async fn overview(state: &AppState, org: &User, viewer: Option<&User>, page: &mut OrgPage) -> AppResult<()> {
    let mut repos = state.core.repos_of(org.id, viewer).await?;
    page.showcase = crate::profile::pinned_cards(state, org, viewer).await?;
    page.showcase_pinned = !page.showcase.is_empty();
    if page.showcase.is_empty() {
        let mut popular: Vec<Repo> = repos.iter().filter(|r| !r.is_archived() && r.forked_from.is_none()).cloned().collect();
        popular.sort_by(|a, b| b.stars.cmp(&a.stars).then(b.updated_at.cmp(&a.updated_at)));
        popular.truncate(POPULAR);
        page.showcase = crate::profile::showcase(state, &popular).await?;
    }

    let mut topics: HashMap<&str, usize> = HashMap::new();
    for t in repos.iter().flat_map(Repo::topics) {
        *topics.entry(t).or_default() += 1;
    }
    page.topics = top(topics.into_iter().map(|(name, count)| (name.to_owned(), "", count)).collect());

    // Recently changed repositories, with their main languages; the
    // languages of those (and a few more) make "Top languages".
    repos.sort_by_key(|r| std::cmp::Reverse(r.updated_at));
    repos.retain(|r| r.forked_from.is_none());
    repos.truncate(LANGUAGE_SAMPLE);
    let sample = crate::profile::showcase(state, &repos).await?;
    let mut languages: HashMap<&'static str, (&'static str, usize)> = HashMap::new();
    for (name, color) in sample.iter().filter_map(|c| c.language) {
        languages.entry(name).or_insert((color, 0)).1 += 1;
    }
    page.languages = top(languages.into_iter().map(|(name, (color, count))| (name.to_owned(), color, count)).collect());
    page.recent = sample.into_iter().take(RECENT).collect();
    Ok(())
}

/// The most frequent first, then by name.
fn top(mut all: Vec<(String, &'static str, usize)>) -> Vec<Tally> {
    all.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    all.into_iter().take(TALLIES).map(|(name, color, count)| Tally { name, color, count }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tallies() {
        let t = top(vec![("go".into(), "", 1), ("rust".into(), "", 3), ("c".into(), "", 1)]);
        let got: Vec<(&str, usize)> = t.iter().map(|t| (t.name.as_str(), t.count)).collect();
        assert_eq!(got, [("rust", 3), ("c", 1), ("go", 1)], "most first, then by name");
        assert_eq!(top((0..20).map(|i| (format!("t{i:02}"), "", 1)).collect()).len(), TALLIES);
        assert_eq!(website_label("https://acme.example.org/"), "acme.example.org");
        assert_eq!(website_label("http://x.dev/a/"), "x.dev/a");
    }
}
