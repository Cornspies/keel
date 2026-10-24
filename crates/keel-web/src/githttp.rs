//! Git's "smart HTTP" protocol. keel only authenticates and authorizes;
//! the protocol itself is spoken by `git upload-pack` / `git receive-pack`,
//! with request and response bodies streamed straight through.

use std::io::Read;

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use keel_core::{Access, Repo, User, looks_like_token};
use keel_git::{Service, git_command};
use serde::Deserialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::io::ReaderStream;

use crate::AppState;
use crate::session::basic_auth;

/// Upper bound for gzip-compressed negotiation requests, which are buffered
/// to be decompressed. Packfiles (pushes) are never compressed this way.
const MAX_GZIP_BODY: usize = 64 * 1024 * 1024;

fn text(status: StatusCode, msg: &str) -> Response {
    (status, format!("{msg}\n")).into_response()
}

fn unauthorized() -> Response {
    let mut res = text(StatusCode::UNAUTHORIZED, "Authentication required");
    res.headers_mut().insert(
        header::WWW_AUTHENTICATE,
        HeaderValue::from_static("Basic realm=\"keel\", charset=\"UTF-8\""),
    );
    res
}

/// Finds the repository and checks the client may run `service` on it.
/// Anonymous clients get 401 for anything they can't access, existing or
/// not, so private repository names don't leak.
#[allow(clippy::result_large_err)] // the error is the response to send
async fn authorize(
    state: &AppState,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
    service: Service,
) -> Result<(Repo, Option<User>), Response> {
    let internal = |e: keel_core::Error| {
        tracing::error!("git http: {e}");
        text(StatusCode::INTERNAL_SERVER_ERROR, "Internal error")
    };
    // Either a password or a personal access token. Tokens are recognised
    // by their prefix (in either field, as some tools put them in the
    // username) and may be limited to reading.
    let (user, can_write) = match basic_auth(headers) {
        Some((login, secret)) => {
            let result = if looks_like_token(&secret) || looks_like_token(&login) {
                let token = if looks_like_token(&secret) { &secret } else { &login };
                state.core.authenticate_token(token).await
            } else {
                state.core.authenticate(&login, &secret).await.map(|u| u.map(|u| (u, true)))
            };
            match result {
                // A password alone isn't enough with two-factor authentication.
                Ok(Some((user, _))) if user.totp_enabled && !looks_like_token(&secret) && !looks_like_token(&login) => {
                    return Err(text(
                        StatusCode::FORBIDDEN,
                        "Two-factor authentication is on for this account: use an access token instead of the password",
                    ));
                }
                Ok(Some((user, can_write))) => (Some(user), can_write),
                Ok(None) => return Err(unauthorized()),
                Err(e) => return Err(internal(e)),
            }
        }
        None => (None, false),
    };
    let name = name.strip_suffix(".git").unwrap_or(name);
    let repo = state.core.repo_or_redirect(owner, name).await.map_err(internal)?;
    let needed = if service.is_write() { Access::Write } else { Access::Read };
    let access = match &repo {
        Some(repo) => {
            let a = state.core.access(repo, user.as_ref()).await.map_err(internal)?;
            if can_write { a } else { a.min(Access::Read) }
        }
        None => Access::None,
    };
    match (repo, &user) {
        (Some(repo), _) if access >= needed => Ok((repo, user)),
        (_, None) => Err(unauthorized()),
        (Some(_), Some(_)) if access == Access::Read => Err(text(
            StatusCode::FORBIDDEN,
            if can_write {
                "You don't have permission to push to this repository"
            } else {
                "This access token is read-only; create one with write access to push"
            },
        )),
        _ => Err(text(StatusCode::NOT_FOUND, "Repository not found")),
    }
}

fn git_protocol(headers: &HeaderMap) -> Option<&str> {
    headers.get("git-protocol").and_then(|v| v.to_str().ok())
}

fn pkt_line(s: &str) -> String {
    format!("{:04x}{s}", s.len() + 4)
}

#[derive(Deserialize)]
pub struct InfoRefsQuery {
    service: Option<String>,
}

