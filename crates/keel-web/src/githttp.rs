//! Git's "smart HTTP" protocol. keel only authenticates and authorizes;
//! the protocol itself is spoken by `git upload-pack` / `git receive-pack`,
//! with request and response bodies streamed straight through.

use axum::body::Body;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use keel_core::{Access, GitSession, GitTarget, User, looks_like_token};
use keel_git::{Service, git_command};
use serde::Deserialize;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio_util::io::{ReaderStream, StreamReader};

use crate::AppState;
use crate::session::basic_auth;

/// Upper bound for gzip-compressed requests once decompressed. Clients only
/// compress negotiation requests, which are small; packfiles (pushes) are
/// never compressed this way.
const MAX_GZIP_PLAIN: u64 = 64 * 1024 * 1024;

/// Streams a request body into git's stdin, decompressing it if `gzip`.
/// Returns whether it decompressed to more than [`MAX_GZIP_PLAIN`] (then
/// git only got the start). Without gzip, the body goes through as is.
async fn write_body(body: Body, gzip: bool, mut stdin: impl AsyncWrite + Unpin) -> anyhow::Result<bool> {
    let mut stream = body.into_data_stream();
    if !gzip {
        while let Some(chunk) = stream.next().await {
            stdin.write_all(&chunk?).await?;
        }
        return Ok(false);
    }
    let compressed = StreamReader::new(stream.map(|c| c.map_err(std::io::Error::other)));
    copy_capped(async_compression::tokio::bufread::GzipDecoder::new(compressed), stdin).await
}

/// Copies `plain` to `stdin`, up to [`MAX_GZIP_PLAIN`] bytes; whether there
/// was more. Keeps counting if git stops reading (e.g. at garbage), so
/// oversized requests are always recognized.
async fn copy_capped(mut plain: impl AsyncRead + Unpin, mut stdin: impl AsyncWrite + Unpin) -> anyhow::Result<bool> {
    let mut buf = vec![0; 64 * 1024];
    let (mut total, mut git_reads) = (0, true);
    loop {
        let n = plain.read(&mut buf).await?;
        if n == 0 {
            return Ok(false);
        }
        total += n as u64;
        if total > MAX_GZIP_PLAIN {
            return Ok(true);
        }
        git_reads = git_reads && stdin.write_all(&buf[..n]).await.is_ok();
    }
}

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

pub(crate) const TWO_FACTOR: &str =
    "Two-factor authentication is on for this account: use an access token instead of the password";

pub(crate) enum CredentialError {
    Wrong,
    /// A password for an account with two-factor authentication.
    TwoFactor,
    /// Too many wrong passwords; the message says for how long.
    Locked(String),
    Internal(keel_core::Error),
}

/// The user of HTTP Basic credentials (`None` without any), and whether
/// they may write: a password, or a personal access token (recognised by
/// its prefix, in either field, as some tools put it in the username),
/// which may be limited to reading.
pub(crate) async fn basic_credentials(state: &AppState, headers: &HeaderMap) -> Result<Option<(User, bool)>, CredentialError> {
    let Some((login, secret)) = basic_auth(headers) else { return Ok(None) };
    let token = [&secret, &login].into_iter().find(|s| looks_like_token(s));
    let result = match token {
        Some(t) => state.core.authenticate_token(t).await,
        None => state.core.authenticate(&login, &secret).await.map(|u| u.map(|u| (u, true))),
    };
    match result {
        // A password alone isn't enough with two-factor authentication.
        Ok(Some((user, _))) if user.totp_enabled && token.is_none() => Err(CredentialError::TwoFactor),
        Ok(Some(found)) => Ok(Some(found)),
        Ok(None) => Err(CredentialError::Wrong),
        Err(keel_core::Error::Invalid(msg)) => Err(CredentialError::Locked(msg)),
        Err(e) => Err(CredentialError::Internal(e)),
    }
}

