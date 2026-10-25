//! Code search: keeping the index current, and the search pages.
//!
//! The index (see `keel-search`) holds each repository's default branch.
//! After a push only the files that changed since the indexed commit are
//! reindexed. A query asks the index for candidate files, then scans their
//! content to confirm matches and build snippets.

use std::collections::{BTreeSet, HashMap};
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
/// A branch differing from the indexed commit in more files than this is
/// searched by reading all its files.
const MAX_DIFF_FILES: usize = 5_000;
/// How much file content a search without the index reads at most.
const MAX_SCAN_BYTES: usize = 256 * 1024 * 1024;
const MAX_LINE_CHARS: usize = 240;
/// Longest query: each distinct trigram is a clause of the index query,
/// and anyone can search public repositories.
const MAX_QUERY_CHARS: usize = 256;

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

/// What to look for.
#[derive(Clone, Debug)]
pub(crate) enum Matcher {
    /// A substring, ignoring case.
    Text(String),
    /// A regular expression, matched line by line.
    Regex(regex::Regex),
}

impl Matcher {
    /// A regular expression if `regex` is set or the query is written
    /// `/like this/`, otherwise text. Errors explain unusable queries.
    fn parse(q: &str, regex: bool) -> Result<Self, String> {
        if q.chars().count() > MAX_QUERY_CHARS {
            return Err(format!("Searches are at most {MAX_QUERY_CHARS} characters long."));
        }
        let slashed = q.len() > 2 && q.starts_with('/') && q.ends_with('/');
        if !(regex || slashed) {
            let n = q.chars().count();
            if n < MIN_QUERY_CHARS {
                return Err(format!("Search for at least {MIN_QUERY_CHARS} characters."));
            }
            return Ok(Self::Text(q.to_owned()));
        }
        let pattern = if slashed && !regex { &q[1..q.len() - 1] } else { q };
        regex::RegexBuilder::new(pattern)
            .size_limit(1 << 20)
            .dfa_size_limit(1 << 22)
            .build()
            .map(Self::Regex)
            .map_err(|e| match e {
                regex::Error::CompiledTooBig(_) => "That regular expression is too complex.".to_owned(),
                e => format!("That isn't a valid regular expression: {}", e.to_string().lines().last().unwrap_or_default().trim()),
            })
    }

    /// Byte ranges of the (non-empty) matches in `line`.
    fn find(&self, line: &str) -> Vec<(usize, usize)> {
        match self {
            Self::Text(t) => find_all(line, t),
            Self::Regex(r) => r.find_iter(line).filter(|m| !m.is_empty()).map(|m| (m.start(), m.end())).collect(),
        }
    }

    /// Trigrams every matching file contains; empty if the index can't
    /// help.
    fn grams(&self) -> BTreeSet<String> {
        match self {
            Self::Text(t) => keel_search::trigrams(t),
            Self::Regex(r) => keel_search::regex_trigrams(r.as_str()).unwrap_or_default(),
        }
    }
}

/// Where results link to.
struct Target {
    repo: String,
    base: String,
    /// The branch or commit searched, URL-encoded.
    rev: String,
}

/// Looks for `matcher` in `files` (path and blob), up to `limit` matching
/// files and `budget` bytes read. Returns whether it stopped early.
fn scan(
    g: &GitRepo,
    matcher: &Matcher,
    files: impl IntoIterator<Item = (String, Oid)>,
    t: &Target,
    limit: usize,
    budget: &mut usize,
    out: &mut Vec<FileResult>,
) -> bool {
    for (path, blob) in files {
        if out.len() >= limit || *budget == 0 {
            return true;
        }
        let Ok(size) = g.blob_size(blob) else { continue };
        if size > MAX_FILE_BYTES {
            continue;
        }
        let Some((data, binary)) = g.blob_by_id(blob).ok().flatten() else { continue };
        *budget = budget.saturating_sub(size);
        if binary {
            continue;
        }
        let text = String::from_utf8_lossy(&data);
        let mut lines = Vec::new();
        let mut total = 0usize;
        for (i, line) in text.lines().enumerate() {
            let ranges = matcher.find(line);
            if ranges.is_empty() {
                continue;
            }
            total += 1;
            if lines.len() < LINES_PER_FILE {
                lines.push(LineMatch { number: i + 1, html: highlight(line, &ranges) });
            }
        }
        if total == 0 && matcher.find(&path).is_empty() {
            continue; // all trigrams present, but no match
        }
        let anchor = lines.first().map(|l| format!("#L{}", l.number)).unwrap_or_default();
        out.push(FileResult {
            repo: t.repo.clone(),
            repo_url: t.base.clone(),
            url: format!("{}/blob/{}/{}{anchor}", t.base, t.rev, encode_path(&path)),
            more: total.saturating_sub(lines.len()),
            path,
            lines,
        });
    }
    false
}

