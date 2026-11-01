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
use crate::lastcommit::{BACKGROUND_WALK, last_commits_for};
use crate::render::{self, LinkBase, datetime, human_size, relative_time};
use crate::session::{SignedIn, Viewer};
use crate::{AppError, AppState, Page};

/// Commits to walk while rendering a tree page. If that doesn't resolve
/// every entry's last commit, the page is sent without them and the browser
/// fetches them from `last-commits` (without a budget) instead, so a large
/// history never delays the first paint.
const LAST_COMMIT_SYNC_WALK: usize = 500;
const COMMITS_PER_PAGE: usize = 50;
/// Larger files are offered as a raw download instead of being rendered.
pub(crate) const MAX_DISPLAY_BYTES: usize = 2 * 1024 * 1024;
pub(crate) const DIFF_MAX_FILES: usize = 300;
pub(crate) const DIFF_MAX_LINES: usize = 10_000;
const README_NAMES: &[&str] = &["readme.md", "readme.markdown", "readme", "readme.txt"];
const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "gif", "webp", "svg", "ico", "bmp", "avif"];

// ---------------------------------------------------------------------------
// Shared helpers and view models
// ---------------------------------------------------------------------------

/// Percent-encodes a repository path or ref for use in a URL, keeping `/`
/// and `:` (valid in paths; it separates a fork from its branch in compare
/// URLs).
pub(crate) fn encode_path(p: &str) -> String {
    let mut out = String::with_capacity(p.len());
    for b in p.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' | b':' => {
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
pub(crate) async fn load(state: &AppState, viewer: Option<&User>, owner: &str, name: &str) -> AppResult<(Repo, Access)> {
    let name = name.strip_suffix(".git").unwrap_or(name);
    let repo = state.core.repo(owner, name).await?.ok_or(AppError::NotFound)?;
    match state.core.access(&repo, viewer).await? {
        Access::None => Err(AppError::NotFound),
        access => Ok((repo, access)),
    }
}

/// Runs `f` against the repository on the blocking thread pool.
pub(crate) async fn with_git<T: Send + 'static>(
    state: &AppState,
    repo: &Repo,
    f: impl FnOnce(&GitRepo) -> AppResult<T> + Send + 'static,
) -> AppResult<T> {
    let path = state.core.repo_path(repo);
    tokio::task::spawn_blocking(move || f(&GitRepo::open(&path)?)).await?
}

/// Like [`with_git`], but `repo` can also read the objects of `other` (a
/// fork, for comparing across repositories).
pub(crate) async fn with_git_pair<T: Send + 'static>(
    state: &AppState,
    repo: &Repo,
    other: &Repo,
    f: impl FnOnce(&GitRepo) -> AppResult<T> + Send + 'static,
) -> AppResult<T> {
    if repo.id == other.id {
        return with_git(state, repo, f).await;
    }
    let (path, other) = (state.core.repo_path(repo), state.core.repo_path(other));
    tokio::task::spawn_blocking(move || f(&GitRepo::open_with(&path, &other)?)).await?
}

pub(crate) struct RepoView {
    pub owner: String,
    pub name: String,
    pub description: String,
    pub is_private: bool,
    pub can_write: bool,
    /// May change settings and collaborators (also of an archived
    /// repository).
    pub can_admin: bool,
    /// When it was archived, if it is.
    pub archived: Option<String>,
    pub http_url: String,
    pub ssh_url: Option<String>,
    /// Which tab is active: code, commits, branches or settings.
    pub tab: &'static str,
    /// Branch used for tab links; empty for an empty repository.
    pub default_rev: String,
    pub open_issues: i64,
    pub open_pulls: i64,
    /// `owner/name` of the repository this was forked from.
    pub parent: Option<String>,
    pub is_template: bool,
    /// `owner/name` of the template this was generated from.
    pub template: Option<String>,
    pub fork_count: i64,
    pub watchers: i64,
    /// Whether the viewer watches it (`None` when signed out).
    pub watching: Option<bool>,
    pub stars: i64,
    /// Whether the viewer starred it (`None` when signed out).
    pub starred: Option<bool>,
    pub mirror: Option<MirrorView>,
    /// Published releases.
    pub releases: i64,
    pub topics: Vec<String>,
    /// Show the wiki tab (there is a wiki, or the viewer can start one).
    pub wiki_tab: bool,
    /// Open projects, and whether to show the tab (there are projects, or
    /// the viewer can start one).
    pub open_projects: i64,
    pub projects_tab: bool,
    /// An import that's running, or ended recently with problems.
    pub import: Option<ImportView>,
    /// The front page's sidebar shows the description and topics, so the
    /// header leaves them out.
    pub about_in_side: bool,
}

pub(crate) struct ImportView {
    pub url: String,
    pub service: &'static str,
    pub running: bool,
    pub progress: String,
    pub error: Option<String>,
}


/// What the header says about a mirror.
pub(crate) struct MirrorView {
    pub url: String,
    /// "synced 3 hours ago", or that the first sync is pending.
    pub status: String,
    pub failed: bool,
    /// The last error, for people with write access.
    pub error: Option<String>,
}

