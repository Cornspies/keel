//! What a file's page and its blame share, as on GitHub: the file tree on
//! the left, and the bar above the code (Preview, Code or Blame; raw, copy,
//! download, edit and delete).

use axum::Json;
use axum::extract::{Path, State};
use axum::http::{HeaderValue, header};
use axum::response::{IntoResponse, Response};
use keel_git::{EntryKind, GitRepo, Oid, TreeEntry};
use serde_json::json;

use crate::render::escape;
use crate::repo::{encode_path, load, with_git};
use crate::session::Viewer;
use crate::{AppError, AppResult, AppState};

/// Entries shown per folder; bigger folders link to their page for the rest.
const MAX_ENTRIES: usize = 1000;

/// One of the views of a file: Preview (rendered Markdown), Code or Blame.
pub(crate) struct Mode {
    pub label: &'static str,
    pub url: String,
    pub active: bool,
}

/// The bar above a file's content.
pub(crate) struct FileBar {
    pub modes: Vec<Mode>,
    /// "12 lines · 1.2 KiB".
    pub meta: String,
    pub raw_url: String,
    pub download_url: String,
    /// Copying needs the text: not for binary, LFS or very large files.
    pub copyable: bool,
    pub edit_url: Option<String>,
    pub delete_url: Option<String>,
    /// Who locked the file with Git LFS.
    pub locked_by: Option<String>,
    /// Blame: the scale from older to newer changes.
    pub age_scale: bool,
}

impl FileBar {
    /// The bar for `path` at `rev` (URL-encoded) in the repository at
    /// `base`, showing `active` (`preview`, `code` or `blame`).
    pub(crate) fn new(base: &str, rev_url: &str, path: &str, active: &str, markdown: bool, blamable: bool) -> Self {
        let path_url = encode_path(path);
        let blob = format!("{base}/blob/{rev_url}/{path_url}");
        let mut modes = Vec::new();
        if markdown {
            modes.push(Mode { label: "Preview", url: blob.clone(), active: active == "preview" });
            modes.push(Mode { label: "Code", url: format!("{blob}?plain=1"), active: active == "code" });
        } else {
            modes.push(Mode { label: "Code", url: blob, active: active == "code" });
        }
        if blamable {
            modes.push(Mode { label: "Blame", url: format!("{base}/blame/{rev_url}/{path_url}"), active: active == "blame" });
        }
        let raw_url = format!("{base}/raw/{rev_url}/{path_url}");
        Self {
            modes,
            meta: String::new(),
            download_url: format!("{raw_url}?download=1"),
            raw_url,
            copyable: blamable,
            edit_url: None,
            delete_url: None,
            locked_by: None,
            age_scale: false,
        }
    }
}

