//! Repository pages.

use std::collections::HashMap;
use std::sync::Arc;

use askama::Template;
use axum::Form;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Redirect, Response};
use keel_core::{Access, Repo, User};
use keel_git::{CommitSummary, DiffLineKind, DiffStatus, EntryKind, GitRepo, Oid, Resolved};
use serde::Deserialize;

use crate::error::{AppResult, html};
use crate::render::{self, LinkBase, datetime, human_size, relative_time};
use crate::session::{SignedIn, Viewer};
use crate::{AppError, AppState, Page};

/// Commits to walk while rendering a tree page. If that doesn't resolve
/// every entry's last commit, the page is sent without them and the browser
/// fetches them from `last-commits` (with a much larger budget) instead, so
/// a large history never delays the first paint.
const LAST_COMMIT_SYNC_WALK: usize = 500;
const LAST_COMMIT_ASYNC_WALK: usize = 500_000;
const COMMITS_PER_PAGE: usize = 50;
/// Larger files are offered as a raw download instead of being rendered.
const MAX_DISPLAY_BYTES: usize = 2 * 1024 * 1024;
const DIFF_MAX_FILES: usize = 300;
const DIFF_MAX_LINES: usize = 10_000;
const README_NAMES: &[&str] = &["readme.md", "readme.markdown", "readme", "readme.txt"];
const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "svg", "ico", "bmp", "avif"];

// ---------------------------------------------------------------------------
// Shared helpers and view models
// ---------------------------------------------------------------------------

/// Percent-encodes a repository path or ref for use in a URL, keeping `/`.
pub(crate) fn encode_path(p: &str) -> String {
    let mut out = String::with_capacity(p.len());
    for b in p.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn extension(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default()
}

/// Looks up a repository the viewer may read. Private repositories the
/// viewer can't see are reported as missing, not forbidden.
async fn load(state: &AppState, viewer: Option<&User>, owner: &str, name: &str) -> AppResult<(Repo, Access)> {
    let name = name.strip_suffix(".git").unwrap_or(name);
    let repo = state.core.repo(owner, name).await?.ok_or(AppError::NotFound)?;
    match repo.access(viewer) {
        Access::None => Err(AppError::NotFound),
        access => Ok((repo, access)),
    }
}

/// Runs `f` against the repository on the blocking thread pool.
async fn with_git<T: Send + 'static>(
    state: &AppState,
    repo: &Repo,
    f: impl FnOnce(&GitRepo) -> AppResult<T> + Send + 'static,
) -> AppResult<T> {
    let path = state.core.repo_path(repo);
    tokio::task::spawn_blocking(move || f(&GitRepo::open(&path)?)).await?
}

pub(crate) struct RepoView {
    pub owner: String,
    pub name: String,
    pub description: String,
    pub is_private: bool,
    pub can_write: bool,
    pub http_url: String,
    pub ssh_url: Option<String>,
    /// Which tab is active: code, commits, branches or settings.
    pub tab: &'static str,
    /// Branch used for tab links; empty for an empty repository.
    pub default_rev: String,
}

impl RepoView {
    fn new(state: &AppState, repo: &Repo, access: Access, tab: &'static str, default_rev: &str) -> Self {
        let cfg = &state.core.config;
        Self {
            owner: repo.owner.clone(),
            name: repo.name.clone(),
            description: repo.description.clone(),
            is_private: repo.is_private,
            can_write: access == Access::Write,
            http_url: cfg.http_clone_url(&repo.owner, &repo.name),
            ssh_url: cfg.ssh_clone_url(&repo.owner, &repo.name),
            tab,
            default_rev: encode_path(default_rev),
        }
    }
}

pub(crate) struct CommitView {
    pub id: String,
    pub short: String,
    pub summary: String,
    pub author: String,
    pub when: String,
    pub when_full: String,
    pub url: String,
}

impl CommitView {
    fn new(base: &str, c: &CommitSummary) -> Self {
        Self {
            id: c.id.to_string(),
            short: c.short_id(),
            summary: c.summary.clone(),
            author: c.author.name.clone(),
            when: relative_time(c.author.time),
            when_full: datetime(c.author.time),
            url: format!("{base}/commit/{}", c.id),
        }
    }
}

