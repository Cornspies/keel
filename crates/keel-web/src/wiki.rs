//! Wiki pages: reading them, the page list, a page's history, and
//! creating, editing and deleting pages on the web.

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Redirect};
use keel_core::{Access, Repo, User, WikiContent, WikiEdit};
use keel_git::Oid;
use serde::Deserialize;

use crate::error::{AppResult, html};
use crate::render::{datetime, relative_time, wiki_markdown};
use crate::repo::{RAW_CSP, RepoView, encode_path, load, raw_content_type};
use crate::session::{SignedIn, Viewer};
use crate::{AppError, AppState, Page};

const HISTORY_PAGE: usize = 30;
/// Pages listed beside a page; all of them are on `_pages`.
const SIDE_PAGES: usize = 25;

fn base(repo: &Repo) -> String {
    format!("/{}/{}/wiki", repo.owner, repo.name)
}

fn page_url(repo: &Repo, name: &str) -> String {
    format!("{}/{}", base(repo), encode_path(name))
}

/// A page name from a URL: `Getting Started` and `Getting-Started.md`
/// both mean `Getting-Started`.
fn wanted(page: &str) -> String {
    let page = [".md", ".markdown"].iter().find_map(|e| page.strip_suffix(e)).unwrap_or(page);
    page.split_whitespace().collect::<Vec<_>>().join("-")
}

pub(crate) struct PageLink {
    pub title: String,
    pub url: String,
    pub current: bool,
}

/// What's around every wiki page: the page list and clone URLs.
pub(crate) struct WikiNav {
    pub base: String,
    pub pages: Vec<PageLink>,
    /// Pages beyond those listed.
    pub more: usize,
    pub http_url: String,
    pub ssh_url: Option<String>,
    pub can_write: bool,
}

async fn nav(state: &AppState, repo: &Repo, access: Access, current: &str) -> AppResult<WikiNav> {
    let all: Vec<_> = state.core.wiki_pages(repo.id).await?.into_iter().filter(|p| !p.is_special()).collect();
    let cfg = &state.core.config;
    let wiki = format!("{}.wiki", repo.name);
    Ok(WikiNav {
        more: all.len().saturating_sub(SIDE_PAGES),
        pages: all
            .iter()
            .take(SIDE_PAGES)
            .map(|p| PageLink { title: p.title(), url: page_url(repo, &p.name), current: p.name.eq_ignore_ascii_case(current) })
            .collect(),
        base: base(repo),
        http_url: cfg.http_clone_url(&repo.owner, &wiki),
        ssh_url: cfg.ssh_clone_url(&repo.owner, &wiki),
        can_write: access >= Access::Write,
    })
}

async fn repo_view(state: &AppState, repo: &Repo, access: Access, viewer: Option<&User>) -> AppResult<RepoView> {
    RepoView::new(state, repo, access, viewer, "wiki", "").await
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "wiki/page.html")]
struct PageView {
    page: Page,
    repo: RepoView,
    wiki: WikiNav,
    title: String,
    html: String,
    sidebar: String,
    footer: String,
    edited_by: String,
    edited: String,
    edited_full: String,
    /// The commit shown, when it isn't the current version.
    old_rev: Option<String>,
    url: String,
}

#[derive(Template)]
#[template(path = "wiki/empty.html")]
struct EmptyView {
    page: Page,
    repo: RepoView,
    wiki: WikiNav,
}

#[derive(Deserialize)]
pub struct RevQuery {
    rev: Option<String>,
}

pub async fn index(
    State(state): State<AppState>,
    Viewer(viewer): Viewer,
    Path((owner, name)): Path<(String, String)>,
) -> AppResult {
    let (repo, access) = load(&state, viewer.as_ref(), &owner, &name).await?;
    if let Some(home) = state.core.wiki_page(repo.id, "Home", None).await? {
        return show_page(&state, viewer, &repo, access, home, false).await;
    }
    if !state.core.wiki_pages(repo.id).await?.is_empty() {
        return Ok(Redirect::to(&format!("{}/_pages", base(&repo))).into_response());
    }
    html(EmptyView {
        wiki: nav(&state, &repo, access, "").await?,
        repo: repo_view(&state, &repo, access, viewer.as_ref()).await?,
        page: Page::new(&state, viewer, format!("Wiki · {}/{}", repo.owner, repo.name)),
    })
}