/// The file tree beside the file at `path` (HTML): the top of the tree, with
/// the folders leading to `path` open and the others loaded when opened
/// (`tree_list`).
pub(crate) fn tree_html(g: &GitRepo, base: &str, rev: &str, commit: Oid, path: &str) -> keel_git::Result<String> {
    let mut out = String::from(r#"<ul class="tree-list">"#);
    folder(g, base, rev, commit, "", path, &mut out)?;
    out.push_str("</ul>");
    Ok(out)
}

/// Renders the entries of folder `dir` into `out`, opening the folder on
/// the way to `current`.
fn folder(g: &GitRepo, base: &str, rev: &str, commit: Oid, dir: &str, current: &str, out: &mut String) -> keel_git::Result<()> {
    let entries = g.tree(commit, dir)?.unwrap_or_default();
    let rev_url = encode_path(rev);
    for e in entries.iter().take(MAX_ENTRIES) {
        let full = if dir.is_empty() { e.name.clone() } else { format!("{dir}/{}", e.name) };
        let name = escape(&e.name);
        if e.kind == EntryKind::Dir {
            let open = current.starts_with(&format!("{full}/"));
            let tree_url = format!("{base}/tree/{rev_url}/{}", encode_path(&full));
            if open {
                out.push_str(&format!(r#"<li><details open><summary class="tree-dir">{name}</summary><ul class="tree-list">"#));
                folder(g, base, rev, commit, &full, current, out)?;
                out.push_str("</ul></details></li>");
            } else {
                let src = format!("{base}/tree-list/{rev_url}/{}", encode_path(&full));
                out.push_str(&format!(
                    r#"<li><details data-src="{src}"><summary class="tree-dir">{name}</summary><ul class="tree-list"><li><a class="tree-more" href="{tree_url}">Open folder</a></li></ul></details></li>"#
                ));
            }
        } else {
            out.push_str(&entry_link(base, &rev_url, e, &full, full == current));
        }
    }
    if entries.len() > MAX_ENTRIES {
        let tree_url = format!("{base}/tree/{rev_url}/{}", encode_path(dir));
        out.push_str(&format!(r#"<li><a class="tree-more" href="{tree_url}">{} more…</a></li>"#, entries.len() - MAX_ENTRIES));
    }
    Ok(())
}

fn entry_link(base: &str, rev_url: &str, e: &TreeEntry, full: &str, current: bool) -> String {
    let mode = if e.kind == EntryKind::Submodule { "tree" } else { "blob" };
    let aria = if current { r#" aria-current="page""# } else { "" };
    format!(
        r#"<li><a class="tree-file" href="{base}/{mode}/{rev_url}/{}"{aria}>{}</a></li>"#,
        encode_path(full),
        escape(&e.name)
    )
}

/// `GET /{owner}/{repo}/tree-list/{rev}/{path}`: a folder's entries, for
/// opening it in the file tree.
pub async fn tree_list(
    State(state): State<AppState>,
    Viewer(viewer): Viewer,
    Path((owner, name, rest)): Path<(String, String, String)>,
) -> AppResult<Response> {
    let (repo, _) = load(&state, viewer.as_ref(), &owner, &name).await?;
    let base = format!("/{}/{}", repo.owner, repo.name);
    let entries = with_git(&state, &repo, move |g| {
        let r = g.resolve_rev_path(&rest).ok_or(AppError::NotFound)?;
        let entries = g.tree(r.commit, &r.path)?.ok_or(AppError::NotFound)?;
        let rev_url = encode_path(&r.rev);
        let list: Vec<_> = entries
            .iter()
            .take(MAX_ENTRIES)
            .map(|e| {
                let full = if r.path.is_empty() { e.name.clone() } else { format!("{}/{}", r.path, e.name) };
                match e.kind {
                    EntryKind::Dir => json!({
                        "name": e.name,
                        "dir": true,
                        "url": format!("{base}/tree/{rev_url}/{}", encode_path(&full)),
                        "src": format!("{base}/tree-list/{rev_url}/{}", encode_path(&full)),
                    }),
                    EntryKind::Submodule => json!({ "name": e.name, "url": format!("{base}/tree/{rev_url}/{}", encode_path(&full)) }),
                    _ => json!({ "name": e.name, "url": format!("{base}/blob/{rev_url}/{}", encode_path(&full)) }),
                }
            })
            .collect();
        Ok(json!({ "entries": list, "more": entries.len().saturating_sub(MAX_ENTRIES) }))
    })
    .await?;
    let mut res = Json(entries).into_response();
    let cache = if repo.is_private { "private, no-cache" } else { "public, no-cache" };
    res.headers_mut().insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modes() {
        let bar = FileBar::new("/a/r", "main", "docs/READ ME.md", "code", true, true);
        let labels: Vec<(&str, bool)> = bar.modes.iter().map(|m| (m.label, m.active)).collect();
        assert_eq!(labels, [("Preview", false), ("Code", true), ("Blame", false)]);
        assert_eq!(bar.modes[1].url, "/a/r/blob/main/docs/READ%20ME.md?plain=1");
        assert_eq!(bar.download_url, "/a/r/raw/main/docs/READ%20ME.md?download=1");
        let bin = FileBar::new("/a/r", "main", "logo.png", "code", false, false);
        assert_eq!(bin.modes.len(), 1);
        assert!(!bin.copyable);
    }
}