impl RepoView {
    pub(crate) async fn new(
        state: &AppState,
        repo: &Repo,
        access: Access,
        viewer: Option<&User>,
        tab: &'static str,
        default_rev: &str,
    ) -> AppResult<Self> {
        let cfg = &state.core.config;
        let counts = state.core.issue_counts(repo.id).await?;
        let parent = match repo.forked_from {
            Some(id) => state.core.repo_by_id(id).await?.map(|p| format!("{}/{}", p.owner, p.name)),
            None => None,
        };
        let mirror = state.core.mirror(repo.id).await?.map(|m| MirrorView {
            status: match m.synced_at {
                Some(t) => format!("synced {}", relative_time(t)),
                None if m.last_error.is_none() => "first sync in progress".into(),
                None => "not synced yet".into(),
            },
            failed: m.last_error.is_some(),
            error: m.last_error.filter(|_| access >= Access::Write),
            url: m.url,
        });
        let import = state.core.import(repo.id).await?.filter(|i| i.worth_showing());
        let (open_projects, closed_projects) = state.core.project_counts(keel_core::ProjectOwner::Repo(repo.id)).await?;
        Ok(Self {
            import: import.map(|i| ImportView {
                running: i.in_progress(),
                service: i.service().name(),
                // The error is for those who can do something about it.
                error: i.error.clone().filter(|_| access >= Access::Write),
                progress: i.progress,
                url: i.url,
            }),
            releases: state.core.release_count(repo.id).await?,
            topics: repo.topics().map(str::to_owned).collect(),
            wiki_tab: access >= Access::Write || state.core.has_wiki(repo.id),
            open_projects,
            projects_tab: access >= Access::Write || open_projects + closed_projects > 0,
            mirror,
            parent,
            is_template: repo.is_template,
            template: state.core.template_of(repo, viewer).await?.map(|t| format!("{}/{}", t.owner, t.name)),
            fork_count: state.core.fork_count(repo.id).await?,
            watchers: state.core.watcher_count(repo.id).await?,
            watching: match viewer {
                Some(v) => Some(state.core.is_watching(v.id, repo.id).await?),
                None => None,
            },
            stars: repo.stars,
            starred: match viewer {
                Some(v) => Some(state.core.is_starred(v.id, repo.id).await?),
                None => None,
            },
            open_issues: counts.open_issues,
            open_pulls: counts.open_pulls,
            owner: repo.owner.clone(),
            name: repo.name.clone(),
            description: repo.description.clone(),
            is_private: repo.is_private,
            can_write: access >= Access::Write,
            can_admin: access == Access::Admin
                || repo.is_archived() && state.core.granted_access(repo, viewer).await? == Access::Admin,
            archived: repo.archived_at.map(crate::render::date),
            http_url: cfg.http_clone_url(&repo.owner, &repo.name),
            ssh_url: cfg.ssh_clone_url(&repo.owner, &repo.name),
            tab,
            default_rev: encode_path(default_rev),
            about_in_side: false,
        })
    }
}

pub(crate) struct CommitView {
    pub id: String,
    pub short: String,
    pub summary: String,
    pub author: String,
    pub author_email: String,
    /// The history filtered to the author's commits (set by
    /// [`link_authors`]).
    pub author_url: Option<String>,
    /// The author's account, if their address belongs to one.
    pub author_username: Option<String>,
    pub when: String,
    pub when_full: String,
    pub url: String,
    /// The status badge (HTML), if CI reported anything.
    pub status: String,
    /// Set for signed commits by [`crate::signatures::attach`].
    pub signature: Option<crate::signatures::SignatureView>,
}

/// Makes the authors of `commits` links: their name to the history at
/// `history` filtered to their commits, and their avatar to their profile
/// if their address belongs to an account.
pub(crate) async fn link_authors(state: &AppState, history: &str, commits: &mut [&mut CommitView]) -> AppResult<()> {
    let emails: Vec<String> = commits.iter().map(|c| c.author_email.to_lowercase()).collect();
    let accounts = state.core.commit_authors(&emails.iter().map(String::as_str).collect::<Vec<_>>()).await?;
    for (c, email) in commits.iter_mut().zip(&emails) {
        if c.author_email.is_empty() {
            continue;
        }
        let encoded: String = form_urlencoded::byte_serialize(c.author_email.as_bytes()).collect();
        c.author_url = Some(format!("{history}?author={encoded}"));
        c.author_username = accounts.get(email).cloned();
    }
    Ok(())
}

impl CommitView {
    pub(crate) fn new(base: &str, c: &CommitSummary) -> Self {
        Self {
            id: c.id.to_string(),
            short: c.short_id(),
            summary: c.summary.clone(),
            author: c.author.name.clone(),
            author_email: c.author.email.clone(),
            author_url: None,
            author_username: None,
            when: relative_time(c.author.time),
            when_full: datetime(c.author.time),
            url: format!("{base}/commit/{}", c.id),
            status: String::new(),
            signature: None,
        }
    }
}

pub(crate) struct Crumb {
    pub name: String,
    pub url: String,
}