/// Finds the repository (or wiki) and checks the client may run `service`
/// on it, then prepares running git. Anonymous clients get 401 for anything
/// they can't access, existing or not, so private repository names don't
/// leak.
#[allow(clippy::result_large_err)] // the error is the response to send
async fn authorize(
    state: &AppState,
    headers: &HeaderMap,
    owner: &str,
    name: &str,
    service: Service,
) -> Result<(GitTarget, GitSession, Option<User>), Response> {
    let internal = |e: keel_core::Error| {
        tracing::error!("git http: {e}");
        text(StatusCode::INTERNAL_SERVER_ERROR, "Internal error")
    };
    let (user, can_write) = match basic_credentials(state, headers).await {
        Ok(Some((user, can_write))) => (Some(user), can_write),
        Ok(None) => (None, false),
        Err(CredentialError::Wrong) => return Err(unauthorized()),
        Err(CredentialError::TwoFactor) => return Err(text(StatusCode::FORBIDDEN, TWO_FACTOR)),
        Err(CredentialError::Locked(msg)) => return Err(text(StatusCode::TOO_MANY_REQUESTS, &msg)),
        Err(CredentialError::Internal(e)) => return Err(internal(e)),
    };
    let target = state.core.git_target(owner, name).await.map_err(internal)?;
    let needed = if service.is_write() { Access::Write } else { Access::Read };
    let access = match &target {
        Some(t) => {
            let a = state.core.access(&t.repo, user.as_ref()).await.map_err(internal)?;
            if can_write { a } else { a.min(Access::Read) }
        }
        None => Access::None,
    };
    match (target, &user) {
        (Some(target), _) if access >= needed => {
            if service.is_write() && !target.wiki && state.core.is_mirror(target.repo.id).await.map_err(internal)? {
                return Err(text(StatusCode::FORBIDDEN, "This repository is a mirror; push to the repository it mirrors"));
            }
            match state.core.git_session(&target, service.is_write()).await {
                Err(keel_core::Error::Invalid(msg)) => Err(text(StatusCode::FORBIDDEN, &msg)),
                Err(e) => Err(internal(e)),
                Ok(Some(session)) => Ok((target, session, user)),
                Ok(None) => Err(text(StatusCode::NOT_FOUND, "This wiki has no pages yet; push to it or create one on the web")),
            }
        }
        (_, None) => Err(unauthorized()),
        (Some(t), Some(_)) if access == Access::Read => Err(text(
            StatusCode::FORBIDDEN,
            if t.repo.is_archived() {
                "This repository is archived, so it's read-only"
            } else if can_write {
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
    let (_, session, _) = match authorize(&state, &headers, &owner, &name, service).await {
        Ok(r) => r,
        Err(res) => return res,
    };
    let protocol = git_protocol(&headers);
    let mut cmd = tokio::process::Command::from(git_command(service, &session.path, true, true, protocol));
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
    let (target, session, user) = match authorize(&state, &headers, &owner, &name, service).await {
        Ok(r) => r,
        Err(res) => return res,
    };
    let pusher = user.map(|u| u.id);
    state.core.metrics().git_operation(if service.is_write() { "receive-pack" } else { "upload-pack" }, "http");
    let GitSession { path, env, before } = session;
    let mut cmd = tokio::process::Command::from(git_command(
        service,
        &path,
        true,
        false,
        git_protocol(&headers),
    ));
    // Branch protection, enforced by a pre-receive hook.
    cmd.envs(env);
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
    let mut writer = tokio::spawn(async move {
        // Dropping stdin at the end signals EOF to git.
        write_body(body, gzip, &mut stdin).await.unwrap_or_else(|e| {
            tracing::debug!("git http request body: {e}");
            false
        })
    });

    let core = state.core.clone();
    tokio::spawn(async move {
        let mut err = String::new();
        let _ = stderr.read_to_string(&mut err).await;
        match child.wait().await {
            Ok(status) if status.success() => {
                if service == Service::ReceivePack
                    && let Err(e) = core.after_receive(&target, pusher, before).await
                {
                    tracing::warn!("after push: {e}");
                }
            }
            Ok(status) => {
                tracing::warn!(repo = target.repo.id, "{} exited with {status}: {}", service.name(), err.trim());
            }
            Err(e) => tracing::error!("waiting for git: {e}"),
        }
    });

    // Answer once git does, or refuses an oversized request: git stops
    // without output when its input is cut off.
    let mut stdout = ReaderStream::new(stdout);
    let mut oversized = None;
    let first = tokio::select! {
        biased;
        r = &mut writer => {
            oversized = Some(r.unwrap_or(false));
            if oversized == Some(true) { None } else { stdout.next().await }
        }
        chunk = stdout.next() => chunk,
    };
    if first.is_none() && oversized.is_none() {
        oversized = Some(writer.await.unwrap_or(false));
    }
    if oversized == Some(true) {
        return text(StatusCode::PAYLOAD_TOO_LARGE, "Request too large");
    }
    (
        [
            (header::CONTENT_TYPE, format!("application/x-{}-result", service.name())),
            (header::CACHE_CONTROL, "no-cache".to_owned()),
        ],
        Body::from_stream(futures_util::stream::iter(first).chain(stdout)),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn gzip_limit() {
        use std::io::Write;
        let gzip = |len: usize| {
            let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            e.write_all(&vec![b'0'; len]).unwrap();
            Body::from(e.finish().unwrap())
        };
        let mut out = Vec::new();
        assert!(!write_body(gzip(1000), true, &mut out).await.unwrap());
        assert_eq!(out, vec![b'0'; 1000]);
        // Too large, whether git reads it all or gives up early.
        let mut out = Vec::new();
        assert!(write_body(gzip(MAX_GZIP_PLAIN as usize + 1), true, &mut out).await.unwrap());
        assert!(out.len() as u64 <= MAX_GZIP_PLAIN);
        let (stdin, stopped) = tokio::io::duplex(1024);
        drop(stopped);
        assert!(write_body(gzip(MAX_GZIP_PLAIN as usize + 1), true, stdin).await.unwrap());
        assert!(write_body(Body::from("not gzip"), true, Vec::new()).await.is_err());
        let mut out = Vec::new();
        assert!(!write_body(Body::from("plain"), false, &mut out).await.unwrap());
        assert_eq!(out, b"plain");
    }

    #[test]
    fn pkt_lines() {
        assert_eq!(pkt_line("# service=git-upload-pack\n"), "001e# service=git-upload-pack\n");
    }
}
