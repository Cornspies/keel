//! Blame: which commit last changed each line of a file.

use std::sync::Arc;
use std::time::Duration;

use askama::Template;
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Redirect};
use keel_git::{Blame, Oid};

use crate::error::{AppResult, html};
use crate::render::{datetime, relative_time};
use crate::repo::{Crumb, MAX_DISPLAY_BYTES, RefNames, RepoView, crumbs, encode_path, highlighted, load, with_git};
use crate::session::Viewer;
use crate::{AppError, AppState, Page};

/// Longer blames are stopped, and the page says so; most take well under a
/// second thanks to the commit-graph's Bloom filters.
const BLAME_TIMEOUT: Duration = Duration::from_secs(20);

/// The first line of a hunk, with what it says about the commit.
pub(crate) struct HunkView {
    /// How many lines it spans.
    pub rows: usize,
    pub url: String,
    pub short: String,
    pub summary: String,
    pub author: String,
    /// The file's history filtered to the author's commits.
    pub author_url: String,
    pub when: String,
    pub when_full: String,
    /// The file's blame before this commit, if it existed.
    pub prior_url: Option<String>,
    /// 0 (the file's oldest commit) to 9 (its newest), for the color bar.
    pub age: u8,
}

pub(crate) struct BlameRow {
    pub n: usize,
    /// Highlighted HTML.
    pub code: String,
    pub hunk: Option<HunkView>,
    /// No hunk covers the line (git and the highlighter disagree about the
    /// last line, say): an empty cell keeps the columns aligned.
    pub uncovered: bool,
}

pub(crate) enum BlameContent {
    Rows(Vec<BlameRow>),
    /// The file isn't blamed (it's binary, a Git LFS pointer, or too large
    /// to show); the message says why.
    Unavailable(&'static str),
    TooSlow,
}

#[derive(Template)]
#[template(path = "repo/blame.html")]
struct BlamePage {
    page: Page,
    repo: RepoView,
    rev: String,
    rev_url: String,
    crumbs: Vec<Crumb>,
    refs: RefNames,
    blob_url: String,
    history_url: String,
    content: BlameContent,
    bar: crate::file_view::FileBar,
    /// The file tree beside it (HTML).
    tree: String,
}

/// `GET /{owner}/{repo}/blame/{rev}/{path}`.
pub async fn blame(
    State(state): State<AppState>,
    Viewer(viewer): Viewer,
    Path((owner, name, rest)): Path<(String, String, String)>,
) -> AppResult {
    let (repo, access) = load(&state, viewer.as_ref(), &owner, &name).await?;
    let base = format!("/{}/{}", repo.owner, repo.name);
    let link_base = base.clone();
    let found = with_git(&state, &repo, move |g| {
        let r = g.resolve_rev_path(&rest).ok_or(AppError::NotFound)?;
        if r.path.is_empty() || g.is_dir(r.commit, &r.path)? {
            return Ok(Err(r));
        }
        let blob = g.blob(r.commit, &r.path)?.ok_or(AppError::NotFound)?;
        let refs = RefNames::load(g, &link_base, "tree")?;
        let tree = crate::file_view::tree_html(g, &link_base, &r.rev, r.commit, &r.path)?;
        Ok(Ok((r, blob, refs, g.default_branch()?.unwrap_or_default(), tree)))
    })
    .await?;
    let (r, blob, refs, default_branch, tree) = match found {
        Ok(v) => v,
        Err(r) => {
            return Ok(Redirect::to(&format!("{base}/tree/{}/{}", encode_path(&r.rev), encode_path(&r.path)))
                .into_response());
        }
    };
    let filename = r.path.rsplit('/').next().unwrap_or_default().to_owned();
    let mut commit_count = 0;
    let content = if blob.binary || keel_core::parse_pointer(&blob.data).is_some() {
        BlameContent::Unavailable("Binary files can't be blamed.")
    } else if blob.data.len() > MAX_DISPLAY_BYTES {
        BlameContent::Unavailable("This file is too large to blame.")
    } else {
        match blame_of(&state, &repo, r.commit, &r.path).await? {
            Some(b) => {
                commit_count = b.commits.len();
                let history = format!("{base}/commits/{}/{}", encode_path(&r.rev), encode_path(&r.path));
                BlameContent::Rows(rows(&base, &history, &b, &highlighted(&state, &blob, &filename)))
            }
            None => BlameContent::TooSlow,
        }
    };
    let (rev_url, path_url) = (encode_path(&r.rev), encode_path(&r.path));
    let markdown = matches!(filename.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).as_deref(), Some("md" | "markdown"));
    let mut bar = crate::file_view::FileBar::new(&base, &rev_url, &r.path, "blame", markdown, true);
    bar.copyable = !blob.binary;
    bar.meta = match commit_count {
        0 => String::new(),
        1 => "1 commit".into(),
        n => format!("{n} commits"),
    };
    bar.age_scale = commit_count > 1;
    html(BlamePage {
        bar,
        tree,
        repo: RepoView::new(&state, &repo, access, viewer.as_ref(), "code", &default_branch).await?,
        page: Page::new(&state, viewer, format!("Blame of {} at {} · {}/{}", r.path, r.rev, repo.owner, repo.name)),
        crumbs: crumbs(&base, &r.rev, &r.path),
        blob_url: format!("{base}/blob/{rev_url}/{path_url}"),
        history_url: format!("{base}/commits/{rev_url}/{path_url}"),
        refs,
        content,
        rev: r.rev,
        rev_url,
    })
}