/// Breadcrumbs for `path`, each linking to its directory.
pub(crate) fn crumbs(base: &str, rev: &str, path: &str) -> Vec<Crumb> {
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
    pub(crate) fn load(g: &GitRepo, base: &str, mode: &str) -> AppResult<Self> {
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
    /// Where to add files here, for people who can.
    new_file_url: Option<String>,
    upload_url: Option<String>,
    /// On the front page (the top of the tree).
    side: Option<crate::repo_side::RepoSide>,
    /// In a folder: the file tree beside it (HTML), as beside files.
    tree: String,
}

#[derive(Template)]
#[template(path = "repo/empty.html")]
struct EmptyPage {
    page: Page,
    repo: RepoView,
    /// The branch the first commit goes to.
    head: String,
}

async fn empty_page(state: &AppState, repo: &Repo, access: Access, viewer: Option<User>, tab: &'static str) -> AppResult {
    let head = with_git(state, repo, |g| Ok(g.head_branch().unwrap_or_else(|| "main".into()))).await?;
    let mut view = RepoView::new(state, repo, access, viewer.as_ref(), tab, "").await?;
    // Mirrors are changed where they come from.
    view.can_write &= view.mirror.is_none();
    html(EmptyPage {
        repo: view,
        page: Page::new(state, viewer, format!("{}/{}", repo.owner, repo.name)),
        head: encode_path(&head),
    })
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
        latest: Option<CommitSummary>,
        readme: Option<(String, Arc<String>)>,
        refs: RefNames,
        /// At the top of the tree: the license file and which license it is.
        license: Option<(String, Option<&'static str>)>,
        tree: String,
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

        let license = match entries.iter().find(|e| {
            resolved.path.is_empty()
                && matches!(e.kind, EntryKind::File | EntryKind::Executable)
                && crate::repo_side::is_license_file(&e.name)
        }) {
            Some(e) => {
                let text = g.blob(resolved.commit, &e.name)?.filter(|b| !b.binary && b.data.len() <= 256 * 1024);
                let kind = text.and_then(|b| crate::repo_side::license_name(&String::from_utf8_lossy(&b.data)));
                Some((e.name.clone(), kind))
            }
            None => None,
        };

        let tree = if resolved.path.is_empty() {
            String::new()
        } else {
            crate::file_view::tree_html(g, &link_base, &resolved.rev, resolved.commit, &format!("{}/", resolved.path))?
        };
        Ok(TreeData::Dir {
            tree,
            license,
            refs: RefNames::load(g, &link_base, "tree")?,
            default_branch: default_branch.unwrap_or_default(),
            resolved,
            entries,
            latest,
            readme,
        })
    })
    .await?;

    match data {
        TreeData::Empty => empty_page(&state, &repo, access, viewer, "code").await,
        TreeData::File(r) => Ok(Redirect::to(&format!(
            "{base}/blob/{}/{}",
            encode_path(&r.rev),
            encode_path(&r.path)
        ))
        .into_response()),
        TreeData::Dir { resolved, default_branch, entries, latest, readme, refs, license, tree } => {
            let last = last_commits_for(&state, &repo, resolved.commit, &resolved.path, LAST_COMMIT_SYNC_WALK).await?;
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
            let mut latest = latest.as_ref().map(|c| CommitView::new(&base, c));
            let history = format!("{base}/commits/{rev_url}/{}", encode_path(&resolved.path));
            if let Some(c) = latest.as_mut() {
                crate::statuses::attach(&state, &repo, &mut [&mut *c]).await?;
                crate::signatures::attach(&state, &repo, &mut [&mut *c]).await?;
                link_authors(&state, history.trim_end_matches('/'), &mut [c]).await?;
            }
            let mut view = RepoView::new(&state, &repo, access, viewer.as_ref(), "code", &default_branch).await?;
            let side = if resolved.path.is_empty() {
                view.about_in_side = true;
                let has_readme = readme.is_some();
                Some(crate::repo_side::load(&state, &repo, &view, &resolved.rev, resolved.commit, has_readme, license).await?)
            } else {
                None
            };
            let here = (view.can_write && view.mirror.is_none() && refs.branches.iter().any(|b| b.name == resolved.rev))
                .then(|| format!("{rev_url}{}", if resolved.path.is_empty() { String::new() } else { format!("/{}", encode_path(&resolved.path)) }));
            html(TreePage {
                side,
                tree,
                new_file_url: here.as_ref().map(|h| format!("{base}/new/{h}")),
                upload_url: here.map(|h| format!("{base}/upload/{h}")),
                repo: view,
                page: Page::new(&state, viewer, title),
                crumbs: crumbs(&base, &resolved.rev, &resolved.path),
                history_url: format!("{base}/commits/{rev_url}/{}", encode_path(&resolved.path)),
                // A full commit id keeps the URL stable (and cacheable) even
                // if the branch moves while the page is open.
                last_commits_url: last.is_none().then(|| {
                    format!("{base}/last-commits/{}/{}", resolved.commit, encode_path(&resolved.path))
                }),
                latest,
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
    let r = with_git(&state, &repo, move |g| g.resolve_rev_path(&rest).ok_or(AppError::NotFound)).await?;
    let found = last_commits_for(&state, &repo, r.commit, &r.path, BACKGROUND_WALK)
        .await?
        .unwrap_or_default();
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
    /// A Git LFS pointer: the file's size, and whether it was uploaded.
    Lfs { size: String, stored: bool },
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
    content: BlobContent,
    raw_url: String,
    history_url: String,
    latest: Option<CommitView>,
    refs: RefNames,
    /// The bar above the content: views, raw, copy, download, edit, delete.
    bar: crate::file_view::FileBar,
    /// The file tree beside it (HTML).
    tree: String,
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

        let lfs = keel_core::parse_pointer(&blob.data);
        let content = if let Some((_, size)) = &lfs {
            // Whether it's stored is checked below.
            BlobContent::Lfs { size: human_size(*size as usize), stored: false }
        } else if IMAGE_EXTENSIONS.contains(&ext.as_str()) {
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
            BlobContent::Lines(highlighted(&st, &blob, &filename))
        };
        let line_count = match &content {
            BlobContent::Lines(l) => l.len(),
            _ => bytecount_lines(&blob.data),
        };
        let latest = g.log(r.commit, &r.path, 0, 1)?.into_iter().next();
        let image = IMAGE_EXTENSIONS.contains(&ext.as_str());
        let tree = crate::file_view::tree_html(g, &link_base, &r.rev, r.commit, &r.path)?;
        Ok(Ok((r, g.default_branch()?.unwrap_or_default(), size, line_count, content, is_markdown, latest, RefNames::load(g, &link_base, "tree")?, lfs, image, tree)))
    })
    .await?;

    let (r, default_branch, size, line_count, mut content, is_markdown, latest, refs, lfs, image, tree) = match result {
        Ok(v) => v,
        Err(r) => {
            return Ok(Redirect::to(&format!("{base}/tree/{}/{}", encode_path(&r.rev), encode_path(&r.path)))
                .into_response());
        }
    };
    // For LFS files, the size of the file rather than of its pointer.
    let shown_size = match &lfs {
        Some((_, s)) => human_size(*s as usize),
        None => human_size(size),
    };
    let line_count = if lfs.is_some() { 0 } else { line_count };
    if let Some((oid, _)) = lfs
        && state.core.lfs_object(repo.id, &oid).await?.is_some()
    {
        content = match content {
            _ if image => BlobContent::Image,
            BlobContent::Lfs { size, .. } => BlobContent::Lfs { size, stored: true },
            other => other,
        };
    }
    let locked_by = state.core.lfs_locks(repo.id, Some(&r.path), None, 0, 1).await?.into_iter().next().map(|l| l.owner);
    let rev_url = encode_path(&r.rev);
    let path_url = encode_path(&r.path);
    let view = RepoView::new(&state, &repo, access, viewer.as_ref(), "code", &default_branch).await?;
    let changeable = view.can_write && view.mirror.is_none() && refs.branches.iter().any(|b| b.name == r.rev);
    let editable = changeable && size <= 1024 * 1024 && matches!(content, BlobContent::Lines(_) | BlobContent::Rendered(_));
    let mut latest = latest.as_ref().map(|c| CommitView::new(&base, c));
    if let Some(c) = latest.as_mut() {
        link_authors(&state, &format!("{base}/commits/{rev_url}/{path_url}"), &mut [c]).await?;
    }
    let text = matches!(content, BlobContent::Lines(_) | BlobContent::Rendered(_));
    let mode = if is_markdown && !plain && text { "preview" } else { "code" };
    let mut bar = crate::file_view::FileBar::new(&base, &rev_url, &r.path, mode, is_markdown && text, text);
    bar.meta = if line_count > 0 { format!("{line_count} lines · {shown_size}") } else { shown_size.clone() };
    bar.edit_url = editable.then(|| format!("{base}/edit/{rev_url}/{path_url}"));
    bar.delete_url = changeable.then(|| format!("{base}/delete/{rev_url}/{path_url}"));
    bar.locked_by = locked_by;
    html(BlobPage {
        bar,
        tree,
        repo: view,
        page: Page::new(&state, viewer, format!("{} at {} · {}/{}", r.path, r.rev, repo.owner, repo.name)),
        crumbs: crumbs(&base, &r.rev, &r.path),
        content,
        raw_url: format!("{base}/raw/{rev_url}/{path_url}"),
        history_url: format!("{base}/commits/{rev_url}/{path_url}"),
        latest,
        rev: r.rev,
        rev_url,
        path: r.path,
        refs,
    })
}

/// The lines of a text file as highlighted HTML, cached by blob.
pub(crate) fn highlighted(state: &AppState, blob: &keel_git::Blob, filename: &str) -> Arc<Vec<String>> {
    let key = (blob.id, filename.to_owned());
    if let Some(hit) = state.highlighted.get(&key) {
        return hit;
    }
    let text = String::from_utf8_lossy(&blob.data);
    let lines = Arc::new(render::highlight_lines(filename, &text));
    state.highlighted.insert(key, lines.clone());
    lines
}

fn bytecount_lines(data: &[u8]) -> usize {
    data.iter().filter(|&&b| b == b'\n').count()
}

#[derive(Deserialize)]
pub struct RawQuery {
    /// Save the file rather than show it.
    download: Option<String>,
}

/// `attachment` with the file's name, for `?download=1` (RFC 6266; the
/// UTF-8 form for names that aren't plain ASCII).
fn attachment(path: &str) -> HeaderValue {
    let name = path.rsplit('/').next().unwrap_or("download");
    let ascii: String = name.chars().map(|c| if c.is_ascii_graphic() && c != '"' && c != '\\' { c } else { '_' }).collect();
    let encoded: String = name
        .bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect();
    HeaderValue::from_str(&format!("attachment; filename=\"{ascii}\"; filename*=UTF-8''{encoded}")).expect("ASCII")
}

pub async fn raw(
    State(state): State<AppState>,
    Viewer(viewer): Viewer,
    Path((owner, name, rest)): Path<(String, String, String)>,
    Query(q): Query<RawQuery>,
    headers: HeaderMap,
) -> AppResult {
    let download = q.download.is_some_and(|d| d != "0");
    let (repo, _) = load(&state, viewer.as_ref(), &owner, &name).await?;
    let (r, blob) = with_git(&state, &repo, move |g| {
        let r = g.resolve_rev_path(&rest).ok_or(AppError::NotFound)?;
        let blob = g.blob(r.commit, &r.path)?.ok_or(AppError::NotFound)?;
        Ok((r, blob))
    })
    .await?;

    // Git LFS: the file itself rather than its pointer, if it's stored.
    if let Some((oid, size)) = keel_core::parse_pointer(&blob.data)
        && state.core.lfs_object(repo.id, &oid).await?.is_some()
    {
        let content_type = match extension(&r.path).as_str() {
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "pdf" => "application/pdf",
            _ => "application/octet-stream",
        };
        let mut res = crate::lfs::serve_object(&state, &oid, size, content_type).await;
        let h = res.headers_mut();
        h.insert(header::CACHE_CONTROL, HeaderValue::from_static(if repo.is_private { "private, no-cache" } else { "public, no-cache" }));
        h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("sandbox; default-src 'none'"));
        if download {
            h.insert(header::CONTENT_DISPOSITION, attachment(&r.path));
        }
        return Ok(res);
    }
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
        ([(header::CONTENT_TYPE, raw_content_type(&r.path, blob.binary))], blob.data).into_response()
    };
    let h = res.headers_mut();
    h.insert(header::ETAG, HeaderValue::from_str(&etag).expect("hex"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(RAW_CSP));
    if download {
        h.insert(header::CONTENT_DISPOSITION, attachment(&r.path));
    }
    Ok(res)
}

/// The content type to serve a raw file as.
pub(crate) fn raw_content_type(path: &str, binary: bool) -> &'static str {
    match extension(path).as_str() {
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
        _ if !binary => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

/// For raw files. Sandboxing stops scripts in e.g. SVG files from running
/// as our origin.
pub(crate) const RAW_CSP: &str = "sandbox; default-src 'none'; img-src 'self'; style-src 'unsafe-inline'";

// ---------------------------------------------------------------------------
// History
// ---------------------------------------------------------------------------

#[derive(Template)]
#[template(path = "repo/commits.html")]
struct CommitsPage {
    /// Filtered to one author: their name, and the unfiltered history.
    author_filter: Option<(String, String)>,
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
    /// Only commits by the author with this address.
    author: Option<String>,
}

pub async fn commits_default(
    State(state): State<AppState>,
    Viewer(viewer): Viewer,
    Path((owner, name)): Path<(String, String)>,
    axum::extract::RawQuery(query): axum::extract::RawQuery,
) -> AppResult {
    let (repo, access) = load(&state, viewer.as_ref(), &owner, &name).await?;
    let branch = with_git(&state, &repo, |g| Ok(g.default_branch()?)).await?;
    // Filters like `?author=` carry over.
    let query = query.map(|q| format!("?{q}")).unwrap_or_default();
    match branch {
        Some(b) => Ok(Redirect::to(&format!("/{}/{}/commits/{}{query}", repo.owner, repo.name, encode_path(&b))).into_response()),
        None => empty_page(&state, &repo, access, viewer, "commits").await,
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
    let author = q.author.map(|a| a.trim().to_owned()).filter(|a| !a.is_empty() && a.len() <= 254);
    let by = author.clone();
    let (r, default_branch, mut list, refs) = with_git(&state, &repo, move |g| {
        let r = g.resolve_rev_path(&rest).ok_or(AppError::NotFound)?;
        // One extra to know whether there is an older page.
        let (skip, limit) = ((page_no - 1) * COMMITS_PER_PAGE, COMMITS_PER_PAGE + 1);
        let list = match &by {
            Some(email) => g.log_by(r.commit, &r.path, email, skip, limit)?,
            None => g.log(r.commit, &r.path, skip, limit)?,
        };
        Ok((r, g.default_branch()?.unwrap_or_default(), list, RefNames::load(g, &link_base, "commits")?))
    })
    .await?;

    let has_older = list.len() > COMMITS_PER_PAGE;
    list.truncate(COMMITS_PER_PAGE);
    let here = format!("{base}/commits/{}/{}", encode_path(&r.rev), encode_path(&r.path));
    let here = here.trim_end_matches('/').to_owned();
    let mut commits: Vec<CommitView> = list.iter().map(|c| CommitView::new(&base, c)).collect();
    crate::statuses::attach(&state, &repo, &mut commits.iter_mut().collect::<Vec<_>>()).await?;
    crate::signatures::attach(&state, &repo, &mut commits.iter_mut().collect::<Vec<_>>()).await?;
    link_authors(&state, &here, &mut commits.iter_mut().collect::<Vec<_>>()).await?;
    // Who the list is filtered to: their name on these commits, or the address.
    let author_filter = author.as_ref().map(|email| {
        let name = commits.first().map_or_else(|| email.clone(), |c| c.author.clone());
        (name, here.clone())
    });
    let page_query = match &author {
        Some(a) => format!("&author={}", form_urlencoded::byte_serialize(a.as_bytes()).collect::<String>()),
        None => String::new(),
    };
    html(CommitsPage {
        author_filter,
        repo: RepoView::new(&state, &repo, access, viewer.as_ref(), "commits", &default_branch).await?,
        page: Page::new(&state, viewer, format!("Commits · {}/{}", repo.owner, repo.name)),
        commits,
        newer_url: (page_no > 1).then(|| format!("{here}?page={}{page_query}", page_no - 1)),
        older_url: has_older.then(|| format!("{here}?page={}{page_query}", page_no + 1)),
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
    /// Row id, e.g. `file-2-n14` (new line 14) or `file-2-o9` (old line 9).
    pub anchor: String,
    /// Where a comment on this line attaches: the side (`new` for added
    /// and context lines, `old` for removed ones) and line number there.
    pub side: &'static str,
    pub line: u32,
    /// Inline review comments shown below the line (pull requests only).
    pub threads: Vec<crate::pulls::ThreadView>,
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
    /// Blob ids of the two sides.
    pub old_blob: Option<String>,
    pub new_blob: Option<String>,
}

/// View models for a diff; "View file" links point at `rev`.
pub(crate) fn file_views(base: &str, rev: &str, diff: &keel_git::CommitDiff) -> Vec<FileView> {
    diff.files
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
                .then(|| format!("{base}/blob/{rev}/{}", encode_path(f.path()))),
            old_blob: f.old_id.map(|id| id.to_string()),
            new_blob: f.new_id.map(|id| id.to_string()),
            hunks: f
                .hunks
                .iter()
                .map(|h| HunkView {
                    header: h.header.clone(),
                    lines: h.lines.iter().map(|l| line_view(i, l)).collect(),
                })
                .collect(),
        })
        .collect()
}

fn line_view(file: usize, l: &keel_git::DiffLine) -> LineView {
    let (side, line) = match (l.kind, l.old_no, l.new_no) {
        (DiffLineKind::Removed, Some(old), _) => ("old", old),
        (DiffLineKind::NoNewline, _, _) => ("", 0),
        (_, _, Some(new)) => ("new", new),
        _ => ("", 0),
    };
    LineView {
        class: match l.kind {
            DiffLineKind::Added => "add",
            DiffLineKind::Removed => "del",
            DiffLineKind::Context => "ctx",
            DiffLineKind::NoNewline => "meta",
        },
        old: l.old_no.map(|n| n.to_string()).unwrap_or_default(),
        new: l.new_no.map(|n| n.to_string()).unwrap_or_default(),
        content: l.content.clone(),
        anchor: if side.is_empty() { String::new() } else { format!("file-{file}-{}{line}", &side[..1]) },
        side,
        line,
        threads: Vec::new(),
    }
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
    inline: Option<crate::pulls::InlineCtx>,
    checks: Vec<crate::statuses::CheckView>,
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

    let files = file_views(&base, &detail.commit.id.to_string(), &diff);
    let checks = state
        .core
        .statuses(repo.id, &detail.commit.id.to_string())
        .await?
        .iter()
        .map(|s| crate::statuses::CheckView::new(s, false))
        .collect();

    let mut commit = CommitView::new(&base, &detail.commit);
    crate::signatures::attach(&state, &repo, &mut [&mut commit]).await?;
    link_authors(&state, &format!("{base}/commits/{}", detail.commit.id), &mut [&mut commit]).await?;
    html(CommitPage {
        checks,
        repo: RepoView::new(&state, &repo, access, viewer.as_ref(), "commits", &default_branch).await?,
        page: Page::new(&state, viewer, format!("{} · {}/{}", detail.commit.summary, repo.owner, repo.name)),
        commit,
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
        inline: None,
    })
}

// ---------------------------------------------------------------------------
// Branches and tags
// ---------------------------------------------------------------------------

pub(crate) struct RefView {
    pub name: String,
    /// The name, URL-encoded.
    pub name_url: String,
    pub url: String,
    pub commit: CommitView,
    pub is_default: bool,
    /// A branch protection rule applies.
    pub protected: bool,
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
    let rules = state.core.branch_rules(repo.id).await?;
    let view = |r: &keel_git::RefInfo, branch: bool| RefView {
        url: format!("{base}/tree/{}", encode_path(&r.name)),
        name_url: encode_path(&r.name),
        commit: CommitView::new(&base, &r.commit),
        is_default: r.name == default_branch,
        protected: branch && keel_core::rule_for(&rules, &r.name).is_some(),
        name: r.name.clone(),
    };
    let mut branches: Vec<RefView> = branches.iter().map(|b| view(b, true)).collect();
    let mut tags: Vec<RefView> = tags.iter().map(|t| view(t, false)).collect();
    crate::statuses::attach(&state, &repo, &mut branches.iter_mut().map(|b| &mut b.commit).collect::<Vec<_>>()).await?;
    let mut commits: Vec<&mut CommitView> = branches.iter_mut().chain(tags.iter_mut()).map(|r| &mut r.commit).collect();
    link_authors(&state, &format!("{base}/commits"), &mut commits).await?;
    html(RefsPage {
        repo: RepoView::new(&state, &repo, access, viewer.as_ref(), "branches", &default_branch).await?,
        page: Page::new(&state, viewer, format!("Branches · {}/{}", repo.owner, repo.name)),
        branches,
        tags,
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
    errors: SettingsErrors,
    collaborators: Vec<(i64, String, &'static str)>,
    owned_by_org: bool,
    /// Accounts the repository can be moved to: you and your organizations.
    transfer_targets: Vec<String>,
    mirror: Option<keel_core::Mirror>,
    intervals: &'static [(i64, &'static str)],
    /// Git LFS files and their total size, if any.
    lfs: Option<(i64, String)>,
    lfs_locks: Vec<(keel_core::LfsLock, String)>,
    push_mirrors: Vec<PushMirrorView>,
}

pub(crate) struct PushMirrorView {
    pub id: i64,
    pub url: String,
    /// "pushed 3 minutes ago", "waiting to push", ….
    pub status: String,
    pub error: Option<String>,
}

/// An error message for one of the settings page's forms.
#[derive(Default)]
pub(crate) struct SettingsErrors {
    pub general: Option<String>,
    pub rename: Option<String>,
    pub collaborator: Option<String>,
    pub transfer: Option<String>,
    pub delete: Option<String>,
    pub mirror: Option<String>,
    pub push_mirror: Option<String>,
}

/// The repository, if `user` may administer it.
async fn writable(state: &AppState, user: &User, owner: &str, name: &str) -> AppResult<Repo> {
    load_for_admin(state, user, owner, name).await
}

/// The repository, if `user` may change its settings, which archived
/// repositories still allow (to unarchive them, say).
pub(crate) async fn load_for_admin(state: &AppState, user: &User, owner: &str, name: &str) -> AppResult<Repo> {
    let (repo, _) = load(state, Some(user), owner, name).await?;
    if state.core.granted_access(&repo, Some(user)).await? != Access::Admin {
        return Err(AppError::Forbidden);
    }
    Ok(repo)
}

async fn settings_page(state: &AppState, user: User, repo: &Repo, errors: SettingsErrors) -> AppResult {
    let default_branch = with_git(state, repo, |g| Ok(g.default_branch()?.unwrap_or_default())).await?;
    let collaborators = state
        .core
        .collaborators(repo.id)
        .await?
        .into_iter()
        .map(|(u, a)| (u.id, u.username, a.role_name()))
        .collect();
    let owned_by_org = state.core.user_by_id(repo.owner_id).await?.is_some_and(|o| o.is_org());
    let mut transfer_targets = vec![user.username.clone()];
    transfer_targets.extend(state.core.orgs_of(user.id).await?.into_iter().map(|(o, _)| o.username));
    transfer_targets.retain(|t| *t != repo.owner);
    html(SettingsPage {
        repo: RepoView::new(state, repo, Access::Admin, Some(&user), "settings", &default_branch).await?,
        page: Page::new(state, Some(user), format!("Settings · {}/{}", repo.owner, repo.name)),
        errors,
        collaborators,
        owned_by_org,
        transfer_targets,
        mirror: state.core.mirror(repo.id).await?,
        intervals: keel_core::MIRROR_INTERVALS,
        lfs: match state.core.lfs_usage(repo.id).await? {
            (0, _) => None,
            (n, bytes) => Some((n, crate::render::human_size(bytes as usize))),
        },
        lfs_locks: state
            .core
            .lfs_locks(repo.id, None, None, 0, 200)
            .await?
            .into_iter()
            .map(|l| {
                let when = relative_time(l.created_at);
                (l, when)
            })
            .collect(),
        push_mirrors: state
            .core
            .push_mirrors(repo.id)
            .await?
            .into_iter()
            .map(|m| PushMirrorView {
                status: match (m.pending, m.pushed_at) {
                    (true, _) if m.attempts > 0 => format!("retrying {}", relative_time_future(m.next_attempt_at)),
                    (true, _) => "pushing soon".into(),
                    (false, Some(t)) => format!("pushed {}", relative_time(t)),
                    (false, None) => "not pushed yet".into(),
                },
                id: m.id,
                url: m.url,
                error: m.last_error,
            })
            .collect(),
    })
}

#[derive(Deserialize)]
pub struct CollaboratorForm {
    username: String,
    role: String,
}

pub async fn set_collaborator(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name)): Path<(String, String)>,
    Form(f): Form<CollaboratorForm>,
) -> AppResult {
    let repo = writable(&state, &user, &owner, &name).await?;
    let role = Access::parse_role(&f.role).ok_or_else(|| AppError::BadRequest("Unknown role".into()))?;
    match state.core.set_collaborator(&repo, &f.username, role).await {
        Ok(()) => Ok(Redirect::to(&format!("/{}/{}/settings#collaborators", repo.owner, repo.name)).into_response()),
        Err(keel_core::Error::Invalid(msg)) => {
            settings_page(&state, user, &repo, SettingsErrors { collaborator: Some(msg), ..Default::default() }).await
        }
        Err(e) => Err(e.into()),
    }
}

pub async fn remove_collaborator(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, user_id)): Path<(String, String, i64)>,
) -> AppResult {
    let repo = writable(&state, &user, &owner, &name).await?;
    state.core.remove_collaborator(repo.id, user_id).await?;
    Ok(Redirect::to(&format!("/{}/{}/settings#collaborators", repo.owner, repo.name)).into_response())
}

