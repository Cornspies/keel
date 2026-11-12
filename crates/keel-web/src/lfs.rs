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
use keel_core::{Access, LFS_TOKEN_SECS, LfsLock, Repo, User, valid_oid};
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
/// for what they can't see, so private names don't leak. The user is
/// `None` for anonymous readers and for deploy keys (with a token from
/// `git-lfs-authenticate`), so after a `write` check it means a deploy key.
#[allow(clippy::result_large_err)] // the error is the response to send
async fn authorize(state: &AppState, headers: &HeaderMap, owner: &str, name: &str, write: bool) -> Result<(Repo, Option<User>), Response> {
    let name = name.strip_suffix(".git").unwrap_or(name);
    let repo = state.core.repo_or_redirect(owner, name).await.map_err(internal)?;
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")));
    if let (Some(token), Some(r)) = (bearer, &repo)
        && let Some(key_writes) = state.core.check_lfs_deploy_token(token.trim(), r).await.map_err(internal)?
    {
        let repo = repo.expect("checked");
        return match () {
            _ if write && (!key_writes || repo.is_archived()) => {
                Err(fail(StatusCode::FORBIDDEN, "This deploy key can't push to this repository"))
            }
            _ if write && state.core.is_mirror(repo.id).await.map_err(internal)? => {
                Err(fail(StatusCode::FORBIDDEN, "This repository is a mirror"))
            }
            _ => Ok((repo, None)),
        };
    }
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
            Err(CredentialError::Locked(msg)) => return Err(fail(StatusCode::TOO_MANY_REQUESTS, &msg)),
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
    let auth = match &user {
        Some(u) => Some(json!({ "Authorization": format!("Bearer {}", state.core.lfs_token(u, &repo, upload)) })),
        // A deploy key's token (anonymous readers have no header).
        None => headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).map(|v| json!({ "Authorization": v })),
    };
    let max = state.core.config.lfs_max_bytes;
    let mut room = if upload {
        match state.core.storage_room(repo.owner_id).await {
            Ok(r) => r,
            Err(e) => return internal(e),
        }
    } else {
        None
    };
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
            (true, None) if room.is_some_and(|r| o.size as u64 > r) => {
                let msg = format!("{} is out of storage on this server", repo.owner);
                out["error"] = json!({ "code": 507, "message": msg });
            }
            (true, None) => {
                if let Some(r) = room.as_mut() {
                    *r -= o.size as u64;
                }
                out["actions"] = json!({ "upload": action });
            }
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
        Ok(Some(size)) => {
            state.core.metrics().lfs_transfer("download", size);
            serve_object(&state, &oid, size, "application/octet-stream").await
        }
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
    let mut res = Body::from_stream(ReaderStream::with_capacity(file, crate::repo::STREAM_CHUNK)).into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_LENGTH, HeaderValue::from(size));
    if let Ok(v) = HeaderValue::from_str(content_type) {
        h.insert(header::CONTENT_TYPE, v);
    }
    // Whatever someone uploaded, served from keel's origin: never run, or
    // taken for another type than it's sent as.
    h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static("sandbox; default-src 'none'"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
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
    let room = match state.core.storage_room(repo.owner_id).await {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    match receive(&state, &oid, body, room).await {
        // The room is checked again as it's recorded: other uploads may have
        // used it meanwhile.
        Ok(Ok((tmp, size))) => {
            let stored = state.core.lfs_store(repo.id, &tmp, &oid, size).await;
            let _ = tokio::fs::remove_file(&tmp).await; // gone already if it was stored
            match stored {
                Ok(()) => {
                    state.core.metrics().lfs_transfer("upload", size);
                    StatusCode::OK.into_response()
                }
                Err(keel_core::Error::Invalid(msg)) => fail(StatusCode::INSUFFICIENT_STORAGE, &msg),
                Err(e) => internal(e),
            }
        }
        Ok(Err(res)) => res,
        Err(e) => internal(e),
    }
}

/// Writes an upload to a temporary file while hashing it. Returns the file
/// (for `lfs_store` to move into place) and its size, or the response for a
/// bad upload. `room` is how much more the repository's owner may store, if
/// limited.
async fn receive(state: &AppState, oid: &str, body: Body, room: Option<u64>) -> std::io::Result<Result<(std::path::PathBuf, u64), Response>> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let tmp_dir = state.core.lfs_tmp_dir();
    tokio::fs::create_dir_all(&tmp_dir).await?;
    let tmp = tmp_dir.join(format!("{}-{}-{}", std::process::id(), crate::now(), NEXT.fetch_add(1, Ordering::Relaxed)));
    let max = state.core.config.lfs_max_bytes;
    let result: std::io::Result<Result<u64, Response>> = async {
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
            if room.is_some_and(|r| size > r) {
                return Ok(Err(fail(StatusCode::INSUFFICIENT_STORAGE, "The repository's owner is out of storage on this server")));
            }
            hasher.update(&chunk);
            file.write_all(&chunk).await?;
        }
        file.sync_all().await?;
        if hex::encode(hasher.finalize()) != oid {
            return Ok(Err(fail(StatusCode::UNPROCESSABLE_ENTITY, "The content doesn't match the object id")));
        }
        Ok(Ok(size))
    }
    .await;
    if !matches!(result, Ok(Ok(_))) {
        let _ = tokio::fs::remove_file(&tmp).await;
    }
    Ok(result?.map(|size| (tmp, size)))
}