/// Runs a query over the default branches of `repos`, using the index.
async fn run_search(state: &AppState, matcher: &Matcher, repos: Vec<Repo>) -> AppResult<(Vec<FileResult>, bool)> {
    let Some(index) = state.search.clone() else {
        return Ok((Vec::new(), false));
    };
    let ids: Vec<u64> = repos.iter().map(|r| r.id as u64).collect();
    let grams = matcher.grams();
    let candidates = tokio::task::spawn_blocking(move || index.candidates_with(&grams, &ids, MAX_CANDIDATES))
        .await?
        .map_err(AppError::Internal)?;

    let by_id: HashMap<i64, Repo> = repos.into_iter().map(|r| (r.id, r)).collect();
    let mut grouped: HashMap<i64, Vec<(String, Oid)>> = HashMap::new();
    for c in candidates {
        if let Ok(blob) = Oid::from_str(&c.blob) {
            grouped.entry(c.repo_id as i64).or_default().push((c.path, blob));
        }
    }
    let mut results = Vec::new();
    for (repo_id, files) in grouped {
        let Some(repo) = by_id.get(&repo_id).cloned() else { continue };
        let m = matcher.clone();
        let (full_name, base) = (format!("{}/{}", repo.owner, repo.name), format!("/{}/{}", repo.owner, repo.name));
        let found = with_git(state, &repo, move |g: &GitRepo| {
            let rev = encode_path(&g.default_branch()?.unwrap_or_default());
            let t = Target { repo: full_name, base, rev };
            let (mut out, mut budget) = (Vec::new(), usize::MAX);
            scan(g, &m, files, &t, usize::MAX, &mut budget, &mut out);
            Ok(out)
        })
        .await?;
        results.extend(found);
    }
    results.sort_by(|a, b| (&a.repo, &a.path).cmp(&(&b.repo, &b.path)));
    let truncated = results.len() > MAX_RESULTS;
    results.truncate(MAX_RESULTS);
    Ok((results, truncated))
}

/// Searches one revision of a repository: files unchanged since the
/// indexed commit come from the index, the rest (all of them, for a
/// regular expression without literal text or without an index) is read.
async fn search_rev(state: &AppState, matcher: &Matcher, repo: &Repo, rev: String) -> AppResult<Option<(Vec<FileResult>, bool)>> {
    let indexed = match &state.search {
        Some(_) => state.core.search_indexed_commit(repo.id).await?.and_then(|c| Oid::from_str(&c).ok()),
        None => None,
    };
    let index = state.search.clone();
    let m = matcher.clone();
    let (owner, name, repo_id) = (repo.owner.clone(), repo.name.clone(), repo.id as u64);
    with_git(state, repo, move |g: &GitRepo| {
        let Some(tip) = g.resolve_rev(&rev) else { return Ok(None) };
        let t = Target { repo: format!("{owner}/{name}"), base: format!("/{owner}/{name}"), rev: encode_path(&rev) };
        let grams = m.grams();
        // Files to read, and whether the index covers the rest.
        let plan = match (&index, indexed) {
            (Some(index), Some(base)) if !grams.is_empty() => {
                let changes = if base == tip { Some(Default::default()) } else { g.changed_files(base, tip).ok() };
                match changes.filter(|c: &keel_git::FileChanges| c.changed.len() <= MAX_DIFF_FILES) {
                    Some(c) => {
                        let skip: std::collections::HashSet<&str> =
                            c.changed.iter().map(|f| f.path.as_str()).chain(c.deleted.iter().map(String::as_str)).collect();
                        let mut files: Vec<(String, Oid)> = index
                            .candidates_with(&grams, &[repo_id], MAX_CANDIDATES)?
                            .into_iter()
                            .filter(|c| !skip.contains(c.path.as_str()))
                            .filter_map(|c| Some((c.path, Oid::from_str(&c.blob).ok()?)))
                            .collect();
                        files.extend(c.changed.into_iter().map(|f| (f.path, f.id)));
                        files.sort();
                        Some(files)
                    }
                    None => None,
                }
            }
            _ => None,
        };
        let files = match plan {
            Some(files) => files,
            None => g.files(tip)?.into_iter().map(|f| (f.path, f.id)).collect(),
        };
        let mut out = Vec::new();
        let mut budget = MAX_SCAN_BYTES;
        let stopped = scan(g, &m, files, &t, MAX_RESULTS, &mut budget, &mut out);
        Ok(Some((out, stopped)))
    })
    .await
}

#[derive(Deserialize)]
pub struct SearchQuery {
    #[serde(default)]
    q: String,
    /// Set (to anything) for a regular expression.
    regex: Option<String>,
    /// The branch, tag or commit to search (repository search only).
    #[serde(rename = "ref")]
    rev: Option<String>,
}

#[derive(Template)]
#[template(path = "search.html")]
struct SearchPage {
    page: Page,
    q: String,
    regex: bool,
    problem: Option<String>,
    results: Vec<FileResult>,
    truncated: bool,
    searched: bool,
    enabled: bool,
    scope: String,
}