pub async fn settings(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name)): Path<(String, String)>,
) -> AppResult {
    let repo = writable(&state, &user, &owner, &name).await?;
    settings_page(&state, user, &repo, SettingsErrors::default()).await
}

#[derive(Deserialize)]
pub struct SettingsForm {
    #[serde(default)]
    description: String,
    #[serde(default)]
    topics: String,
    private: Option<String>,
    template: Option<String>,
}

pub async fn update_settings(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name)): Path<(String, String)>,
    Form(f): Form<SettingsForm>,
) -> AppResult {
    let repo = writable(&state, &user, &owner, &name).await?;
    let updated = async {
        let topics = keel_core::parse_topics(&f.topics)?;
        state.core.update_repo(repo.id, &f.description, f.private.is_some()).await?;
        state.core.set_template(repo.id, f.template.is_some()).await?;
        state.core.set_topics(repo.id, &topics).await
    };
    match updated.await {
        Ok(()) => Ok(Redirect::to(&format!("/{}/{}/settings", repo.owner, repo.name)).into_response()),
        Err(keel_core::Error::Invalid(msg)) => {
            settings_page(&state, user, &repo, SettingsErrors { general: Some(msg), ..Default::default() }).await
        }
        Err(e) => Err(e.into()),
    }
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
        return settings_page(&state, user, &repo, SettingsErrors { delete: Some(msg), ..Default::default() }).await;
    }
    state.core.delete_repo(&repo).await?;
    Ok(Redirect::to(&format!("/{}", repo.owner)).into_response())
}