pub(crate) struct Crumb {
    pub name: String,
    pub url: String,
}

/// Breadcrumbs for `path`, each linking to its directory.
fn crumbs(base: &str, rev: &str, path: &str) -> Vec<Crumb> {
    let mut out = Vec::new();
    let mut acc = String::new();
    for seg in path.split('/').filter(|s| !s.is_empty()) {
        if !acc.is_empty() {
            acc.push('/');
        }
        acc.push_str(seg);
        out.push(Crumb {
            name: seg.to_owned(),
            url: format!("{base}/tree/{}/{}", encode_path(rev), encode_path(&acc)),
        });
    }
    out
}

pub(crate) struct RefLink {
    pub name: String,
    pub url: String,
}

/// Branches and tags for the revision switcher, linking to `{base}/{mode}/<ref>`.
pub(crate) struct RefNames {
    pub branches: Vec<RefLink>,
    pub tags: Vec<RefLink>,
}

impl RefNames {
    fn load(g: &GitRepo, base: &str, mode: &str) -> AppResult<Self> {
        let link = |r: keel_git::RefInfo| RefLink {
            url: format!("{base}/{mode}/{}", encode_path(&r.name)),
            name: r.name,
        };
        Ok(Self {
            branches: g.branches()?.into_iter().map(link).collect(),
            tags: g.tags()?.into_iter().map(link).collect(),
        })
    }
}

// ---------------------------------------------------------------------------
// Tree (and repository home)
// ---------------------------------------------------------------------------

pub(crate) struct EntryView {
    pub name: String,
    pub url: String,
    pub kind: &'static str,
    pub last: Option<CommitView>,
}

#[derive(Template)]
#[template(path = "repo/tree.html")]
struct TreePage {
    page: Page,
    repo: RepoView,
    rev: String,
    rev_url: String,
    path: String,
    crumbs: Vec<Crumb>,
    entries: Vec<EntryView>,
    latest: Option<CommitView>,
    history_url: String,
    /// Set when last commits are loaded asynchronously.
    last_commits_url: Option<String>,
    readme_name: String,
    readme: Option<Arc<String>>,
    refs: RefNames,
}

#[derive(Template)]
#[template(path = "repo/empty.html")]
struct EmptyPage {
    page: Page,
    repo: RepoView,
}

// Built once per request and immediately consumed; boxing buys nothing.
#[allow(clippy::large_enum_variant)]
enum TreeData {
    Empty,
    File(Resolved),
    Dir {
        resolved: Resolved,
        default_branch: String,
        entries: Vec<keel_git::TreeEntry>,
        last: Option<Arc<HashMap<String, CommitSummary>>>,
        latest: Option<CommitSummary>,
        readme: Option<(String, Arc<String>)>,
        refs: RefNames,
    },
}

pub async fn home(
    State(state): State<AppState>,
    Viewer(viewer): Viewer,
    Path((owner, name)): Path<(String, String)>,
) -> AppResult {
    let (repo, access) = load(&state, viewer.as_ref(), &owner, &name).await?;
    if repo.owner != owner || repo.name != name {
        return Ok(Redirect::permanent(&format!("/{}/{}", repo.owner, repo.name)).into_response());
    }
    render_tree(state, viewer, repo, access, None).await
}

pub async fn tree(
    State(state): State<AppState>,
    Viewer(viewer): Viewer,
    Path((owner, name, rest)): Path<(String, String, String)>,
) -> AppResult {
    let (repo, access) = load(&state, viewer.as_ref(), &owner, &name).await?;
    render_tree(state, viewer, repo, access, Some(rest)).await
}