pub async fn info_refs(
    State(state): State<AppState>,
    Path((owner, name)): Path<(String, String)>,
    Query(q): Query<InfoRefsQuery>,
    headers: HeaderMap,
) -> Response {
    let Some(service) = q.service.as_deref().and_then(Service::parse) else {
        return text(StatusCode::FORBIDDEN, "Only the smart HTTP protocol is supported; please update git");
    };
    let (repo, _) = match authorize(&state, &headers, &owner, &name, service).await {
        Ok(r) => r,
        Err(res) => return res,
    };
    let protocol = git_protocol(&headers);
    let path = state.core.repo_path(&repo);
    let mut cmd = tokio::process::Command::from(git_command(service, &path, true, true, protocol));
    let out = match cmd.output().await {
        Ok(out) if out.status.success() => out,
        Ok(out) => {
            tracing::error!("{} --advertise-refs failed: {}", service.name(), String::from_utf8_lossy(&out.stderr));
            return text(StatusCode::INTERNAL_SERVER_ERROR, "Internal error");
        }
        Err(e) => {
            tracing::error!("spawning git: {e}");
            return text(StatusCode::INTERNAL_SERVER_ERROR, "Internal error");
        }
    };

    // Protocol v2 responses start directly with the capability list; v0/v1
    // need the service announcement first (see git's http-backend.c).
    let v2 = protocol.is_some_and(|p| p.split(':').any(|kv| kv == "version=2"));
    let mut body = Vec::with_capacity(out.stdout.len() + 64);
    if !v2 {
        body.extend_from_slice(pkt_line(&format!("# service={}\n", service.name())).as_bytes());
        body.extend_from_slice(b"0000");
    }
    body.extend_from_slice(&out.stdout);
    (
        [
            (header::CONTENT_TYPE, format!("application/x-{}-advertisement", service.name())),
            (header::CACHE_CONTROL, "no-cache".to_owned()),
        ],
        body,
    )
        .into_response()
}

pub async fn upload_pack(
    State(state): State<AppState>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    rpc(state, owner, name, headers, body, Service::UploadPack).await
}

pub async fn receive_pack(
    State(state): State<AppState>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    rpc(state, owner, name, headers, body, Service::ReceivePack).await
}

async fn rpc(
    state: AppState,
    owner: String,
    name: String,
    headers: HeaderMap,
    body: Body,
    service: Service,
) -> Response {
    let (repo, user) = match authorize(&state, &headers, &owner, &name, service).await {
        Ok(r) => r,
        Err(res) => return res,
    };
    let pusher = user.map(|u| u.id);
    // What the refs were, to tell webhooks what the push changed.
    let before = match service {
        Service::ReceivePack => state.core.ref_snapshot(repo.id).await.ok(),
        _ => None,
    };
    let path = state.core.repo_path(&repo);
    let mut cmd = tokio::process::Command::from(git_command(
        service,
        &path,
        true,
        false,
        git_protocol(&headers),
    ));
    if service == Service::ReceivePack {
        // Branch protection, enforced by a pre-receive hook.
        match state.core.receive_pack_env(repo.id).await {
            Ok(env) => {
                cmd.envs(env);
            }
            Err(e) => {
                tracing::error!("branch rules: {e}");
                return text(StatusCode::INTERNAL_SERVER_ERROR, "Internal error");
            }
        }
    }
    let mut child = match cmd.kill_on_drop(true).spawn() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("spawning git: {e}");
            return text(StatusCode::INTERNAL_SERVER_ERROR, "Internal error");
        }
    };
    let (Some(mut stdin), Some(stdout), Some(mut stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        return text(StatusCode::INTERNAL_SERVER_ERROR, "Internal error");
    };

    let gzip = headers
        .get(header::CONTENT_ENCODING)
        .is_some_and(|v| v.as_bytes().eq_ignore_ascii_case(b"gzip"));
    tokio::spawn(async move {
        let result: anyhow::Result<()> = async {
            if gzip {
                let compressed = axum::body::to_bytes(body, MAX_GZIP_BODY).await?;
                let plain = tokio::task::spawn_blocking(move || {
                    let mut out = Vec::new();
                    flate2::read::GzDecoder::new(&compressed[..])
                        .take(MAX_GZIP_BODY as u64 * 8)
                        .read_to_end(&mut out)
                        .map(|_| out)
                })
                .await??;
                stdin.write_all(&plain).await?;
            } else {
                let mut stream = body.into_data_stream();
                while let Some(chunk) = stream.next().await {
                    stdin.write_all(&chunk?).await?;
                }
            }
            Ok(())
        }
        .await;
        if let Err(e) = result {
            tracing::debug!("git http request body: {e}");
        }
        // Dropping stdin signals EOF to git.
    });

    let core = state.core.clone();
    tokio::spawn(async move {
        let mut err = String::new();
        let _ = stderr.read_to_string(&mut err).await;
        match child.wait().await {
            Ok(status) if status.success() => {
                if service == Service::ReceivePack
                    && let Err(e) = core.after_push(repo.id, pusher, before).await
                {
                    tracing::warn!("after push: {e}");
                }
            }
            Ok(status) => {
                tracing::warn!(repo = repo.id, "{} exited with {status}: {}", service.name(), err.trim());
            }
            Err(e) => tracing::error!("waiting for git: {e}"),
        }
    });

    (
        [
            (header::CONTENT_TYPE, format!("application/x-{}-result", service.name())),
            (header::CACHE_CONTROL, "no-cache".to_owned()),
        ],
        Body::from_stream(ReaderStream::new(stdout)),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pkt_lines() {
        assert_eq!(pkt_line("# service=git-upload-pack\n"), "001e# service=git-upload-pack\n");
    }
}
