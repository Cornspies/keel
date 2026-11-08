//! Downloading a branch, tag or commit as `.tar.gz` or `.zip`, streamed
//! from `git archive`.

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::IntoResponse;
use keel_git::{ArchiveFormat, archive_command};
use tokio_util::io::ReaderStream;

use crate::error::AppResult;
use crate::repo::{load, with_git};
use crate::session::Viewer;
use crate::{AppError, AppState};

/// Archives built at the same time; more wait their turn.
pub(crate) const CONCURRENT_ARCHIVES: usize = 4;
/// How long a download may make no progress before git is stopped: a
/// client that stops reading would otherwise keep its turn for good.
const STALL_LIMIT: std::time::Duration = std::time::Duration::from_secs(60);

/// A name usable in a file name: `feature/x` becomes `feature-x`.
fn file_label(rev: &str) -> String {
    rev.chars().map(|c| if c.is_ascii_alphanumeric() || "._-".contains(c) { c } else { '-' }).collect()
}

/// `/<owner>/<repo>/archive/<rev>.tar.gz` and `.zip`.
pub async fn download(
    State(state): State<AppState>,
    Viewer(viewer): Viewer,
    Path((owner, name, rest)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> AppResult {
    let (repo, _) = load(&state, viewer.as_ref(), &owner, &name).await?;
    let (rev, format) = ArchiveFormat::split(&rest).ok_or(AppError::NotFound)?;
    let r = rev.to_owned();
    let commit = with_git(&state, &repo, move |g| Ok(g.resolve_rev(&r))).await?.ok_or(AppError::NotFound)?;
    let etag = format!("\"{commit}-{}\"", format.extension());
    let cache = match (repo.is_private, rev.len() == 40) {
        (false, true) => "public, max-age=31536000, immutable",
        (false, false) => "public, no-cache",
        (true, _) => "private, no-cache",
    };
    if headers.get(header::IF_NONE_MATCH).is_some_and(|v| v.as_bytes() == etag.as_bytes()) {
        return Ok((StatusCode::NOT_MODIFIED, [(header::ETAG, etag)]).into_response());
    }

    let base = format!("{}-{}", repo.name, file_label(rev));
    let permit = state.archives.clone().acquire_owned().await.map_err(|e| AppError::Internal(e.into()))?;
    let mut child = tokio::process::Command::from(archive_command(&state.core.repo_path(&repo), commit, format, &base))
        .kill_on_drop(true)
        .spawn()?;
    let (Some(stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
        return Err(AppError::Internal(anyhow::anyhow!("git archive: no output")));
    };
    let repo_id = repo.id;
    // When (seconds after the start) the client last read some of it.
    let started = std::time::Instant::now();
    let progress = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
    let watched = progress.clone();
    tokio::spawn(async move {
        let _permit = permit; // held until git is done
        let mut err = String::new();
        let stalled = async {
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                let idle = started.elapsed().as_secs().saturating_sub(watched.load(std::sync::atomic::Ordering::Relaxed));
                if idle > STALL_LIMIT.as_secs() {
                    break;
                }
            }
        };
        tokio::select! {
            _ = tokio::io::AsyncReadExt::read_to_string(&mut stderr, &mut err) => {}
            () = stalled => {
                let _ = child.start_kill();
                tracing::info!(repo = repo_id, "git archive stopped: the download stalled");
            }
        }
        match child.wait().await {
            Ok(s) if !s.success() => tracing::warn!(repo = repo_id, "git archive: {}", err.trim()),
            Err(e) => tracing::warn!(repo = repo_id, "git archive: {e}"),
            _ => {}
        }
    });
    let stream = futures_util::StreamExt::inspect(ReaderStream::new(stdout), move |_| {
        progress.store(started.elapsed().as_secs(), std::sync::atomic::Ordering::Relaxed);
    });
    let mut res = Body::from_stream(stream).into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static(format.content_type()));
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{base}.{}\"", format.extension())) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    h.insert(header::ETAG, HeaderValue::from_str(&etag).expect("hex"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    Ok(res)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        assert_eq!(file_label("feature/x"), "feature-x");
        assert_eq!(file_label("v1.0"), "v1.0");
        assert_eq!(file_label("a\"b"), "a-b");
    }
}