async fn render_tree(
    state: AppState,
    viewer: Option<User>,
    repo: Repo,
    access: Access,
    rest: Option<String>,
) -> AppResult {
    let base = format!("/{}/{}", repo.owner, repo.name);
    let st = state.clone();
    let link_base = base.clone();
    let data = with_git(&state, &repo, move |g| {
        let default_branch = g.default_branch()?;
        let resolved = match (rest, &default_branch) {
            (Some(rest), _) => g.resolve_rev_path(&rest).ok_or(AppError::NotFound)?,
            (None, None) => return Ok(TreeData::Empty),
            (None, Some(branch)) => Resolved {
                rev: branch.clone(),
                commit: g.resolve_rev(branch).ok_or(AppError::NotFound)?,
                path: String::new(),
            },
        };
        let Some(entries) = g.tree(resolved.commit, &resolved.path)? else {
            return Ok(TreeData::File(resolved));
        };

        let last = last_commits(&st, g, resolved.commit, &resolved.path, LAST_COMMIT_SYNC_WALK)?;
        let latest = g.log(resolved.commit, &resolved.path, 0, 1)?.into_iter().next();

        let readme_entry = entries.iter().find(|e| {
            matches!(e.kind, EntryKind::File | EntryKind::Executable)
                && README_NAMES.contains(&e.name.to_ascii_lowercase().as_str())
        });
        let readme = match readme_entry {
            Some(e) => {
                let file = join(&resolved.path, &e.name);
                let rev = encode_path(&resolved.rev);
                let links = LinkBase {
                    blob: &format!("{link_base}/blob/{rev}"),
                    raw: &format!("{link_base}/raw/{rev}"),
                    dir: &resolved.path,
                };
                g.blob(resolved.commit, &file)?
                    .filter(|b| !b.binary && b.data.len() <= MAX_DISPLAY_BYTES)
                    .map(|b| (e.name.clone(), render_text_doc(&st, &e.name, &b, &links)))
            }
            None => None,
        };

        Ok(TreeData::Dir {
            refs: RefNames::load(g, &link_base, "tree")?,
            default_branch: default_branch.unwrap_or_default(),
            resolved,
            entries,
            last,
            latest,
            readme,
        })
    })
    .await?;

    match data {
        TreeData::Empty => html(EmptyPage {
            repo: RepoView::new(&state, &repo, access, "code", ""),
            page: Page::new(&state, viewer, format!("{}/{}", repo.owner, repo.name)),
        }),
        TreeData::File(r) => Ok(Redirect::to(&format!(
            "{base}/blob/{}/{}",
            encode_path(&r.rev),
            encode_path(&r.path)
        ))
        .into_response()),
        TreeData::Dir { resolved, default_branch, entries, last, latest, readme, refs } => {
            let rev_url = encode_path(&resolved.rev);
            let entries = entries
                .into_iter()
                .map(|e| {
                    let full = join(&resolved.path, &e.name);
                    let (kind, mode) = match e.kind {
                        EntryKind::Dir => ("dir", "tree"),
                        EntryKind::Submodule => ("submodule", "tree"),
                        EntryKind::Symlink => ("symlink", "blob"),
                        EntryKind::Executable => ("exec", "blob"),
                        EntryKind::File => ("file", "blob"),
                    };
                    EntryView {
                        url: format!("{base}/{mode}/{rev_url}/{}", encode_path(&full)),
                        kind,
                        last: last.as_ref().and_then(|l| l.get(&e.name)).map(|c| CommitView::new(&base, c)),
                        name: e.name,
                    }
                })
                .collect();
            let title = if resolved.path.is_empty() {
                format!("{}/{}", repo.owner, repo.name)
            } else {
                format!("{} at {} · {}/{}", resolved.path, resolved.rev, repo.owner, repo.name)
            };
            let (readme_name, readme) = match readme {
                Some((n, r)) => (n, Some(r)),
                None => (String::new(), None),
            };
            html(TreePage {
                page: Page::new(&state, viewer, title),
                repo: RepoView::new(&state, &repo, access, "code", &default_branch),
                crumbs: crumbs(&base, &resolved.rev, &resolved.path),
                history_url: format!("{base}/commits/{rev_url}/{}", encode_path(&resolved.path)),
                // A full commit id keeps the URL stable (and cacheable) even
                // if the branch moves while the page is open.
                last_commits_url: last.is_none().then(|| {
                    format!("{base}/last-commits/{}/{}", resolved.commit, encode_path(&resolved.path))
                }),
                latest: latest.as_ref().map(|c| CommitView::new(&base, c)),
                rev: resolved.rev,
                rev_url,
                path: resolved.path,
                entries,
                readme_name,
                readme,
                refs,
            })
        }
    }
}

