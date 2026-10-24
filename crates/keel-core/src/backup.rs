//! `keel backup`: a consistent copy of a data directory, taken while the
//! server keeps running. The copy is itself a data directory: to restore,
//! point `--data-dir` at it.
//!
//! The database is copied first, with SQLite's `VACUUM INTO` (a consistent
//! snapshot, even while the server writes). The repositories are copied
//! after it (with their wikis), so every commit the database mentions is in
//! them. Large files (Git LFS) are immutable and copied as they are.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use sqlx::Connection;
use sqlx::sqlite::{SqliteConnectOptions, SqliteConnection};

/// What a backup copied.
#[derive(Debug)]
pub struct BackupReport {
    pub repos: usize,
    pub wikis: usize,
    /// Repositories deleted between the database snapshot and copying.
    pub skipped: usize,
    pub lfs_objects: usize,
    pub release_files: usize,
    pub bytes: u64,
    pub took: Duration,
}

/// Copies the data directory `data_dir` to `out`, which must not exist or
/// be empty.
pub async fn backup(data_dir: &Path, out: &Path) -> anyhow::Result<BackupReport> {
    let started = Instant::now();
    let db = data_dir.join("keel.db");
    if !db.is_file() {
        bail!("{} has no keel.db; is it a keel data directory?", data_dir.display());
    }
    if out.exists() && std::fs::read_dir(out)?.next().is_some() {
        bail!("{} isn't empty", out.display());
    }
    std::fs::create_dir_all(out.join("repos"))?;

    // 1. The database.
    let snapshot = out.join("keel.db");
    let mut conn = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&db).read_only(true))
        .await
        .context("opening the database")?;
    sqlx::query("VACUUM INTO ?1")
        .bind(snapshot.to_string_lossy().into_owned())
        .execute(&mut conn)
        .await
        .context("copying the database")?;
    conn.close().await?;

    // 2. The repositories it knows.
    let mut copy = SqliteConnection::connect_with(&SqliteConnectOptions::new().filename(&snapshot).read_only(true)).await?;
    let ids: Vec<(i64,)> = sqlx::query_as("SELECT id FROM repositories ORDER BY id").fetch_all(&mut copy).await?;
    copy.close().await?;
    let (mut repos, mut wikis, mut skipped) = (0, 0, 0);
    for (id,) in ids {
        let (src, dst) = (data_dir.join("repos").join(format!("{id}.git")), out.join("repos").join(format!("{id}.git")));
        if !src.exists() {
            skipped += 1;
            continue;
        }
        tokio::task::spawn_blocking(move || keel_git::backup(&src, &dst))
            .await?
            .with_context(|| format!("copying repository {id}"))?;
        repos += 1;
        let wiki = format!("{id}.wiki.git");
        let (src, dst) = (data_dir.join("repos").join(&wiki), out.join("repos").join(&wiki));
        if src.join("HEAD").is_file() {
            tokio::task::spawn_blocking(move || keel_git::backup(&src, &dst))
                .await?
                .with_context(|| format!("copying the wiki of repository {id}"))?;
            wikis += 1;
        }
    }

    // 3. The SSH host key (so clients don't see a changed key after a
    // restore) and large files.
    let key = data_dir.join("ssh_host_ed25519_key");
    if key.is_file() {
        std::fs::copy(&key, out.join("ssh_host_ed25519_key"))?;
    }
    let lfs_objects = copy_files(&data_dir.join("lfs"), &out.join("lfs"))?;
    let release_files = copy_files(&data_dir.join("releases"), &out.join("releases"))?;
    copy_files(&data_dir.join("avatars"), &out.join("avatars"))?;

    Ok(BackupReport { repos, wikis, skipped, lfs_objects, release_files, bytes: dir_size(out)?, took: started.elapsed() })
}

/// Copies stored files (not uploads in progress, which are in `tmp` or
/// named `upload-…`). Returns how many.
fn copy_files(src: &Path, dst: &Path) -> anyhow::Result<usize> {
    if !src.is_dir() {
        return Ok(0);
    }
    let mut n = 0;
    let mut stack: Vec<PathBuf> = vec![src.to_owned()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let rel = path.strip_prefix(src)?;
            if rel.starts_with("tmp") || entry.file_name().to_string_lossy().starts_with("upload-") {
                continue;
            }
            if entry.file_type()?.is_dir() {
                std::fs::create_dir_all(dst.join(rel))?;
                stack.push(path);
            } else {
                std::fs::create_dir_all(dst.join(rel).parent().expect("has a parent"))?;
                std::fs::copy(&path, dst.join(rel))?;
                n += 1;
            }
        }
    }
    Ok(n)
}

fn dir_size(dir: &Path) -> std::io::Result<u64> {
    let mut total = 0;
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let meta = entry.metadata()?;
        total += if meta.is_dir() { dir_size(&entry.path())? } else { meta.len() };
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Core;

    #[tokio::test]
    async fn backup_and_restore() {
        let (core, dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let repo = core.create_repo(&ada, "engine", "", false).await.unwrap();
        let gone = core.create_repo(&ada, "gone", "", false).await.unwrap();
        core.create_issue(repo.id, &ada, "Kept", "").await.unwrap();
        let page = crate::WikiEdit { page: None, title: "Home", text: "Hi", message: "", base: None };
        core.save_wiki_page(&repo, &ada, page).await.unwrap();
        // Simulate a repository deleted after the snapshot.
        std::fs::remove_dir_all(core.repo_path(&gone)).unwrap();
        std::fs::create_dir_all(dir.path().join("lfs/ab/cd")).unwrap();
        std::fs::write(dir.path().join("lfs/ab/cd/abcd1234"), "big").unwrap();
        std::fs::create_dir_all(dir.path().join("lfs/tmp")).unwrap();
        std::fs::write(dir.path().join("lfs/tmp/upload"), "partial").unwrap();

        let out = tempfile::tempdir().unwrap();
        let target = out.path().join("backup");
        let report = backup(dir.path(), &target).await.unwrap();
        assert_eq!((report.repos, report.wikis, report.skipped, report.lfs_objects), (1, 1, 1, 1));
        assert!(report.bytes > 0);
        assert!(backup(dir.path(), &target).await.is_err(), "won't overwrite");
        assert!(!target.join("lfs/tmp").exists());

        // The copy is a working data directory.
        let restored = Core::open(crate::Config { data_dir: target.clone(), ..crate::Config::default() }).await.unwrap();
        let r = restored.repo("ada", "engine").await.unwrap().unwrap();
        assert_eq!(restored.issue(r.id, 1).await.unwrap().unwrap().title, "Kept");
        assert!(keel_git::GitRepo::open(&restored.repo_path(&r)).is_ok());
        assert!(restored.authenticate("ada", "password1").await.unwrap().is_some());
        assert_eq!(restored.wiki_page(r.id, "Home", None).await.unwrap().unwrap().text, "Hi");
    }
}
