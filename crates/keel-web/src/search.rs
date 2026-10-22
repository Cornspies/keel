//! Code search: keeping the index current, and the search pages.
//!
//! The index (see `keel-search`) holds each repository's default branch.
//! After a push only the files that changed since the indexed commit are
//! reindexed. A query asks the index for candidate files, then scans their
//! content to confirm matches and build snippets.

use std::collections::HashMap;
use std::sync::Arc;

use askama::Template;
use axum::extract::{Path, Query, State};
use keel_core::{Repo, RepoEvent};
use keel_git::{GitRepo, Oid};
use keel_search::{FileDoc, MIN_QUERY_CHARS, SearchIndex};
use serde::Deserialize;
use tokio::sync::broadcast::error::RecvError;

use crate::error::{AppResult, html};
use crate::render::escape;
use crate::repo::{RepoView, encode_path, load, with_git};
use crate::session::Viewer;
use crate::{AppError, AppState, Page};

/// Larger files (minified bundles, data dumps) aren't worth indexing.
const MAX_FILE_BYTES: usize = 1024 * 1024;
const MAX_CANDIDATES: usize = 400;
const MAX_RESULTS: usize = 50;
const LINES_PER_FILE: usize = 3;
const MAX_LINE_CHARS: usize = 240;

pub fn open_index(state_dir: &std::path::Path) -> Option<Arc<SearchIndex>> {
    match SearchIndex::open(&state_dir.join("search")) {
        Ok(index) => Some(Arc::new(index)),
        Err(e) => {
            tracing::error!("code search disabled: {e:#}");
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Indexing
// ---------------------------------------------------------------------------

/// Indexes all repositories that changed since they were last indexed,
/// then follows pushes and deletions.
pub fn start_indexer(state: AppState) {
    let Some(index) = state.search.clone() else { return };
    let mut events = state.core.subscribe();
    tokio::spawn(async move {
        // An empty index (e.g. deleted directory) means nothing is indexed,
        // whatever the database says.
        let rebuild = index.is_empty();
        match state.core.all_repo_ids().await {
            Ok(ids) => {
                for id in ids {
                    if let Err(e) = index_repo(&state, &index, id, rebuild).await {
                        tracing::warn!(repo = id, "indexing failed: {e:?}");
                    }
                }
            }
            Err(e) => tracing::error!("listing repositories for indexing: {e}"),
        }
        loop {
            let result = match events.recv().await {
                Ok(RepoEvent::Pushed { repo_id }) => index_repo(&state, &index, repo_id, false).await,
                Ok(RepoEvent::Deleted { repo_id }) => {
                    let index = index.clone();
                    tokio::task::spawn_blocking(move || index.delete_repo(repo_id as u64))
                        .await
                        .map_err(AppError::from)
                        .and_then(|r| r.map_err(AppError::Internal))
                }
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => return,
            };
            if let Err(e) = result {
                tracing::warn!("indexing failed: {e:?}");
            }
        }
    });
}

async fn index_repo(state: &AppState, index: &Arc<SearchIndex>, repo_id: i64, rebuild: bool) -> AppResult<()> {
    let Some(repo) = state.core.repo_by_id(repo_id).await? else {
        return Ok(());
    };
    let indexed = if rebuild { None } else { state.core.search_indexed_commit(repo_id).await? };
    let index = index.clone();
    let started = std::time::Instant::now();
    let done = with_git(state, &repo, move |g| {
        let Some(head) = g.default_branch()?.and_then(|b| g.resolve_rev(&b)) else {
            return Ok(None);
        };
        let old = indexed.as_deref().and_then(|c| Oid::from_str(c).ok());
        if old == Some(head) {
            return Ok(None);
        }
        // Incremental if the old commit still exists, otherwise everything.
        let (changed, mut deleted, full) = match old.and_then(|o| g.changed_files(o, head).ok()) {
            Some(c) => (c.changed, c.deleted, false),
            None => (g.files(head)?, Vec::new(), true),
        };
        if full {
            index.delete_repo(repo_id as u64)?;
        }
        let mut skipped = Vec::new();
        let docs = changed.into_iter().filter_map(|f| {
            let content = (g.blob_size(f.id).ok()? <= MAX_FILE_BYTES)
                .then(|| g.blob_by_id(f.id).ok().flatten())
                .flatten()
                .filter(|(_, binary)| !binary);
            match content {
                Some((data, _)) => Some(FileDoc {
                    path: f.path,
                    blob: f.id.to_string(),
                    content: String::from_utf8_lossy(&data).into_owned(),
                }),
                None => {
                    // Not searchable (any more): drop an older version.
                    skipped.push(f.path);
                    None
                }
            }
        });
        let added = index.update(repo_id as u64, &[], docs)?;
        deleted.append(&mut skipped);
        if !deleted.is_empty() {
            index.update(repo_id as u64, &deleted, [])?;
        }
        Ok(Some((head, added)))
    })
    .await?;
    if let Some((head, added)) = done {
        state.core.set_search_indexed_commit(repo_id, &head.to_string()).await?;
        tracing::info!(repo = repo_id, files = added, "indexed for search in {:?}", started.elapsed());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Searching
// ---------------------------------------------------------------------------

pub(crate) struct LineMatch {
    pub number: usize,
    /// Escaped, with matches in `<mark>`.
    pub html: String,
}

pub(crate) struct FileResult {
    pub repo: String,
    pub repo_url: String,
    pub path: String,
    pub url: String,
    pub lines: Vec<LineMatch>,
    pub more: usize,
}

/// Byte ranges where `needle` occurs in `line`, ignoring case where that
/// keeps byte offsets intact (always for ASCII).
fn find_all(line: &str, needle: &str) -> Vec<(usize, usize)> {
    let (hay, pat) = {
        let (h, p) = (line.to_lowercase(), needle.to_lowercase());
        if h.len() == line.len() && p.len() == needle.len() { (h, p) } else { (line.to_owned(), needle.to_owned()) }
    };
    hay.match_indices(&pat).map(|(i, m)| (i, i + m.len())).collect()
}

/// Escapes `line` and marks the ranges, shortening very long lines around
/// the first match.
fn highlight(line: &str, ranges: &[(usize, usize)]) -> String {
    let (mut start, mut end) = (0, line.len());
    if line.chars().count() > MAX_LINE_CHARS {
        let first = ranges.first().map_or(0, |r| r.0);
        start = line.floor_char_boundary(first.saturating_sub(MAX_LINE_CHARS / 3));
        end = line.floor_char_boundary((start + MAX_LINE_CHARS).min(line.len()));
    }
    let mut out = String::new();
    if start > 0 {
        out.push('…');
    }
    let mut pos = start;
    for &(a, b) in ranges.iter().filter(|(a, b)| *a >= start && *b <= end) {
        out.push_str(&escape(&line[pos..a]));
        out.push_str("<mark>");
        out.push_str(&escape(&line[a..b]));
        out.push_str("</mark>");
        pos = b;
    }
    out.push_str(&escape(&line[pos..end]));
    if end < line.len() {
        out.push('…');
    }
    out
}

/// Runs a query over `repos` and confirms the candidates.
async fn run_search(state: &AppState, query: &str, repos: Vec<Repo>) -> AppResult<(Vec<FileResult>, bool)> {
    let Some(index) = state.search.clone() else {
        return Ok((Vec::new(), false));
    };
    let ids: Vec<u64> = repos.iter().map(|r| r.id as u64).collect();
    let q = query.to_owned();
    let candidates = tokio::task::spawn_blocking(move || index.candidates(&q, &ids, MAX_CANDIDATES))
        .await?
        .map_err(AppError::Internal)?;

    let by_id: HashMap<i64, Repo> = repos.into_iter().map(|r| (r.id, r)).collect();
    let mut grouped: HashMap<i64, Vec<(String, String)>> = HashMap::new();
    for c in candidates {
        grouped.entry(c.repo_id as i64).or_default().push((c.path, c.blob));
    }
    let mut results = Vec::new();
    let mut truncated = false;
    for (repo_id, files) in grouped {
        let Some(repo) = by_id.get(&repo_id).cloned() else { continue };
        let q = query.to_owned();
        let base = format!("/{}/{}", repo.owner, repo.name);
        let full_name = format!("{}/{}", repo.owner, repo.name);
        let found = with_git(state, &repo, move |g: &GitRepo| {
            let branch = encode_path(&g.default_branch()?.unwrap_or_default());
            let mut out = Vec::new();
            for (path, blob) in files {
                let Some((data, _)) = Oid::from_str(&blob).ok().and_then(|id| g.blob_by_id(id).ok().flatten()) else {
                    continue;
                };
                let text = String::from_utf8_lossy(&data);
                let mut lines = Vec::new();
                let mut total = 0usize;
                for (i, line) in text.lines().enumerate() {
                    let ranges = find_all(line, &q);
                    if ranges.is_empty() {
                        continue;
                    }
                    total += 1;
                    if lines.len() < LINES_PER_FILE {
                        lines.push(LineMatch { number: i + 1, html: highlight(line, &ranges) });
                    }
                }
                let path_match = !find_all(&path, &q).is_empty();
                if total == 0 && !path_match {
                    continue; // all trigrams present, but not the query
                }
                let anchor = lines.first().map(|l| format!("#L{}", l.number)).unwrap_or_default();
                out.push(FileResult {
                    repo: full_name.clone(),
                    repo_url: base.clone(),
                    url: format!("{base}/blob/{branch}/{}{anchor}", encode_path(&path)),
                    more: total.saturating_sub(lines.len()),
                    path,
                    lines,
                });
            }
            Ok(out)
        })
        .await?;
        results.extend(found);
    }
    results.sort_by(|a, b| (&a.repo, &a.path).cmp(&(&b.repo, &b.path)));
    if results.len() > MAX_RESULTS {
        results.truncate(MAX_RESULTS);
        truncated = true;
    }
    Ok((results, truncated))
}

#[derive(Deserialize)]
pub struct SearchQuery {
    #[serde(default)]
    q: String,
}

fn query_problem(q: &str) -> Option<String> {
    let n = q.chars().count();
    (n > 0 && n < MIN_QUERY_CHARS).then(|| format!("Search for at least {MIN_QUERY_CHARS} characters."))
}

#[derive(Template)]
#[template(path = "search.html")]
struct SearchPage {
    page: Page,
    q: String,
    problem: Option<String>,
    results: Vec<FileResult>,
    truncated: bool,
    searched: bool,
    enabled: bool,
}

/// `/search`: all repositories the viewer can read.
pub async fn global(State(state): State<AppState>, Viewer(viewer): Viewer, Query(sq): Query<SearchQuery>) -> AppResult {
    let q = sq.q.trim().to_owned();
    let problem = query_problem(&q);
    let searched = !q.is_empty() && problem.is_none();
    let (results, truncated) = if searched {
        let mut repos = Vec::new();
        for id in state.core.visible_repo_ids(viewer.as_ref()).await? {
            repos.extend(state.core.repo_by_id(id).await?);
        }
        run_search(&state, &q, repos).await?
    } else {
        (Vec::new(), false)
    };
    let title = if q.is_empty() { "Search".to_owned() } else { format!("{q} · Search") };
    html(SearchPage {
        enabled: state.search.is_some(),
        page: Page::new(&state, viewer, title),
        q,
        problem,
        results,
        truncated,
        searched,
    })
}

#[derive(Template)]
#[template(path = "repo/search.html")]
struct RepoSearchPage {
    page: Page,
    repo: RepoView,
    q: String,
    problem: Option<String>,
    results: Vec<FileResult>,
    truncated: bool,
    searched: bool,
    enabled: bool,
}

/// `/<owner>/<repo>/search`: one repository.
pub async fn in_repo(
    State(state): State<AppState>,
    Viewer(viewer): Viewer,
    Path((owner, name)): Path<(String, String)>,
    Query(sq): Query<SearchQuery>,
) -> AppResult {
    let (repo, access) = load(&state, viewer.as_ref(), &owner, &name).await?;
    let q = sq.q.trim().to_owned();
    let problem = query_problem(&q);
    let searched = !q.is_empty() && problem.is_none();
    let (results, truncated) = if searched {
        run_search(&state, &q, vec![repo.clone()]).await?
    } else {
        (Vec::new(), false)
    };
    html(RepoSearchPage {
        enabled: state.search.is_some(),
        repo: RepoView::new(&state, &repo, access, viewer.as_ref(), "code", "").await?,
        page: Page::new(&state, viewer, format!("Search · {}/{}", repo.owner, repo.name)),
        q,
        problem,
        results,
        truncated,
        searched,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_and_highlighting() {
        assert_eq!(find_all("Foo foo FOO", "foo"), [(0, 3), (4, 7), (8, 11)]);
        assert_eq!(highlight("let <a> = 1;", &find_all("let <a> = 1;", "<a>")), "let <mark>&lt;a&gt;</mark> = 1;");
        let long = format!("{}needle{}", "x".repeat(500), "y".repeat(500));
        let h = highlight(&long, &find_all(&long, "needle"));
        assert!(h.starts_with('…') && h.ends_with('…') && h.contains("<mark>needle</mark>"));
        assert!(h.chars().count() < 300);
        // Case changes that alter byte lengths fall back to exact matching.
        assert_eq!(find_all("İstanbul", "stan"), [(2, 6)]);
    }
}
