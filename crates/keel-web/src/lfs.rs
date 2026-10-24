//! The Git LFS server: the batch API and basic transfers
//! (<https://github.com/git-lfs/git-lfs/blob/main/docs/api>), under
//! `/<owner>/<repo>.git/info/lfs`, where LFS clients look for it.
//!
//! Clients authenticate like git does (password or access token, HTTP
//! Basic), or with the short-lived token the batch response and
//! `git-lfs-authenticate` (over SSH) hand out. Uploads are streamed to disk
//! and their SHA-256 checked before they're stored.

use std::sync::atomic::{AtomicU64, Ordering};

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use keel_core::{Access, LFS_TOKEN_SECS, Repo, User, valid_oid};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tokio_util::io::ReaderStream;

use crate::AppState;
use crate::githttp::{CredentialError, TWO_FACTOR, basic_credentials};

const LFS_JSON: &str = "application/vnd.git-lfs+json";
const MAX_BATCH: usize = 1000;

fn reply(status: StatusCode, body: Value) -> Response {
    let mut res = (status, body.to_string()).into_response();
    res.headers_mut().insert(header::CONTENT_TYPE, HeaderValue::from_static(LFS_JSON));
    res
}

fn fail(status: StatusCode, msg: &str) -> Response {
    let mut res = reply(status, json!({ "message": msg }));
    if status == StatusCode::UNAUTHORIZED {
        res.headers_mut().insert(header::WWW_AUTHENTICATE, HeaderValue::from_static("Basic realm=\"keel\""));
        res.headers_mut().insert("LFS-Authenticate", HeaderValue::from_static("Basic realm=\"keel\""));
    }
    res
}

fn internal(e: impl std::fmt::Display) -> Response {
    tracing::error!("lfs: {e}");
    fail(StatusCode::INTERNAL_SERVER_ERROR, "Internal error")
}

/// Finds the repository and checks the client may read it (or, with
/// `write`, upload to it). Like git over HTTP, anonymous clients get 401
/// for what they can't see, so private names don't leak.
#[allow(clippy::result_large_err)] // the error is the response to send
async fn authorize(state: &AppState, headers: &HeaderMap, owner: &str, name: &str, write: bool) -> Result<(Repo, Option<User>), Response> {
    let name = name.strip_suffix(".git").unwrap_or(name);
    let repo = state.core.repo_or_redirect(owner, name).await.map_err(internal)?;
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")));
    let (user, can_write) = match (bearer, &repo) {
        (Some(token), Some(repo)) => match state.core.check_lfs_token(token.trim(), repo).await.map_err(internal)? {
            Some((user, write)) => (Some(user), write),
            None => return Err(fail(StatusCode::UNAUTHORIZED, "This token is invalid or expired")),
        },
        (Some(_), None) => return Err(fail(StatusCode::UNAUTHORIZED, "Authentication required")),
        (None, _) => match basic_credentials(state, headers).await {
            Ok(Some((user, can_write))) => (Some(user), can_write),
            Ok(None) => (None, false),
            Err(CredentialError::Wrong) => return Err(fail(StatusCode::UNAUTHORIZED, "Wrong username or password")),
            Err(CredentialError::TwoFactor) => return Err(fail(StatusCode::FORBIDDEN, TWO_FACTOR)),
            Err(CredentialError::Internal(e)) => return Err(internal(e)),
        },
    };
    let needed = if write { Access::Write } else { Access::Read };
    let access = match &repo {
        Some(r) => {
            let a = state.core.access(r, user.as_ref()).await.map_err(internal)?;
            if can_write { a } else { a.min(Access::Read) }
        }
        None => Access::None,
    };
    match repo {
        Some(repo) if access >= needed => {
            if write && state.core.is_mirror(repo.id).await.map_err(internal)? {
                return Err(fail(StatusCode::FORBIDDEN, "This repository is a mirror"));
            }
            Ok((repo, user))
        }
        _ if user.is_none() => Err(fail(StatusCode::UNAUTHORIZED, "Authentication required")),
        Some(_) if access == Access::Read => {
            Err(fail(StatusCode::FORBIDDEN, if can_write { "You can't push to this repository" } else { "This access token is read-only" }))
        }
        _ => Err(fail(StatusCode::NOT_FOUND, "Repository not found")),
    }
}