/// Last commit per directory entry, from the cache or computed with a walk
/// budget. `None` if the budget wasn't enough (partial results aren't
/// cached).
fn last_commits(
    state: &AppState,
    g: &GitRepo,
    commit: Oid,
    path: &str,
    budget: usize,
) -> AppResult<Option<Arc<HashMap<String, CommitSummary>>>> {
    let key = (commit, path.to_owned());
    if let Some(hit) = state.last_commits.get(&key) {
        return Ok(Some(hit));
    }
    let (found, complete) = g.last_commits(commit, path, budget)?;
    if !complete {
        return Ok(None);
    }
    let found = Arc::new(found);
    state.last_commits.insert(key, found.clone());
    Ok(Some(found))
}

#[derive(serde::Serialize)]
struct LastCommitJson {
    summary: String,
    url: String,
    when: String,
    when_full: String,
}

/// JSON map of entry name to its last commit, for tree pages that were
/// rendered before this was known.
pub async fn last_commits_json(
    State(state): State<AppState>,
    Viewer(viewer): Viewer,
    Path((owner, name, rest)): Path<(String, String, String)>,
) -> AppResult {
    let (repo, _) = load(&state, viewer.as_ref(), &owner, &name).await?;
    let base = format!("/{}/{}", repo.owner, repo.name);
    let st = state.clone();
    let found = with_git(&state, &repo, move |g| {
        let r = g.resolve_rev_path(&rest).ok_or(AppError::NotFound)?;
        Ok(last_commits(&st, g, r.commit, &r.path, LAST_COMMIT_ASYNC_WALK)?.unwrap_or_default())
    })
    .await?;
    let json: HashMap<&str, LastCommitJson> = found
        .iter()
        .map(|(name, c)| {
            let v = CommitView::new(&base, c);
            (
                name.as_str(),
                LastCommitJson { summary: v.summary, url: v.url, when: v.when, when_full: v.when_full },
            )
        })
        .collect();
    let cache = if repo.is_private { "private, max-age=300" } else { "public, max-age=300" };
    Ok((
        [(header::CONTENT_TYPE, "application/json"), (header::CACHE_CONTROL, cache)],
        serde_json::to_string(&json).map_err(anyhow::Error::from)?,
    )
        .into_response())
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() {
        name.to_owned()
    } else {
        format!("{dir}/{name}")
    }
}

/// Renders Markdown, or other text as a preformatted block. Cached by blob
/// id and link base, since relative links depend on where the file is shown.
fn render_text_doc(state: &AppState, name: &str, blob: &keel_git::Blob, links: &LinkBase<'_>) -> Arc<String> {
    let key = (blob.id, format!("{}\0{}\0{}", links.blob, links.dir, name));
    if let Some(hit) = state.markdown.get(&key) {
        return hit;
    }
    let text = String::from_utf8_lossy(&blob.data);
    let html = if matches!(extension(name).as_str(), "md" | "markdown") {
        render::markdown(&text, links)
    } else {
        format!("<pre class=\"plain-doc\">{}</pre>", render::escape(&text))
    };
    let html = Arc::new(html);
    state.markdown.insert(key, html.clone());
    html
}

// ---------------------------------------------------------------------------
// Blob and raw
// ---------------------------------------------------------------------------

pub(crate) enum BlobContent {
    Lines(Arc<Vec<String>>),
    Rendered(Arc<String>),
    Image,
    Binary,
    TooLarge,
}

#[derive(Template)]
#[template(path = "repo/blob.html")]
struct BlobPage {
    page: Page,
    repo: RepoView,
    rev: String,
    rev_url: String,
    path: String,
    crumbs: Vec<Crumb>,
    size: String,
    line_count: usize,
    content: BlobContent,
    is_markdown: bool,
    plain: bool,
    raw_url: String,
    history_url: String,
    latest: Option<CommitView>,
    refs: RefNames,
}

#[derive(Deserialize)]
pub struct BlobQuery {
    plain: Option<String>,
}