/// The blame of `path` at `commit`, cached (commits never change). `None`
/// if git took too long. Blames run a few at a time, since a long history
/// keeps a CPU busy, and anyone who can read the repository can ask.
async fn blame_of(state: &AppState, repo: &keel_core::Repo, commit: Oid, path: &str) -> AppResult<Option<Arc<Blame>>> {
    let key = (commit, path.to_owned());
    if let Some(hit) = state.blames.get(&key) {
        return Ok(Some(hit));
    }
    let _turn = state.blame_slots.acquire().await.map_err(|e| AppError::Internal(e.into()))?;
    // Someone else may have just made it.
    if let Some(hit) = state.blames.get(&key) {
        return Ok(Some(hit));
    }
    let p = path.to_owned();
    let Some(blame) = with_git(state, repo, move |g| Ok(g.blame(commit, &p, BLAME_TIMEOUT)?)).await? else {
        return Ok(None);
    };
    let blame = Arc::new(blame);
    state.blames.insert(key, blame.clone());
    Ok(Some(blame))
}

/// One row per line, with the commit's details on the first line of each
/// hunk.
fn rows(base: &str, history: &str, blame: &Blame, lines: &[String]) -> Vec<BlameRow> {
    let times = blame.commits.values().map(|c| c.time);
    let (oldest, newest) = (times.clone().min().unwrap_or(0), times.max().unwrap_or(0));
    let mut out: Vec<BlameRow> =
        lines.iter().enumerate().map(|(i, code)| BlameRow { n: i + 1, code: code.clone(), hunk: None, uncovered: true }).collect();
    let mut covered_to = 0; // rows up to here are spanned by an earlier hunk
    for h in &blame.hunks {
        if h.start <= covered_to {
            continue; // overlaps (git doesn't do that)
        }
        let (Some(c), true) = (blame.commits.get(&h.commit), (1..=lines.len()).contains(&h.start)) else { continue };
        let rows = h.lines.min(lines.len() + 1 - h.start);
        for r in &mut out[h.start - 1..h.start - 1 + rows] {
            r.uncovered = false;
        }
        covered_to = h.start - 1 + rows;
        let row = &mut out[h.start - 1];
        let age = if newest > oldest { ((c.time - oldest) * 9 / (newest - oldest)) as u8 } else { 9 };
        row.hunk = Some(HunkView {
            rows,
            url: format!("{base}/commit/{}", h.commit),
            short: h.commit.to_string()[..7].to_owned(),
            summary: c.summary.clone(),
            author: c.author.clone(),
            author_url: format!("{history}?author={}", form_urlencoded::byte_serialize(c.email.as_bytes()).collect::<String>()),
            when: relative_time(c.time),
            when_full: datetime(c.time),
            prior_url: h.previous.as_ref().map(|(id, path)| {
                format!("{base}/blame/{id}/{}#L{}", encode_path(path), h.orig_start)
            }),
            age,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_git::{BlameCommit, BlameHunk};

    #[test]
    fn rows_carry_hunks() {
        let (a, b) = (Oid::from_str(&"a".repeat(40)).unwrap(), Oid::from_str(&"b".repeat(40)).unwrap());
        let commit = |time| BlameCommit { summary: "s".into(), author: "Ada".into(), email: String::new(), time };
        let hunk = |commit, start, lines, previous| BlameHunk {
            commit,
            start,
            lines,
            orig_start: start,
            orig_path: "x y.rs".into(),
            previous,
        };
        let blame = Blame {
            hunks: vec![hunk(a, 1, 2, None), hunk(b, 3, 5, Some((a, "x y.rs".to_owned())))],
            commits: [(a, commit(100)), (b, commit(200))].into(),
        };
        let lines: Vec<String> = ["1", "2", "3"].map(String::from).into();
        let rows = rows("/ada/engine", "/ada/engine/commits/main/x", &blame, &lines);
        assert_eq!(rows.len(), 3);
        let first = rows[0].hunk.as_ref().unwrap();
        assert_eq!((first.rows, first.age, first.prior_url.as_deref()), (2, 0, None));
        assert!(rows[1].hunk.is_none() && !rows[1].uncovered);
        let third = rows[2].hunk.as_ref().unwrap();
        assert_eq!(third.rows, 1, "clamped to the lines there are");
        assert_eq!(third.age, 9);
        assert_eq!(third.prior_url.as_deref(), Some(format!("/ada/engine/blame/{a}/x%20y.rs#L3").as_str()));

        // A line no hunk covers gets an empty cell.
        let short = Blame { hunks: vec![hunk(a, 1, 2, None)], commits: blame.commits.clone() };
        let rows = super::rows("/ada/engine", "/ada/engine/commits/main/x", &short, &lines);
        assert_eq!(rows.iter().map(|r| r.uncovered).collect::<Vec<_>>(), [false, false, true]);
    }
}