/// `/search`: the default branches of all repositories the viewer can read.
pub async fn global(State(state): State<AppState>, Viewer(viewer): Viewer, Query(sq): Query<SearchQuery>) -> AppResult {
    let q = sq.q.trim().to_owned();
    let regex = sq.regex.is_some();
    let matcher = if q.is_empty() { None } else { Some(Matcher::parse(&q, regex)) };
    let mut problem = matcher.as_ref().and_then(|m| m.as_ref().err().cloned());
    if let Some(Ok(m)) = &matcher
        && m.grams().is_empty()
    {
        problem = Some(format!(
            "To search all repositories, a regular expression needs some literal text ({MIN_QUERY_CHARS} or more characters in a row, like fn \\w+). Within one repository, any expression works."
        ));
    }
    let searched = matcher.is_some() && problem.is_none();
    let (results, truncated) = match (&matcher, searched) {
        (Some(Ok(m)), true) => {
            let mut repos = Vec::new();
            for id in state.core.visible_repo_ids(viewer.as_ref()).await? {
                repos.extend(state.core.repo_by_id(id).await?);
            }
            run_search(&state, m, repos).await?
        }
        _ => (Vec::new(), false),
    };
    let title = if q.is_empty() { "Search".to_owned() } else { format!("{q} · Search") };
    html(SearchPage {
        enabled: state.search.is_some(),
        page: Page::new(&state, viewer, title),
        q,
        regex,
        problem,
        results,
        truncated,
        searched,
        scope: "on default branches".into(),
    })
}

#[derive(Template)]
#[template(path = "repo/search.html")]
struct RepoSearchPage {
    page: Page,
    repo: RepoView,
    q: String,
    regex: bool,
    problem: Option<String>,
    results: Vec<FileResult>,
    truncated: bool,
    searched: bool,
    enabled: bool,
    scope: String,
    /// Branches to choose from, and the one searched.
    branches: Vec<String>,
    rev: String,
}

/// `/<owner>/<repo>/search`: one repository, any branch.
pub async fn in_repo(
    State(state): State<AppState>,
    Viewer(viewer): Viewer,
    Path((owner, name)): Path<(String, String)>,
    Query(sq): Query<SearchQuery>,
) -> AppResult {
    let (repo, access) = load(&state, viewer.as_ref(), &owner, &name).await?;
    let q = sq.q.trim().to_owned();
    let regex = sq.regex.is_some();
    let (default, branches) = with_git(&state, &repo, |g| {
        Ok((g.default_branch()?.unwrap_or_default(), g.branches()?.into_iter().map(|b| b.name).collect::<Vec<_>>()))
    })
    .await?;
    let rev = sq.rev.filter(|r| !r.trim().is_empty()).unwrap_or(default);
    let matcher = if q.is_empty() { None } else { Some(Matcher::parse(&q, regex)) };
    let mut problem = matcher.as_ref().and_then(|m| m.as_ref().err().cloned());
    let (results, truncated) = match &matcher {
        Some(Ok(m)) => match search_rev(&state, m, &repo, rev.clone()).await? {
            Some(found) => found,
            None => {
                problem = Some(format!("There's no branch, tag or commit “{rev}”."));
                (Vec::new(), false)
            }
        },
        _ => (Vec::new(), false),
    };
    let searched = matcher.is_some() && problem.is_none();
    html(RepoSearchPage {
        // With an index, searching works even while it's unavailable.
        enabled: true,
        repo: RepoView::new(&state, &repo, access, viewer.as_ref(), "code", &rev).await?,
        page: Page::new(&state, viewer, format!("Search · {}/{}", repo.owner, repo.name)),
        scope: format!("in {rev}"),
        q,
        regex,
        problem,
        results,
        truncated,
        searched,
        branches,
        rev,
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

    #[test]
    fn matchers() {
        let text = Matcher::parse("Main(", false).unwrap();
        assert_eq!(text.find("fn main() {}"), [(3, 8)]);
        assert!(Matcher::parse("ab", false).is_err(), "too short");
        assert!(Matcher::parse(&"é".repeat(MAX_QUERY_CHARS), false).is_ok());
        let long = "abc".repeat(100);
        for regex in [false, true] {
            assert!(Matcher::parse(&long, regex).unwrap_err().contains("at most 256 characters"));
        }

        let re = Matcher::parse(r"fn\s+\w+\(", true).unwrap();
        assert_eq!(re.find("pub fn  run(x)"), [(4, 12)]);
        assert!(re.find("FN RUN(").is_empty(), "case-sensitive unless (?i)");
        assert_eq!(re.grams(), keel_search::trigrams("fn"), "too short for trigrams: scan everything");
        let slashed = Matcher::parse("/(?i)hello/", false).unwrap();
        assert_eq!(slashed.find("say HELLO"), [(4, 9)]);
        assert_eq!(slashed.grams(), keel_search::trigrams("hello"));
        assert!(Matcher::parse("x*", true).unwrap().find("abc").is_empty(), "empty matches don't count");
        assert!(Matcher::parse("(", true).unwrap_err().contains("valid regular expression"));
        assert!(Matcher::parse("a{1000}{1000}", true).unwrap_err().contains("too complex"));
    }
}