#[derive(Deserialize)]
pub struct MirrorForm {
    url: String,
    #[serde(default)]
    username: String,
    /// Empty keeps the stored one.
    #[serde(default)]
    password: String,
    interval: i64,
}

pub async fn update_mirror(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name)): Path<(String, String)>,
    Form(f): Form<MirrorForm>,
) -> AppResult {
    let repo = writable(&state, &user, &owner, &name).await?;
    let password = (!f.password.is_empty()).then_some(f.password.as_str());
    match state.core.update_mirror(repo.id, &f.url, &f.username, password, f.interval).await {
        Ok(()) => Ok(Redirect::to(&format!("/{}/{}/settings#mirror", repo.owner, repo.name)).into_response()),
        Err(keel_core::Error::Invalid(msg)) => {
            settings_page(&state, user, &repo, SettingsErrors { mirror: Some(msg), ..Default::default() }).await
        }
        Err(e) => Err(e.into()),
    }
}

pub async fn stop_mirroring(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name)): Path<(String, String)>,
) -> AppResult {
    let repo = writable(&state, &user, &owner, &name).await?;
    state.core.stop_mirroring(repo.id).await?;
    Ok(Redirect::to(&format!("/{}/{}/settings", repo.owner, repo.name)).into_response())
}

/// "Sync now", for anyone with write access.
pub async fn sync_mirror(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name)): Path<(String, String)>,
) -> AppResult {
    let (repo, access) = load(&state, Some(&user), &owner, &name).await?;
    if access < Access::Write {
        return Err(AppError::Forbidden);
    }
    state.core.request_mirror_sync(repo.id).await?;
    Ok(Redirect::to(&format!("/{}/{}", repo.owner, repo.name)).into_response())
}