pub async fn blob(
    State(state): State<AppState>,
    Viewer(viewer): Viewer,
    Path((owner, name, rest)): Path<(String, String, String)>,
    Query(q): Query<BlobQuery>,
) -> AppResult {
    let (repo, access) = load(&state, viewer.as_ref(), &owner, &name).await?;
    let base = format!("/{}/{}", repo.owner, repo.name);
    let plain = q.plain.is_some_and(|p| p != "0");
    let st = state.clone();
    let link_base = base.clone();

    let result = with_git(&state, &repo, move |g| {
        let r = g.resolve_rev_path(&rest).ok_or(AppError::NotFound)?;
        if r.path.is_empty() || g.is_dir(r.commit, &r.path)? {
            return Ok(Err(r));
        }
        let blob = g.blob(r.commit, &r.path)?.ok_or(AppError::NotFound)?;
        let filename = r.path.rsplit('/').next().unwrap_or_default().to_owned();
        let ext = extension(&filename);
        let is_markdown = matches!(ext.as_str(), "md" | "markdown");
        let size = blob.data.len();

        let content = if IMAGE_EXTENSIONS.contains(&ext.as_str()) {
            BlobContent::Image
        } else if blob.binary {
            BlobContent::Binary
        } else if size > MAX_DISPLAY_BYTES {
            BlobContent::TooLarge
        } else if is_markdown && !plain {
            let dir = r.path.rsplit_once('/').map(|(d, _)| d).unwrap_or("");
            let rev = encode_path(&r.rev);
            let links = LinkBase {
                blob: &format!("{link_base}/blob/{rev}"),
                raw: &format!("{link_base}/raw/{rev}"),
                dir,
            };
            BlobContent::Rendered(render_text_doc(&st, &filename, &blob, &links))
        } else {
            let key = (blob.id, filename.clone());
            let lines = match st.highlighted.get(&key) {
                Some(hit) => hit,
                None => {
                    let text = String::from_utf8_lossy(&blob.data);
                    let lines = Arc::new(render::highlight_lines(&filename, &text));
                    st.highlighted.insert(key, lines.clone());
                    lines
                }
            };
            BlobContent::Lines(lines)
        };
        let line_count = match &content {
            BlobContent::Lines(l) => l.len(),
            _ => bytecount_lines(&blob.data),
        };
        let latest = g.log(r.commit, &r.path, 0, 1)?.into_iter().next();
        Ok(Ok((r, g.default_branch()?.unwrap_or_default(), size, line_count, content, is_markdown, latest, RefNames::load(g, &link_base, "tree")?)))
    })
    .await?;

    let (r, default_branch, size, line_count, content, is_markdown, latest, refs) = match result {
        Ok(v) => v,
        Err(r) => {
            return Ok(Redirect::to(&format!("{base}/tree/{}/{}", encode_path(&r.rev), encode_path(&r.path)))
                .into_response());
        }
    };
    let rev_url = encode_path(&r.rev);
    let path_url = encode_path(&r.path);
    html(BlobPage {
        page: Page::new(&state, viewer, format!("{} at {} · {}/{}", r.path, r.rev, repo.owner, repo.name)),
        repo: RepoView::new(&state, &repo, access, "code", &default_branch),
        crumbs: crumbs(&base, &r.rev, &r.path),
        size: human_size(size),
        line_count,
        content,
        is_markdown,
        plain,
        raw_url: format!("{base}/raw/{rev_url}/{path_url}"),
        history_url: format!("{base}/commits/{rev_url}/{path_url}"),
        latest: latest.as_ref().map(|c| CommitView::new(&base, c)),
        rev: r.rev,
        rev_url,
        path: r.path,
        refs,
    })
}

fn bytecount_lines(data: &[u8]) -> usize {
    data.iter().filter(|&&b| b == b'\n').count()
}