// ---------------------------------------------------------------------------
// File locking (https://github.com/git-lfs/git-lfs/blob/main/docs/api/locking.md)
// ---------------------------------------------------------------------------

const LOCK_PAGE: i64 = 100;
const DEPLOY_KEY_LOCKS: &str = "Deploy keys can't lock files; locks belong to people";

fn lock_json(l: &LfsLock) -> Value {
    json!({
        "id": l.id.to_string(),
        "path": l.path,
        "locked_at": keel_core::json_time(l.created_at),
        "owner": { "name": l.owner },
    })
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct RefSpec {
    name: String,
}

#[derive(Deserialize)]
struct NewLock {
    path: String,
    #[serde(default, rename = "ref")]
    ref_spec: RefSpec,
}

/// `POST …/info/lfs/locks`: locks a file.
pub async fn create_lock(
    State(state): State<AppState>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let (repo, user) = match authorize(&state, &headers, &owner, &name, true).await {
        Ok(r) => r,
        Err(res) => return res,
    };
    let Some(user) = user else { return fail(StatusCode::FORBIDDEN, DEPLOY_KEY_LOCKS) };
    let Ok(req) = serde_json::from_slice::<NewLock>(&body) else {
        return fail(StatusCode::UNPROCESSABLE_ENTITY, "Invalid lock request");
    };
    match state.core.create_lfs_lock(repo.id, &user, &req.path, &req.ref_spec.name).await {
        Ok(Ok(lock)) => reply(StatusCode::CREATED, json!({ "lock": lock_json(&lock) })),
        Ok(Err(existing)) => reply(StatusCode::CONFLICT, json!({ "lock": lock_json(&existing), "message": "already created lock" })),
        Err(keel_core::Error::Invalid(msg)) => fail(StatusCode::UNPROCESSABLE_ENTITY, &msg),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
pub struct LockQuery {
    path: Option<String>,
    id: Option<String>,
    cursor: Option<String>,
    limit: Option<String>,
}

fn page_params(cursor: Option<&str>, limit: Option<&str>) -> (i64, i64) {
    let after = cursor.and_then(|c| c.parse().ok()).unwrap_or(0);
    let limit = limit.and_then(|l| l.parse().ok()).unwrap_or(LOCK_PAGE).clamp(1, LOCK_PAGE);
    (after, limit)
}

/// `GET …/info/lfs/locks`: lists locks.
pub async fn list_locks(
    State(state): State<AppState>,
    Path((owner, name)): Path<(String, String)>,
    axum::extract::Query(q): axum::extract::Query<LockQuery>,
    headers: HeaderMap,
) -> Response {
    let (repo, _) = match authorize(&state, &headers, &owner, &name, false).await {
        Ok(r) => r,
        Err(res) => return res,
    };
    let (after, limit) = page_params(q.cursor.as_deref(), q.limit.as_deref());
    let id = match q.id.as_deref().filter(|i| !i.is_empty()) {
        Some(i) => match i.parse() {
            Ok(i) => Some(i),
            Err(_) => return reply(StatusCode::OK, json!({ "locks": [] })),
        },
        None => None,
    };
    let path = q.path.as_deref().filter(|p| !p.is_empty()).map(|p| p.trim_start_matches('/'));
    match state.core.lfs_locks(repo.id, path, id, after, limit + 1).await {
        Ok(mut locks) => {
            let next = (locks.len() as i64 > limit).then(|| locks[limit as usize - 1].id.to_string());
            locks.truncate(limit as usize);
            let mut body = json!({ "locks": locks.iter().map(lock_json).collect::<Vec<_>>() });
            if let Some(n) = next {
                body["next_cursor"] = json!(n);
            }
            reply(StatusCode::OK, body)
        }
        Err(e) => internal(e),
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct VerifyRequest {
    cursor: Option<String>,
    limit: Option<i64>,
}

/// `POST …/info/lfs/locks/verify`: the pusher's locks and everyone
/// else's, so the client can refuse to push changes to others' files.
pub async fn verify_locks(
    State(state): State<AppState>,
    Path((owner, name)): Path<(String, String)>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let (repo, user) = match authorize(&state, &headers, &owner, &name, true).await {
        Ok(r) => r,
        Err(res) => return res,
    };
    // A deploy key holds no locks: every lock is someone else's.
    let req: VerifyRequest = serde_json::from_slice(&body).unwrap_or_default();
    let limit = req.limit.map(|l| l.to_string());
    let (after, limit) = page_params(req.cursor.as_deref(), limit.as_deref());
    match state.core.lfs_locks(repo.id, None, None, after, limit + 1).await {
        Ok(mut locks) => {
            let next = (locks.len() as i64 > limit).then(|| locks[limit as usize - 1].id.to_string());
            locks.truncate(limit as usize);
            let (ours, theirs): (Vec<_>, Vec<_>) = locks.iter().partition(|l| user.as_ref().is_some_and(|u| l.owner_id == u.id));
            let mut body = json!({
                "ours": ours.into_iter().map(lock_json).collect::<Vec<_>>(),
                "theirs": theirs.into_iter().map(lock_json).collect::<Vec<_>>(),
            });
            if let Some(n) = next {
                body["next_cursor"] = json!(n);
            }
            reply(StatusCode::OK, body)
        }
        Err(e) => internal(e),
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct UnlockRequest {
    force: bool,
}

/// `POST …/info/lfs/locks/<id>/unlock`: your own lock, or (with `force`,
/// for repository admins) anyone's.
pub async fn unlock(
    State(state): State<AppState>,
    Path((owner, name, id)): Path<(String, String, String)>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let (repo, user) = match authorize(&state, &headers, &owner, &name, true).await {
        Ok(r) => r,
        Err(res) => return res,
    };
    let Some(user) = user else { return fail(StatusCode::FORBIDDEN, DEPLOY_KEY_LOCKS) };
    let req: UnlockRequest = serde_json::from_slice(&body).unwrap_or_default();
    let Ok(id) = id.parse::<i64>() else { return fail(StatusCode::NOT_FOUND, "Lock not found") };
    let lock = match state.core.lfs_locks(repo.id, None, Some(id), 0, 1).await {
        Ok(l) => l.into_iter().next(),
        Err(e) => return internal(e),
    };
    let Some(lock) = lock else { return fail(StatusCode::NOT_FOUND, "Lock not found") };
    if lock.owner_id != user.id {
        if !req.force {
            return fail(StatusCode::FORBIDDEN, &format!("{} holds this lock; only they can unlock it (or an admin, with --force)", lock.owner));
        }
        match state.core.access(&repo, Some(&user)).await {
            Ok(a) if a >= Access::Admin => {}
            Ok(_) => return fail(StatusCode::FORBIDDEN, "Only repository admins can break others' locks"),
            Err(e) => return internal(e),
        }
    }
    match state.core.delete_lfs_lock(repo.id, lock.id).await {
        Ok(()) => reply(StatusCode::OK, json!({ "lock": lock_json(&lock) })),
        Err(e) => internal(e),
    }
}
