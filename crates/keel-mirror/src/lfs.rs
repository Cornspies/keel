//! A Git LFS client (batch API, basic transfers), for mirrors.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use futures_util::StreamExt;
use keel_core::{Core, MirrorRemote};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

const LFS_JSON: &str = "application/vnd.git-lfs+json";
/// Objects per batch request.
const BATCH: usize = 100;

/// Where the LFS server of the repository at `url` is, by the convention
/// LFS clients use.
pub fn endpoint(url: &str) -> String {
    let url = url.trim_end_matches('/');
    if url.ends_with(".git") { format!("{url}/info/lfs") } else { format!("{url}.git/info/lfs") }
}

pub(crate) use keel_hooks::check_url;

#[derive(Deserialize)]
struct BatchResponse {
    #[serde(default)]
    objects: Vec<BatchObject>,
}

#[derive(Deserialize)]
struct BatchObject {
    oid: String,
    #[serde(default)]
    actions: Option<Actions>,
    #[serde(default)]
    error: Option<ObjectError>,
}

#[derive(Deserialize)]
struct Actions {
    download: Option<Action>,
    upload: Option<Action>,
    verify: Option<Action>,
}

#[derive(Deserialize)]
struct Action {
    href: String,
    #[serde(default)]
    header: HashMap<String, String>,
}

#[derive(Deserialize)]
struct ObjectError {
    message: String,
}

/// Whether two URLs have the same scheme, host and port, so credentials for
/// one may go to the other.
pub(crate) fn same_origin(a: &reqwest::Url, b: &reqwest::Url) -> bool {
    a.scheme() == b.scheme() && a.host_str() == b.host_str() && a.port_or_known_default() == b.port_or_known_default()
}

/// A request to `url` with the action's headers, and the remote's
/// credentials if the action brings none and it's the remote's origin
/// (not, say, plain HTTP on the same host).
fn request(client: &reqwest::Client, core: &Core, method: reqwest::Method, remote: &MirrorRemote, action: &Action) -> Result<reqwest::RequestBuilder, String> {
    let url = reqwest::Url::parse(&action.href).map_err(|_| "the server sent an invalid URL".to_owned())?;
    check_url(&url, &core.config.outbound())?;
    let same_host = reqwest::Url::parse(&remote.url).is_ok_and(|r| same_origin(&r, &url));
    let mut req = client.request(method, url);
    let mut has_auth = false;
    for (k, v) in &action.header {
        has_auth |= k.eq_ignore_ascii_case("authorization");
        req = req.header(k, v);
    }
    if !has_auth && same_host && (!remote.username.is_empty() || !remote.password.is_empty()) {
        req = req.basic_auth(&remote.username, Some(&remote.password));
    }
    Ok(req)
}

async fn batch(client: &reqwest::Client, core: &Core, remote: &MirrorRemote, operation: &str, objects: &[(String, u64)]) -> Result<Vec<BatchObject>, String> {
    let url = reqwest::Url::parse(&format!("{}/objects/batch", endpoint(&remote.url))).map_err(|e| e.to_string())?;
    check_url(&url, &core.config.outbound())?;
    let body = json!({
        "operation": operation,
        "transfers": ["basic"],
        "objects": objects.iter().map(|(oid, size)| json!({ "oid": oid, "size": size })).collect::<Vec<_>>(),
    });
    let mut req = client
        .post(url)
        .header("Accept", LFS_JSON)
        .header("Content-Type", LFS_JSON)
        .body(body.to_string());
    if !remote.username.is_empty() || !remote.password.is_empty() {
        req = req.basic_auth(&remote.username, Some(&remote.password));
    }
    let res = req.send().await.map_err(|e| format!("batch request failed: {e}"))?;
    let status = res.status();
    let text = res.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        let msg = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v["message"].as_str().map(str::to_owned))
            .unwrap_or_else(|| status.to_string());
        return Err(format!("the server said: {msg}"));
    }
    serde_json::from_str::<BatchResponse>(&text)
        .map(|r| r.objects)
        .map_err(|_| "the server's batch response isn't valid".to_owned())
}

/// Downloads `wanted` objects into the store for `repo_id`. Returns those
/// still missing, and the first problem.
pub async fn download(
    core: &Core,
    client: &reqwest::Client,
    repo_id: i64,
    remote: &MirrorRemote,
    wanted: &[(String, u64)],
) -> (Vec<(String, u64)>, Option<String>) {
    let mut missing = Vec::new();
    let mut problem: Option<String> = None;
    let note = |p: String, problem: &mut Option<String>| {
        problem.get_or_insert(p);
    };
    for chunk in wanted.chunks(BATCH) {
        let objects = match batch(client, core, remote, "download", chunk).await {
            Ok(o) => o,
            Err(e) => {
                missing.extend_from_slice(chunk);
                note(e, &mut problem);
                continue;
            }
        };
        for (oid, size) in chunk {
            let found = objects.iter().find(|o| &o.oid == oid);
            let result = match found {
                Some(BatchObject { error: Some(e), .. }) => Err(format!("{oid}: {}", e.message)),
                Some(BatchObject { actions: Some(Actions { download: Some(a), .. }), .. }) => {
                    fetch(core, client, repo_id, remote, a, oid, *size).await
                }
                _ => Err(format!("the server didn't offer {oid}")),
            };
            if let Err(e) = result {
                missing.push((oid.clone(), *size));
                note(e, &mut problem);
            }
        }
    }
    if let Some(p) = problem.as_mut()
        && missing.len() > 1
    {
        *p = format!("{} files couldn't be downloaded; first: {p}", missing.len());
    }
    (missing, problem)
}