fn object_url(state: &AppState, repo: &Repo, oid: &str) -> String {
    format!("{}/{}/{}.git/info/lfs/objects/{oid}", state.core.config.base_url, repo.owner, repo.name)
}

#[derive(Deserialize)]
struct BatchRequest {
    operation: String,
    #[serde(default)]
    transfers: Vec<String>,
    objects: Vec<ObjectSpec>,
}

#[derive(Deserialize)]
struct ObjectSpec {
    oid: String,
    size: i64,
}

/// `POST …/info/lfs/objects/batch`: where to download or upload objects.
pub async fn batch(
    State(state): State<AppState>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let Ok(req) = serde_json::from_slice::<BatchRequest>(&body) else {
        return fail(StatusCode::UNPROCESSABLE_ENTITY, "Invalid batch request");
    };
    let upload = match req.operation.as_str() {
        "download" => false,
        "upload" => true,
        _ => return fail(StatusCode::UNPROCESSABLE_ENTITY, "operation must be download or upload"),
    };
    if !req.transfers.is_empty() && !req.transfers.iter().any(|t| t == "basic") {
        return fail(StatusCode::UNPROCESSABLE_ENTITY, "Only the basic transfer is supported");
    }
    if req.objects.len() > MAX_BATCH {
        return fail(StatusCode::UNPROCESSABLE_ENTITY, "Too many objects in one request");
    }
    let (repo, user) = match authorize(&state, &headers, &owner, &name, upload).await {
        Ok(r) => r,
        Err(res) => return res,
    };
    // Transfers are authorized with a token rather than the password again.
    let auth = user.as_ref().map(|u| json!({ "Authorization": format!("Bearer {}", state.core.lfs_token(u, &repo, upload)) }));
    let max = state.core.config.lfs_max_bytes;
    let mut objects = Vec::new();
    for o in req.objects {
        let mut out = json!({ "oid": o.oid, "size": o.size, "authenticated": true });
        if !valid_oid(&o.oid) || o.size < 0 {
            out["error"] = json!({ "code": 422, "message": "Invalid object" });
            objects.push(out);
            continue;
        }
        let stored = match state.core.lfs_object(repo.id, &o.oid).await {
            Ok(s) => s,
            Err(e) => return internal(e),
        };
        let mut action = json!({ "href": object_url(&state, &repo, &o.oid), "expires_in": LFS_TOKEN_SECS });
        if let Some(a) = &auth {
            action["header"] = a.clone();
        }
        match (upload, stored) {
            (false, Some(_)) => out["actions"] = json!({ "download": action }),
            (false, None) => out["error"] = json!({ "code": 404, "message": "Object does not exist" }),
            (true, Some(_)) => {} // already there
            (true, None) if o.size as u64 > max => {
                let msg = format!("Larger than this server's limit of {} MiB", max >> 20);
                out["error"] = json!({ "code": 422, "message": msg });
            }
            (true, None) => out["actions"] = json!({ "upload": action }),
        }
        objects.push(out);
    }
    reply(StatusCode::OK, json!({ "transfer": "basic", "objects": objects, "hash_algo": "sha256" }))
}