pub async fn show(
    State(state): State<AppState>,
    Viewer(viewer): Viewer,
    Path((owner, name, page)): Path<(String, String, String)>,
    Query(q): Query<RevQuery>,
) -> AppResult {
    let (repo, access) = load(&state, viewer.as_ref(), &owner, &name).await?;
    let rev = match q.rev.as_deref().filter(|r| !r.is_empty()) {
        Some(r) => Some(Oid::from_str(r).map_err(|_| AppError::NotFound)?),
        None => None,
    };
    let want = wanted(&page);
    let Some(content) = state.core.wiki_page(repo.id, &want, rev).await? else {
        if rev.is_none() && access >= Access::Write && keel_core::page_name(&want).is_ok() {
            let title: String = form_urlencoded::byte_serialize(keel_core::page_title(&want).as_bytes()).collect();
            return Ok(Redirect::to(&format!("{}/_new?title={title}", base(&repo))).into_response());
        }
        return Err(AppError::NotFound);
    };
    // One address per page.
    if content.page.name != page {
        let query = rev.map(|r| format!("?rev={r}")).unwrap_or_default();
        return Ok(Redirect::to(&format!("{}{query}", page_url(&repo, &content.page.name))).into_response());
    }
    show_page(&state, viewer, &repo, access, content, rev.is_some()).await
}

async fn show_page(state: &AppState, viewer: Option<User>, repo: &Repo, access: Access, content: WikiContent, old: bool) -> AppResult {
    let b = base(repo);
    let raw = format!("{b}/_raw");
    let rev = old.then_some(content.commit);
    let mut around = [String::new(), String::new()];
    if !content.page.is_special() {
        for (slot, special) in around.iter_mut().zip(keel_core::WIKI_SPECIAL) {
            if let Some(c) = state.core.wiki_page(repo.id, special, rev).await? {
                *slot = wiki_markdown(&c.text, &b, &raw);
            }
        }
    }
    let [sidebar, footer] = around;
    let title = content.page.title();
    let (edited_by, edited, edited_full) = match &content.last {
        Some(c) => (c.author.name.clone(), relative_time(c.author.time), datetime(c.author.time)),
        None => Default::default(),
    };
    html(PageView {
        wiki: nav(state, repo, access, &content.page.name).await?,
        repo: repo_view(state, repo, access, viewer.as_ref()).await?,
        page: Page::new(state, viewer, format!("{title} · {}/{} wiki", repo.owner, repo.name)),
        html: wiki_markdown(&content.text, &b, &raw),
        url: page_url(repo, &content.page.name),
        old_rev: rev.map(|r| r.to_string()[..7].to_owned()),
        title,
        sidebar,
        footer,
        edited_by,
        edited,
        edited_full,
    })
}

#[derive(Template)]
#[template(path = "wiki/pages.html")]
struct PagesView {
    page: Page,
    repo: RepoView,
    wiki: WikiNav,
    all: Vec<PageLink>,
}

pub async fn pages(
    State(state): State<AppState>,
    Viewer(viewer): Viewer,
    Path((owner, name)): Path<(String, String)>,
) -> AppResult {
    let (repo, access) = load(&state, viewer.as_ref(), &owner, &name).await?;
    let all = state
        .core
        .wiki_pages(repo.id)
        .await?
        .into_iter()
        .filter(|p| !p.is_special())
        .map(|p| PageLink { title: p.title(), url: page_url(&repo, &p.name), current: false })
        .collect();
    html(PagesView {
        wiki: nav(&state, &repo, access, "").await?,
        repo: repo_view(&state, &repo, access, viewer.as_ref()).await?,
        page: Page::new(&state, viewer, format!("Pages · {}/{} wiki", repo.owner, repo.name)),
        all,
    })
}

