//! Starring repositories, and who starred one.

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::response::{IntoResponse, Redirect};
use serde::Deserialize;

use crate::error::{AppResult, html};
use crate::render::{datetime, relative_time};
use crate::repo::{RepoView, load};
use crate::session::{SignedIn, Viewer};
use crate::{AppState, Page};

const PER_PAGE: i64 = 50;

#[derive(Deserialize)]
pub struct StarForm {
    /// `on` or `off`.
    to: String,
}

pub async fn star(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name)): Path<(String, String)>,
    Form(f): Form<StarForm>,
) -> AppResult {
    let (repo, _) = load(&state, Some(&user), &owner, &name).await?;
    state.core.set_star(&user, &repo, f.to == "on").await?;
    Ok(Redirect::to(&format!("/{}/{}", repo.owner, repo.name)).into_response())
}

pub(crate) struct GazerView {
    pub name: String,
    pub when: String,
    pub when_full: String,
}

#[derive(Template)]
#[template(path = "repo/stargazers.html")]
struct StargazersPage {
    page: Page,
    repo: RepoView,
    gazers: Vec<GazerView>,
    newer_url: Option<String>,
    older_url: Option<String>,
}

#[derive(Deserialize)]
pub struct PageQuery {
    page: Option<i64>,
}

pub async fn stargazers(
    State(state): State<AppState>,
    Viewer(viewer): Viewer,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<PageQuery>,
) -> AppResult {
    let (repo, access) = load(&state, viewer.as_ref(), &owner, &name).await?;
    let page_no = q.page.unwrap_or(1).max(1);
    let mut gazers = state.core.stargazers(repo.id, (page_no - 1) * PER_PAGE, PER_PAGE + 1).await?;
    let more = gazers.len() as i64 > PER_PAGE;
    gazers.truncate(PER_PAGE as usize);
    let url = format!("/{}/{}/stargazers", repo.owner, repo.name);
    html(StargazersPage {
        repo: RepoView::new(&state, &repo, access, viewer.as_ref(), "code", "").await?,
        page: Page::new(&state, viewer, format!("Stargazers · {}/{}", repo.owner, repo.name)),
        gazers: gazers
            .into_iter()
            .map(|g| GazerView { name: g.user.username, when: relative_time(g.starred_at), when_full: datetime(g.starred_at) })
            .collect(),
        newer_url: (page_no > 1).then(|| format!("{url}?page={}", page_no - 1)),
        older_url: more.then(|| format!("{url}?page={}", page_no + 1)),
    })
}