/// `GET …/info/lfs/objects/<oid>`.
pub async fn download(
    State(state): State<AppState>,
    Path((owner, name, oid)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let (repo, _) = match authorize(&state, &headers, &owner, &name, false).await {
        Ok(r) => r,
        Err(res) => return res,
    };
    if !valid_oid(&oid) {
        return fail(StatusCode::NOT_FOUND, "Object does not exist");
    }
    match state.core.lfs_object(repo.id, &oid).await {
        Ok(Some(size)) => serve_object(&state, &oid, size, "application/octet-stream").await,
        Ok(None) => fail(StatusCode::NOT_FOUND, "Object does not exist"),
        Err(e) => internal(e),
    }
}

/// Streams a stored object.
pub(crate) async fn serve_object(state: &AppState, oid: &str, size: u64, content_type: &str) -> Response {
    let file = match tokio::fs::File::open(state.core.lfs_path(oid)).await {
        Ok(f) => f,
        Err(e) => return internal(e),
    };
    let mut res = Body::from_stream(ReaderStream::new(file)).into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(size));
    if let Ok(v) = HeaderValue::from_str(content_type) {
        h.insert(header::CONTENT_TYPE, v);
    }
    res
}

/// `PUT …/info/lfs/objects/<oid>`: stores an upload if its content matches.
pub async fn upload(
    State(state): State<AppState>,
    Path((owner, name, oid)): Path<(String, String, String)>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let (repo, _) = match authorize(&state, &headers, &owner, &name, true).await {
        Ok(r) => r,
        Err(res) => return res,
    };
    if !valid_oid(&oid) {
        return fail(StatusCode::UNPROCESSABLE_ENTITY, "Invalid object id");
    }
    match receive(&state, &oid, body).await {
        Ok(Ok(size)) => match state.core.lfs_link(repo.id, &oid, size).await {
            Ok(()) => StatusCode::OK.into_response(),
            Err(e) => internal(e),
        },
        Ok(Err(res)) => res,
        Err(e) => internal(e),
    }
}

/// Writes an upload to a temporary file while hashing it, then moves it
/// into place. Returns its size, or the response for a bad upload.
async fn receive(state: &AppState, oid: &str, body: Body) -> std::io::Result<Result<u64, Response>> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let tmp_dir = state.core.lfs_tmp_dir();
    tokio::fs::create_dir_all(&tmp_dir).await?;
    let tmp = tmp_dir.join(format!("{}-{}-{}", std::process::id(), crate::now(), NEXT.fetch_add(1, Ordering::Relaxed)));
    let max = state.core.config.lfs_max_bytes;
    let result = async {
        let mut file = tokio::fs::File::create(&tmp).await?;
        let mut hasher = Sha256::new();
        let mut size = 0u64;
        let mut stream = body.into_data_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(std::io::Error::other)?;
            size += chunk.len() as u64;
            if size > max {
                let msg = format!("Larger than this server's limit of {} MiB", max >> 20);
                return Ok(Err(fail(StatusCode::PAYLOAD_TOO_LARGE, &msg)));
            }
            hasher.update(&chunk);
            file.write_all(&chunk).await?;
        }
        file.sync_all().await?;
        if hex::encode(hasher.finalize()) != oid {
            return Ok(Err(fail(StatusCode::UNPROCESSABLE_ENTITY, "The content doesn't match the object id")));
        }
        let path = state.core.lfs_path(oid);
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        tokio::fs::rename(&tmp, &path).await?;
        Ok(Ok(size))
    }
    .await;
    let _ = tokio::fs::remove_file(&tmp).await; // gone already if it was stored
    result
}

/// `POST …/info/lfs/locks/verify`: file locking isn't supported, so there
/// are never locks. Answering keeps clients from warning on every push.
pub async fn verify_locks(
    State(state): State<AppState>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    match authorize(&state, &headers, &owner, &name, true).await {
        Ok(_) => reply(StatusCode::OK, json!({ "ours": [], "theirs": [] })),
        Err(res) => res,
    }
}

/// `GET …/info/lfs/locks`.
pub async fn list_locks(
    State(state): State<AppState>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    match authorize(&state, &headers, &owner, &name, false).await {
        Ok(_) => reply(StatusCode::OK, json!({ "locks": [] })),
        Err(res) => res,
    }
}