pub(crate) struct RevisionView {
    pub summary: String,
    pub author: String,
    pub when: String,
    pub when_full: String,
    pub short: String,
    pub url: String,
}

#[derive(Template)]
#[template(path = "wiki/history.html")]
struct HistoryView {
    page: Page,
    repo: RepoView,
    wiki: WikiNav,
    title: String,
    url: String,
    revisions: Vec<RevisionView>,
    newer_url: Option<String>,
    older_url: Option<String>,
}

#[derive(Deserialize)]
pub struct HistoryQuery {
    page: Option<usize>,
}

pub async fn history(
    State(state): State<AppState>,
    Viewer(viewer): Viewer,
    Path((owner, name, page)): Path<(String, String, String)>,
    Query(q): Query<HistoryQuery>,
) -> AppResult {
    let (repo, access) = load(&state, viewer.as_ref(), &owner, &name).await?;
    let page_no = q.page.unwrap_or(1).max(1);
    let (found, mut log) = state
        .core
        .wiki_history(repo.id, &wanted(&page), (page_no - 1) * HISTORY_PAGE, HISTORY_PAGE + 1)
        .await?
        .ok_or(AppError::NotFound)?;
    let more = log.len() > HISTORY_PAGE;
    log.truncate(HISTORY_PAGE);
    let url = page_url(&repo, &found.name);
    let revisions = log
        .iter()
        .map(|c| RevisionView {
            summary: c.summary.clone(),
            author: c.author.name.clone(),
            when: relative_time(c.author.time),
            when_full: datetime(c.author.time),
            short: c.short_id(),
            url: format!("{url}?rev={}", c.id),
        })
        .collect();
    html(HistoryView {
        wiki: nav(&state, &repo, access, &found.name).await?,
        repo: repo_view(&state, &repo, access, viewer.as_ref()).await?,
        page: Page::new(&state, viewer, format!("History of {} · {}/{} wiki", found.title(), repo.owner, repo.name)),
        title: found.title(),
        newer_url: (page_no > 1).then(|| format!("{url}/history?page={}", page_no - 1)),
        older_url: more.then(|| format!("{url}/history?page={}", page_no + 1)),
        url,
        revisions,
    })
}

/// Files of the wiki, e.g. images pages show.
pub async fn raw(
    State(state): State<AppState>,
    Viewer(viewer): Viewer,
    Path((owner, name, path)): Path<(String, String, String)>,
) -> AppResult {
    let (repo, _) = load(&state, viewer.as_ref(), &owner, &name).await?;
    let blob = state.core.wiki_file(repo.id, &path).await?.ok_or(AppError::NotFound)?;
    let mut res = ([(header::CONTENT_TYPE, raw_content_type(&path, blob.binary))], blob.data).into_response();
    let h = res.headers_mut();
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(if repo.is_private { "private, no-cache" } else { "public, no-cache" }));
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(RAW_CSP));
    Ok(res)
}

// ---------------------------------------------------------------------------
// Editing
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "wiki/edit.html")]
struct EditView {
    page: Page,
    repo: RepoView,
    wiki: WikiNav,
    /// The page being edited; `None` for a new one.
    editing: Option<String>,
    action: String,
    title: String,
    text: String,
    message: String,
    base: String,
    error: Option<String>,
}

async fn writer_repo(state: &AppState, user: &User, owner: &str, name: &str) -> AppResult<(Repo, Access)> {
    let (repo, access) = load(state, Some(user), owner, name).await?;
    if access < Access::Write {
        return Err(AppError::Forbidden);
    }
    Ok((repo, access))
}

struct Draft {
    title: String,
    text: String,
    message: String,
    base: String,
}