pub async fn raw(
    State(state): State<AppState>,
    Viewer(viewer): Viewer,
    Path((owner, name, rest)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> AppResult {
    let (repo, _) = load(&state, viewer.as_ref(), &owner, &name).await?;
    let (r, blob) = with_git(&state, &repo, move |g| {
        let r = g.resolve_rev_path(&rest).ok_or(AppError::NotFound)?;
        let blob = g.blob(r.commit, &r.path)?.ok_or(AppError::NotFound)?;
        Ok((r, blob))
    })
    .await?;

    let etag = format!("\"{}\"", blob.id);
    let cache = match (repo.is_private, r.rev.len() == 40) {
        // A full commit hash pins the content forever.
        (false, true) => "public, max-age=31536000, immutable",
        (false, false) => "public, no-cache",
        (true, _) => "private, no-cache",
    };
    let mut res = if headers
        .get(header::IF_NONE_MATCH)
        .is_some_and(|v| v.as_bytes() == etag.as_bytes())
    {
        StatusCode::NOT_MODIFIED.into_response()
    } else {
        let content_type = match extension(&r.path).as_str() {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "avif" => "image/avif",
            "bmp" => "image/bmp",
            "ico" => "image/x-icon",
            "svg" => "image/svg+xml",
            "pdf" => "application/pdf",
            // Never text/html: user content must not run on our origin.
            _ if !blob.binary => "text/plain; charset=utf-8",
            _ => "application/octet-stream",
        };
        ([(header::CONTENT_TYPE, content_type)], blob.data).into_response()
    };
    let h = res.headers_mut();
    h.insert(header::ETAG, HeaderValue::from_str(&etag).expect("hex"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    // Sandboxing stops scripts in e.g. SVG files from running as our origin.
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("sandbox; default-src 'none'; img-src 'self'; style-src 'unsafe-inline'"));
    Ok(res)
}

// ---------------------------------------------------------------------------
// History
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "repo/commits.html")]
struct CommitsPage {
    page: Page,
    repo: RepoView,
    rev: String,
    path: String,
    commits: Vec<CommitView>,
    newer_url: Option<String>,
    older_url: Option<String>,
    refs: RefNames,
}

#[derive(Deserialize)]
pub struct PageQuery {
    page: Option<usize>,
}

pub async fn commits_default(
    State(state): State<AppState>,
    Viewer(viewer): Viewer,
    Path((owner, name)): Path<(String, String)>,
) -> AppResult {
    let (repo, access) = load(&state, viewer.as_ref(), &owner, &name).await?;
    let branch = with_git(&state, &repo, |g| Ok(g.default_branch()?)).await?;
    match branch {
        Some(b) => Ok(Redirect::to(&format!("/{}/{}/commits/{}", repo.owner, repo.name, encode_path(&b))).into_response()),
        None => html(EmptyPage {
            repo: RepoView::new(&state, &repo, access, "commits", ""),
            page: Page::new(&state, viewer, format!("{}/{}", repo.owner, repo.name)),
        }),
    }
}

pub async fn commits(
    State(state): State<AppState>,
    Viewer(viewer): Viewer,
    Path((owner, name, rest)): Path<(String, String, String)>,
    Query(q): Query<PageQuery>,
) -> AppResult {
    let (repo, access) = load(&state, viewer.as_ref(), &owner, &name).await?;
    let base = format!("/{}/{}", repo.owner, repo.name);
    let page_no = q.page.unwrap_or(1).max(1);
    let link_base = base.clone();
    let (r, default_branch, mut list, refs) = with_git(&state, &repo, move |g| {
        let r = g.resolve_rev_path(&rest).ok_or(AppError::NotFound)?;
        // One extra to know whether there is an older page.
        let list = g.log(r.commit, &r.path, (page_no - 1) * COMMITS_PER_PAGE, COMMITS_PER_PAGE + 1)?;
        Ok((r, g.default_branch()?.unwrap_or_default(), list, RefNames::load(g, &link_base, "commits")?))
    })
    .await?;

    let has_older = list.len() > COMMITS_PER_PAGE;
    list.truncate(COMMITS_PER_PAGE);
    let here = format!("{base}/commits/{}/{}", encode_path(&r.rev), encode_path(&r.path));
    let here = here.trim_end_matches('/').to_owned();
    html(CommitsPage {
        page: Page::new(&state, viewer, format!("Commits · {}/{}", repo.owner, repo.name)),
        repo: RepoView::new(&state, &repo, access, "commits", &default_branch),
        commits: list.iter().map(|c| CommitView::new(&base, c)).collect(),
        newer_url: (page_no > 1).then(|| format!("{here}?page={}", page_no - 1)),
        older_url: has_older.then(|| format!("{here}?page={}", page_no + 1)),
        rev: r.rev,
        path: r.path,
        refs,
    })
}

pub(crate) struct LineView {
    pub class: &'static str,
    pub old: String,
    pub new: String,
    pub content: String,
}

pub(crate) struct HunkView {
    pub header: String,
    pub lines: Vec<LineView>,
}

pub(crate) struct FileView {
    pub anchor: String,
    pub path: String,
    pub old_path: Option<String>,
    pub status: &'static str,
    pub status_class: &'static str,
    pub additions: usize,
    pub deletions: usize,
    pub binary: bool,
    pub collapsed: bool,
    pub hunks: Vec<HunkView>,
    pub view_url: Option<String>,
}

#[derive(Template)]
#[template(path = "repo/commit.html")]
struct CommitPage {
    page: Page,
    repo: RepoView,
    commit: CommitView,
    body: String,
    committer: String,
    parents: Vec<(String, String)>,
    files: Vec<FileView>,
    additions: usize,
    deletions: usize,
    truncated: bool,
}

pub async fn commit(
    State(state): State<AppState>,
    Viewer(viewer): Viewer,
    Path((owner, name, sha)): Path<(String, String, String)>,
) -> AppResult {
    let (repo, access) = load(&state, viewer.as_ref(), &owner, &name).await?;
    let base = format!("/{}/{}", repo.owner, repo.name);
    let (detail, diff, default_branch) = with_git(&state, &repo, move |g| {
        let id = g.resolve_rev(&sha).ok_or(AppError::NotFound)?;
        Ok((
            g.commit_detail(id)?,
            g.diff_commit(id, DIFF_MAX_FILES, DIFF_MAX_LINES)?,
            g.default_branch()?.unwrap_or_default(),
        ))
    })
    .await?;

    let commit_id = detail.commit.id.to_string();
    let files = diff
        .files
        .iter()
        .enumerate()
        .map(|(i, f)| FileView {
            anchor: format!("file-{i}"),
            path: f.path().to_owned(),
            old_path: (f.status == DiffStatus::Renamed).then(|| f.old_path.clone()).flatten(),
            status: match f.status {
                DiffStatus::Added => "added",
                DiffStatus::Deleted => "deleted",
                DiffStatus::Modified => "modified",
                DiffStatus::Renamed => "renamed",
                DiffStatus::Copied => "copied",
                DiffStatus::TypeChange => "type changed",
            },
            status_class: match f.status {
                DiffStatus::Added | DiffStatus::Copied => "added",
                DiffStatus::Deleted => "deleted",
                DiffStatus::Renamed => "renamed",
                _ => "modified",
            },
            additions: f.additions,
            deletions: f.deletions,
            binary: f.binary,
            collapsed: f.collapsed,
            view_url: (f.status != DiffStatus::Deleted)
                .then(|| format!("{base}/blob/{commit_id}/{}", encode_path(f.path()))),
            hunks: f
                .hunks
                .iter()
                .map(|h| HunkView {
                    header: h.header.clone(),
                    lines: h
                        .lines
                        .iter()
                        .map(|l| LineView {
                            class: match l.kind {
                                DiffLineKind::Added => "add",
                                DiffLineKind::Removed => "del",
                                DiffLineKind::Context => "ctx",
                                DiffLineKind::NoNewline => "meta",
                            },
                            old: l.old_no.map(|n| n.to_string()).unwrap_or_default(),
                            new: l.new_no.map(|n| n.to_string()).unwrap_or_default(),
                            content: l.content.clone(),
                        })
                        .collect(),
                })
                .collect(),
        })
        .collect();

    html(CommitPage {
        page: Page::new(&state, viewer, format!("{} · {}/{}", detail.commit.summary, repo.owner, repo.name)),
        repo: RepoView::new(&state, &repo, access, "commits", &default_branch),
        commit: CommitView::new(&base, &detail.commit),
        body: detail.body,
        committer: detail.committer.name,
        parents: detail
            .parents
            .iter()
            .map(|p: &Oid| (p.to_string()[..10].to_owned(), format!("{base}/commit/{p}")))
            .collect(),
        files,
        additions: diff.additions,
        deletions: diff.deletions,
        truncated: diff.truncated,
    })
}

// ---------------------------------------------------------------------------
// Branches and tags
// ---------------------------------------------------------------------------

pub(crate) struct RefView {
    pub name: String,
    pub url: String,
    pub commit: CommitView,
    pub is_default: bool,
}

#[derive(Template)]
#[template(path = "repo/refs.html")]
struct RefsPage {
    page: Page,
    repo: RepoView,
    branches: Vec<RefView>,
    tags: Vec<RefView>,
}

pub async fn refs(
    State(state): State<AppState>,
    Viewer(viewer): Viewer,
    Path((owner, name)): Path<(String, String)>,
) -> AppResult {
    let (repo, access) = load(&state, viewer.as_ref(), &owner, &name).await?;
    let base = format!("/{}/{}", repo.owner, repo.name);
    let (branches, tags, default_branch) = with_git(&state, &repo, |g| {
        Ok((g.branches()?, g.tags()?, g.default_branch()?.unwrap_or_default()))
    })
    .await?;
    let view = |r: &keel_git::RefInfo| RefView {
        url: format!("{base}/tree/{}", encode_path(&r.name)),
        commit: CommitView::new(&base, &r.commit),
        is_default: r.name == default_branch,
        name: r.name.clone(),
    };
    html(RefsPage {
        page: Page::new(&state, viewer, format!("Branches · {}/{}", repo.owner, repo.name)),
        repo: RepoView::new(&state, &repo, access, "branches", &default_branch),
        branches: branches.iter().map(view).collect(),
        tags: tags.iter().map(view).collect(),
    })
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "repo/settings.html")]
struct SettingsPage {
    page: Page,
    repo: RepoView,
    error: Option<String>,
}

