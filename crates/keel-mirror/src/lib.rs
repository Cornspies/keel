//! Keeps mirrors in sync: pulls into pull mirrors on their schedule, and
//! pushes to push mirrors after changes, with their Git LFS files both ways.
//! Also imports repositories from GitHub and Gitea.
//!
//! git does the fetching and pushing (see `keel_core::mirrors`); this crate
//! schedules the work and is the LFS client, through the same guarded HTTP
//! client as webhooks.

mod import;
mod lfs;

use std::time::Duration;

use keel_core::Core;

pub use lfs::endpoint;

/// The HTTP client for LFS transfers: no proxies, no private addresses
/// (unless allowed), a few checked redirects (storage services use them),
/// and generous timeouts for large files.
pub fn client(outbound: &keel_core::Outbound) -> reqwest::Client {
    keel_hooks::safe_client_builder(outbound)
        .redirect(keel_hooks::checked_redirects(outbound, 5))
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(3600))
        .build()
        .expect("valid HTTP client configuration")
}

fn message(e: keel_core::Error) -> String {
    match e {
        keel_core::Error::Invalid(m) => m,
        e => e.to_string(),
    }
}

/// Starts the workers (needs a Tokio runtime).
pub fn start(core: Core) {
    let client = client(&core.config.outbound());
    start_imports(core.clone(), client.clone());
    tokio::spawn(async move {
        let wakeup = core.mirror_wakeup();
        loop {
            let mut busy = false;
            match core.claim_mirror().await {
                Ok(Some(repo_id)) => {
                    pull(&core, &client, repo_id).await;
                    busy = true;
                }
                Ok(None) => {}
                Err(e) => tracing::warn!("mirror queue: {e}"),
            }
            match core.claim_push_mirror().await {
                Ok(Some((id, repo_id))) => {
                    push(&core, &client, id, repo_id).await;
                    busy = true;
                }
                Ok(None) => {}
                Err(e) => tracing::warn!("push mirror queue: {e}"),
            }
            if busy {
                continue;
            }
            let wait = match core.next_mirror_work().await {
                Ok(Some(t)) => (t - now()).clamp(1, 300) as u64,
                _ => 300,
            };
            tokio::select! {
                _ = wakeup.notified() => {}
                _ = tokio::time::sleep(Duration::from_secs(wait)) => {}
            }
        }
    });
}

/// Imports one at a time (they can take a while), apart from mirrors.
fn start_imports(core: Core, client: reqwest::Client) {
    tokio::spawn(async move {
        if let Err(e) = core.fail_interrupted_imports().await {
            tracing::warn!("imports: {e}");
        }
        let wakeup = core.import_wakeup();
        loop {
            match core.claim_import().await {
                Ok(Some(repo_id)) => {
                    import::run(&core, &client, repo_id).await;
                    continue;
                }
                Ok(None) => {}
                Err(e) => tracing::warn!("import queue: {e}"),
            }
            tokio::select! {
                _ = wakeup.notified() => {}
                _ = tokio::time::sleep(Duration::from_secs(300)) => {}
            }
        }
    });
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

/// Syncs a pull mirror: git, then the LFS files the new commits need.
pub async fn pull(core: &Core, client: &reqwest::Client, repo_id: i64) {
    let result = match core.fetch_mirror(repo_id).await {
        Ok(None) => return,
        Ok(Some((remote, wanted))) => {
            let (missing, error) = if wanted.is_empty() {
                (Vec::new(), None)
            } else {
                lfs::download(core, client, repo_id, &remote, &wanted).await
            };
            let error = error.map(|e| format!("Git LFS: {e}"));
            core.finish_mirror_sync(repo_id, Some(&missing), error).await
        }
        Err(e) => core.finish_mirror_sync(repo_id, None, Some(message(e))).await,
    };
    if let Err(e) = result {
        tracing::warn!(repo = repo_id, "recording mirror sync: {e}");
    }
}

/// Pushes to a push mirror: LFS files first (so the commits pointing to
/// them never arrive without them), then git.
pub async fn push(core: &Core, client: &reqwest::Client, id: i64, repo_id: i64) {
    let Ok(Some(remote)) = core.push_mirror_remote(id).await else { return };
    let result: Result<(), String> = async {
        let files = core.lfs_files_to_push(repo_id, &remote).await.map_err(message)?;
        if !files.is_empty() {
            lfs::upload(core, client, &remote, &files).await.map_err(|e| format!("Git LFS: {e}"))?;
        }
        core.push_to_mirror(repo_id, &remote).await.map_err(message)
    }
    .await;
    if let Err(e) = core.finish_push(id, result.err()).await {
        tracing::warn!(push_mirror = id, "recording push: {e}");
    }
}
