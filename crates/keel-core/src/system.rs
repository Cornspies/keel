//! How much space the data directory takes, by kind, for the administrators'
//! System page.

use std::collections::HashSet;
use std::path::Path;

use crate::{Core, Result};

/// Bytes on disk under the data directory, by what they hold.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StorageUsage {
    /// `keel.db` with its write-ahead log.
    pub database: u64,
    pub repositories: u64,
    pub wikis: u64,
    /// Git LFS files.
    pub lfs: u64,
    pub releases: u64,
    pub avatars: u64,
    /// The code search index (rebuilt if deleted).
    pub search: u64,
    /// Keys, hooks and anything else.
    pub other: u64,
    /// The filesystem the data directory is on: (free, size), if known.
    pub disk: Option<(u64, u64)>,
    /// Where that filesystem is mounted and its type (Linux), e.g.
    /// `("/tmp", "tmpfs")`.
    pub mount: Option<(String, String)>,
}

impl StorageUsage {
    /// The data directory is in memory (tmpfs, ramfs): gone after a restart
    /// of the machine.
    pub fn in_memory(&self) -> bool {
        self.mount.as_ref().is_some_and(|(_, kind)| matches!(kind.as_str(), "tmpfs" | "ramfs"))
    }
}

impl StorageUsage {
    pub fn total(&self) -> u64 {
        self.database + self.repositories + self.wikis + self.lfs + self.releases + self.avatars + self.search + self.other
    }
}

impl Core {
    /// Measures the data directory. Walks every file, so it takes a while
    /// on a large server; callers cache it.
    pub async fn storage_usage(&self) -> Result<StorageUsage> {
        let dir = self.config.data_dir.clone();
        Ok(tokio::task::spawn_blocking(move || measure(&dir)).await.map_err(anyhow::Error::from)?)
    }
}

fn measure(data_dir: &Path) -> StorageUsage {
    let mut usage = StorageUsage { disk: disk_space(data_dir), mount: mount_of(data_dir), ..StorageUsage::default() };
    // Forks share objects with their originals through hard links: each
    // file counts once.
    let mut seen = HashSet::new();
    let Ok(entries) = std::fs::read_dir(data_dir) else { return usage };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let path = entry.path();
        if name == "repos" {
            for repo in std::fs::read_dir(&path).into_iter().flatten().flatten() {
                let size = disk_usage(&repo.path(), &mut seen);
                if repo.file_name().to_string_lossy().ends_with(".wiki.git") {
                    usage.wikis += size;
                } else {
                    usage.repositories += size;
                }
            }
            continue;
        }
        let size = disk_usage(&path, &mut seen);
        let slot = match name.as_str() {
            n if n.starts_with("keel.db") => &mut usage.database,
            "lfs" => &mut usage.lfs,
            "releases" => &mut usage.releases,
            "avatars" => &mut usage.avatars,
            "search" => &mut usage.search,
            _ => &mut usage.other,
        };
        *slot += size;
    }
    usage
}

/// Space the files under `path` take on disk. Symbolic links aren't
/// followed, and hard-linked files already in `seen` aren't counted again.
pub(crate) fn disk_usage(path: &Path, seen: &mut HashSet<(u64, u64)>) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else { return 0 };
    if meta.is_dir() {
        return std::fs::read_dir(path)
            .map(|entries| entries.flatten().map(|e| disk_usage(&e.path(), seen)).sum())
            .unwrap_or(0);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if meta.nlink() > 1 && !seen.insert((meta.dev(), meta.ino())) {
            return 0;
        }
        // Allocated blocks, as `du` counts them.
        meta.blocks() * 512
    }
    #[cfg(not(unix))]
    {
        let _ = seen;
        meta.len()
    }
}

/// The mount point and filesystem type `path` is on, from
/// `/proc/self/mountinfo`: the longest mount point it's under.
fn mount_of(path: &Path) -> Option<(String, String)> {
    let info = std::fs::read_to_string("/proc/self/mountinfo").ok()?;
    let path = path.canonicalize().ok()?;
    info.lines()
        .filter_map(|line| {
            // `<id> <parent> <dev> <root> <mount point> <options> [optional…] - <type> <source> …`
            let (left, right) = line.split_once(" - ")?;
            let point = unescape_mount(left.split(' ').nth(4)?);
            let kind = right.split(' ').next()?.to_owned();
            path.starts_with(&point).then_some((point, kind))
        })
        .max_by_key(|(point, _)| point.len())
}

/// Mount points escape spaces and the like as `\040`.
fn unescape_mount(s: &str) -> String {
    let mut out = Vec::new();
    let b = s.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'\\' && i + 3 < b.len() && b[i + 1..i + 4].iter().all(|c| (b'0'..=b'7').contains(c)) {
            out.push((b[i + 1] - b'0') * 64 + (b[i + 2] - b'0') * 8 + (b[i + 3] - b'0'));
            i += 4;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Free and total bytes of the filesystem `path` is on.
fn disk_space(path: &Path) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
        let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
        // SAFETY: `c` is a valid C string and `stat` a writable statvfs.
        if unsafe { libc::statvfs(c.as_ptr(), &mut stat) } != 0 {
            return None;
        }
        let unit = stat.f_frsize as u64;
        Some((stat.f_bavail as u64 * unit, stat.f_blocks as u64 * unit))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn measures_by_kind() {
        let (core, dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let repo = core.create_repo(&ada, "engine", "", false).await.unwrap();
        std::fs::create_dir_all(dir.path().join("lfs/ab")).unwrap();
        std::fs::write(dir.path().join("lfs/ab/big"), vec![1u8; 64 * 1024]).unwrap();
        let before = core.storage_usage().await.unwrap();
        assert!(before.database > 0 && before.repositories > 0, "{before:?}");
        assert!(before.lfs >= 64 * 1024);
        assert_eq!(before.wikis, 0);
        assert_eq!(before.total(), before.database + before.repositories + before.lfs + before.other + before.search
            + before.releases + before.avatars);
        #[cfg(unix)]
        {
            let (free, size) = before.disk.unwrap();
            assert!(free <= size && size > 0);
            // A hard link takes no more space.
            std::fs::hard_link(dir.path().join("lfs/ab/big"), dir.path().join("lfs/ab/again")).unwrap();
            assert_eq!(core.storage_usage().await.unwrap().lfs, before.lfs);
        }
        #[cfg(target_os = "linux")]
        assert!(before.mount.is_some(), "the data directory is on some mount");
        assert_eq!(unescape_mount("/mnt/my\\040disk"), "/mnt/my disk");
        let page = crate::WikiEdit { page: None, title: "Home", text: "Hi", message: "", base: None };
        core.save_wiki_page(&repo, &ada, page).await.unwrap();
        assert!(core.storage_usage().await.unwrap().wikis > 0);
    }
}