/// "in 5 minutes", for retries.
fn relative_time_future(ts: i64) -> String {
    match ts - crate::now() {
        d if d < 60 => "in a moment".into(),
        d if d < 3600 => format!("in {} minutes", d / 60),
        d => format!("in {} hours", d / 3600),
    }
}

#[derive(Deserialize)]
pub struct PushMirrorForm {
    url: String,
    #[serde(default)]
    username: String,
    #[serde(default)]
    password: String,
}

pub async fn add_push_mirror(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name)): Path<(String, String)>,
    Form(f): Form<PushMirrorForm>,
) -> AppResult {
    let repo = writable(&state, &user, &owner, &name).await?;
    match state.core.add_push_mirror(repo.id, &f.url, &f.username, &f.password).await {
        Ok(_) => Ok(Redirect::to(&format!("/{}/{}/settings#push-mirrors", repo.owner, repo.name)).into_response()),
        Err(keel_core::Error::Invalid(msg)) => {
            settings_page(&state, user, &repo, SettingsErrors { push_mirror: Some(msg), ..Default::default() }).await
        }
        Err(e) => Err(e.into()),
    }
}

pub async fn delete_push_mirror(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> AppResult {
    let repo = writable(&state, &user, &owner, &name).await?;
    state.core.delete_push_mirror(repo.id, id).await?;
    Ok(Redirect::to(&format!("/{}/{}/settings#push-mirrors", repo.owner, repo.name)).into_response())
}

pub async fn push_now(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> AppResult {
    let repo = writable(&state, &user, &owner, &name).await?;
    state.core.request_push(repo.id, id).await?;
    Ok(Redirect::to(&format!("/{}/{}/settings#push-mirrors", repo.owner, repo.name)).into_response())
}

/// Breaks a Git LFS lock (repository admins).
pub async fn unlock_file(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name, id)): Path<(String, String, i64)>,
) -> AppResult {
    let repo = writable(&state, &user, &owner, &name).await?;
    state.core.delete_lfs_lock(repo.id, id).await?;
    Ok(Redirect::to(&format!("/{}/{}/settings#lfs", repo.owner, repo.name)).into_response())
}