async fn writable(state: &AppState, user: &User, owner: &str, name: &str) -> AppResult<Repo> {
    let (repo, access) = load(state, Some(user), owner, name).await?;
    if access != Access::Write {
        return Err(AppError::Forbidden);
    }
    Ok(repo)
}

async fn settings_page(state: &AppState, user: User, repo: &Repo, error: Option<String>) -> AppResult {
    let default_branch = with_git(state, repo, |g| Ok(g.default_branch()?.unwrap_or_default())).await?;
    html(SettingsPage {
        repo: RepoView::new(state, repo, Access::Write, "settings", &default_branch),
        page: Page::new(state, Some(user), format!("Settings · {}/{}", repo.owner, repo.name)),
        error,
    })
}

pub async fn settings(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name)): Path<(String, String)>,
) -> AppResult {
    let repo = writable(&state, &user, &owner, &name).await?;
    settings_page(&state, user, &repo, None).await
}

#[derive(Deserialize)]
pub struct SettingsForm {
    #[serde(default)]
    description: String,
    private: Option<String>,
}

pub async fn update_settings(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name)): Path<(String, String)>,
    Form(f): Form<SettingsForm>,
) -> AppResult {
    let repo = writable(&state, &user, &owner, &name).await?;
    state.core.update_repo(repo.id, &f.description, f.private.is_some()).await?;
    Ok(Redirect::to(&format!("/{}/{}/settings", repo.owner, repo.name)).into_response())
}

#[derive(Deserialize)]
pub struct DeleteForm {
    confirm: String,
}

pub async fn delete(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name)): Path<(String, String)>,
    Form(f): Form<DeleteForm>,
) -> Result<Response, AppError> {
    let repo = writable(&state, &user, &owner, &name).await?;
    let full = format!("{}/{}", repo.owner, repo.name);
    if f.confirm.trim() != full {
        let msg = format!("To delete, type “{full}” exactly.");
        return settings_page(&state, user, &repo, Some(msg)).await;
    }
    state.core.delete_repo(&repo).await?;
    Ok(Redirect::to(&format!("/{}", repo.owner)).into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_paths() {
        assert_eq!(encode_path("src/main.rs"), "src/main.rs");
        assert_eq!(encode_path("a b/c#d?.txt"), "a%20b/c%23d%3F.txt");
        assert_eq!(encode_path("ünï"), "%C3%BCn%C3%AF");
    }

    #[test]
    fn breadcrumbs() {
        let c = crumbs("/ada/engine", "feature/x", "src/bin");
        assert_eq!(c.len(), 2);
        assert_eq!(c[1].url, "/ada/engine/tree/feature/x/src/bin");
    }
}