async fn edit_page(state: &AppState, user: User, repo: &Repo, access: Access, editing: Option<String>, d: Draft, error: Option<String>) -> AppResult {
    let action = match &editing {
        Some(name) => format!("{}/edit", page_url(repo, name)),
        None => base(repo),
    };
    let heading = match &editing {
        Some(name) => format!("Edit {}", keel_core::page_title(name)),
        None => "New page".into(),
    };
    html(EditView {
        wiki: nav(state, repo, access, editing.as_deref().unwrap_or_default()).await?,
        repo: repo_view(state, repo, access, Some(&user)).await?,
        page: Page::new(state, Some(user), format!("{heading} · {}/{} wiki", repo.owner, repo.name)),
        editing,
        action,
        title: d.title,
        text: d.text,
        message: d.message,
        base: d.base,
        error,
    })
}

#[derive(Deserialize)]
pub struct NewQuery {
    title: Option<String>,
}

pub async fn new_form(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<NewQuery>,
) -> AppResult {
    let (repo, access) = writer_repo(&state, &user, &owner, &name).await?;
    // The first page is the front page.
    let first = state.core.wiki_pages(repo.id).await?.is_empty();
    let title = q.title.unwrap_or_else(|| if first { "Home".into() } else { String::new() });
    let d = Draft { title, text: String::new(), message: String::new(), base: String::new() };
    edit_page(&state, user, &repo, access, None, d, None).await
}

#[derive(Deserialize)]
pub struct PageForm {
    title: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    message: String,
    #[serde(default)]
    base: String,
}

pub async fn create(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name)): Path<(String, String)>,
    Form(f): Form<PageForm>,
) -> AppResult {
    let (repo, access) = writer_repo(&state, &user, &owner, &name).await?;
    save(&state, user, &repo, access, None, f).await
}

pub async fn edit_form(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, page)): Path<(String, String, String)>,
) -> AppResult {
    let (repo, access) = writer_repo(&state, &user, &owner, &name).await?;
    let content = state.core.wiki_page(repo.id, &wanted(&page), None).await?.ok_or(AppError::NotFound)?;
    let d = Draft { title: content.page.title(), text: content.text, message: String::new(), base: content.commit.to_string() };
    edit_page(&state, user, &repo, access, Some(content.page.name), d, None).await
}

pub async fn update(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, page)): Path<(String, String, String)>,
    Form(f): Form<PageForm>,
) -> AppResult {
    let (repo, access) = writer_repo(&state, &user, &owner, &name).await?;
    save(&state, user, &repo, access, Some(wanted(&page)), f).await
}

async fn save(state: &AppState, user: User, repo: &Repo, access: Access, editing: Option<String>, f: PageForm) -> AppResult {
    let edit = WikiEdit {
        page: editing.as_deref(),
        title: &f.title,
        text: &f.text,
        message: &f.message,
        base: Oid::from_str(&f.base).ok(),
    };
    match state.core.save_wiki_page(repo, &user, edit).await {
        Ok(saved) => Ok(Redirect::to(&page_url(repo, &saved)).into_response()),
        Err(keel_core::Error::Invalid(msg)) => {
            let d = Draft { title: f.title, text: f.text, message: f.message, base: f.base };
            edit_page(state, user, repo, access, editing, d, Some(msg)).await
        }
        Err(e) => Err(e.into()),
    }
}

pub async fn delete(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, page)): Path<(String, String, String)>,
) -> AppResult {
    let (repo, _) = writer_repo(&state, &user, &owner, &name).await?;
    state.core.delete_wiki_page(&repo, &user, &wanted(&page)).await?;
    Ok(Redirect::to(&base(&repo)).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn page_addresses() {
        assert_eq!(wanted("Getting Started"), "Getting-Started");
        assert_eq!(wanted("Getting-Started.md"), "Getting-Started");
        assert_eq!(wanted("Notes.markdown"), "Notes");
        assert_eq!(wanted("Home"), "Home");
    }
}