/// Downloads one object, checking its size and content as it streams in.
async fn fetch(core: &Core, client: &reqwest::Client, repo_id: i64, remote: &MirrorRemote, action: &Action, oid: &str, size: u64) -> Result<(), String> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let max = core.config.lfs_max_bytes;
    if size > max {
        return Err(format!("{oid} is larger than this server's limit of {} MiB", max >> 20));
    }
    core.check_repo_storage(repo_id, size).await.map_err(|e| e.to_string())?;
    let res = request(client, core, reqwest::Method::GET, remote, action)?
        .send()
        .await
        .map_err(|e| format!("downloading {oid}: {e}"))?;
    if !res.status().is_success() {
        return Err(format!("downloading {oid}: {}", res.status()));
    }
    let tmp_dir = core.lfs_tmp_dir();
    tokio::fs::create_dir_all(&tmp_dir).await.map_err(|e| e.to_string())?;
    let tmp = tmp_dir.join(format!("mirror-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    let result = async {
        let mut file = tokio::fs::File::create(&tmp).await.map_err(|e| e.to_string())?;
        let (mut hasher, mut got) = (Sha256::new(), 0u64);
        let mut stream = res.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|e| format!("downloading {oid}: {e}"))?;
            got += chunk.len() as u64;
            // The announced size was checked against the limit.
            if got > size {
                return Err(format!("{oid} is larger than announced"));
            }
            hasher.update(&chunk);
            file.write_all(&chunk).await.map_err(|e| e.to_string())?;
        }
        file.sync_all().await.map_err(|e| e.to_string())?;
        if hex::encode(hasher.finalize()) != oid {
            return Err(format!("{oid} arrived with different content"));
        }
        core.lfs_store(repo_id, &tmp, oid, got).await.map_err(|e| e.to_string())?;
        core.metrics().lfs_transfer("mirror_download", got);
        Ok(())
    }
    .await;
    let _ = tokio::fs::remove_file(&tmp).await;
    result
}

/// Uploads stored `files` to the remote's LFS server (skipping those it
/// has).
pub async fn upload(core: &Core, client: &reqwest::Client, remote: &MirrorRemote, files: &[(String, u64)]) -> Result<(), String> {
    for chunk in files.chunks(BATCH) {
        let objects = batch(client, core, remote, "upload", chunk).await?;
        for obj in objects {
            if let Some(e) = obj.error {
                return Err(format!("{}: {}", obj.oid, e.message));
            }
            let Some(actions) = obj.actions else { continue }; // it has it
            let Some((_, size)) = chunk.iter().find(|(oid, _)| *oid == obj.oid) else { continue };
            if let Some(up) = &actions.upload {
                let file = tokio::fs::File::open(core.lfs_path(&obj.oid)).await.map_err(|e| e.to_string())?;
                let body = reqwest::Body::wrap_stream(tokio_util::io::ReaderStream::new(file));
                let res = request(client, core, reqwest::Method::PUT, remote, up)?
                    .header("Content-Type", "application/octet-stream")
                    .header("Content-Length", size.to_string())
                    .body(body)
                    .send()
                    .await
                    .map_err(|e| format!("uploading {}: {e}", obj.oid))?;
                if !res.status().is_success() {
                    return Err(format!("uploading {}: {}", obj.oid, res.status()));
                }
                core.metrics().lfs_transfer("mirror_upload", *size);
            }
            if let Some(verify) = &actions.verify {
                let res = request(client, core, reqwest::Method::POST, remote, verify)?
                    .header("Content-Type", LFS_JSON)
                    .header("Accept", LFS_JSON)
                    .body(json!({ "oid": obj.oid, "size": size }).to_string())
                    .send()
                    .await
                    .map_err(|e| format!("verifying {}: {e}", obj.oid))?;
                if !res.status().is_success() {
                    return Err(format!("verifying {}: {}", obj.oid, res.status()));
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_core::Outbound;

    #[test]
    fn endpoints_and_urls() {
        assert_eq!(endpoint("https://github.com/a/b.git"), "https://github.com/a/b.git/info/lfs");
        assert_eq!(endpoint("https://github.com/a/b"), "https://github.com/a/b.git/info/lfs");
        assert_eq!(endpoint("https://github.com/a/b/"), "https://github.com/a/b.git/info/lfs");
        let url = |s: &str| reqwest::Url::parse(s).unwrap();
        let (strict, open) = (Outbound::default(), Outbound { allow_private: true, ..Outbound::default() });
        assert!(check_url(&url("https://media.example.com/x"), &strict).is_ok());
        assert!(check_url(&url("http://127.0.0.1/x"), &strict).is_err());
        assert!(check_url(&url("http://[::1]/x"), &strict).is_err());
        assert!(check_url(&url("http://127.0.0.1/x"), &open).is_ok());
        assert!(check_url(&url("file:///etc/passwd"), &open).is_err());
        let blocked = Outbound { blocked: vec![keel_core::Network::parse("203.0.113.0/24").unwrap()], ..open };
        assert!(check_url(&url("http://203.0.113.7/x"), &blocked).is_err());
        assert!(check_url(&url("http://198.51.100.7/x"), &blocked).is_ok());

        assert!(same_origin(&url("https://git.example/a"), &url("https://git.example:443/b")));
        assert!(!same_origin(&url("https://git.example/a"), &url("http://git.example/a")), "plain HTTP");
        assert!(!same_origin(&url("https://git.example/a"), &url("https://git.example:8443/a")));
        assert!(!same_origin(&url("https://git.example/a"), &url("https://media.example/a")));
    }
}