#[derive(Deserialize)]
pub struct RenameForm {
    name: String,
}

pub async fn rename(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name)): Path<(String, String)>,
    Form(f): Form<RenameForm>,
) -> AppResult {
    let repo = writable(&state, &user, &owner, &name).await?;
    match state.core.rename_repo(&repo, &f.name).await {
        Ok(r) => Ok(Redirect::to(&format!("/{}/{}/settings", r.owner, r.name)).into_response()),
        Err(keel_core::Error::Invalid(msg)) => {
            settings_page(&state, user, &repo, SettingsErrors { rename: Some(msg), ..Default::default() }).await
        }
        Err(e) => Err(e.into()),
    }
}

#[derive(Deserialize)]
pub struct TransferForm {
    new_owner: String,
    confirm: String,
}

#[derive(Deserialize)]
pub struct ArchiveForm {
    /// `on` or `off`.
    to: String,
}

pub async fn archive(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name)): Path<(String, String)>,
    Form(f): Form<ArchiveForm>,
) -> AppResult {
    let repo = writable(&state, &user, &owner, &name).await?;
    state.core.set_archived(repo.id, f.to == "on", Some(user.id)).await?;
    Ok(Redirect::to(&format!("/{}/{}", repo.owner, repo.name)).into_response())
}

pub async fn transfer(
    State(state): State<AppState>,
    SignedIn(user): SignedIn,
    Path((owner, name)): Path<(String, String)>,
    Form(f): Form<TransferForm>,
) -> AppResult {
    let repo = writable(&state, &user, &owner, &name).await?;
    let full = format!("{}/{}", repo.owner, repo.name);
    let fail = |msg: String| SettingsErrors { transfer: Some(msg), ..Default::default() };
    if f.confirm.trim() != full {
        return settings_page(&state, user, &repo, fail(format!("To transfer, type “{full}” exactly."))).await;
    }
    let Some(target) = state.core.user_by_name(&f.new_owner).await? else {
        return settings_page(&state, user, &repo, fail(format!("There's no account named “{}”.", f.new_owner))).await;
    };
    match state.core.transfer_repo(&repo, &user, &target).await {
        Ok(r) => Ok(Redirect::to(&format!("/{}/{}", r.owner, r.name)).into_response()),
        Err(keel_core::Error::Invalid(msg)) => settings_page(&state, user, &repo, fail(msg)).await,
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_paths() {
        assert_eq!(encode_path("src/main.rs"), "src/main.rs");
        assert_eq!(encode_path("a b/c#d?.txt"), "a%20b/c%23d%3F.txt");
        assert_eq!(encode_path("ünï"), "%C3%BCn%C3%AF");
        assert_eq!(encode_path("bob/engine:topic"), "bob/engine:topic");
    }

    #[test]
    fn breadcrumbs() {
        let c = crumbs("/ada/engine", "feature/x", "src/bin");
        assert_eq!(c.len(), 2);
        assert_eq!(c[1].url, "/ada/engine/tree/feature/x/src/bin");
    }
}
