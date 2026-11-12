use std::path::Path;

use crate::{Error, GitRepo, Result};

/// Config written into every repository keel creates.
const REPO_CONFIG: &[(&str, &str)] = &[
    // Let clients do partial clones (`--filter=blob:none`).
    ("uploadpack.allowFilter", "true"),
    ("uploadpack.allowReachableSHA1InWant", "true"),
    // Use the commit-graph written by `run_maintenance` for fast history walks.
    ("core.commitGraph", "true"),
    // Writing bitmaps on repack makes clones much cheaper to serve.
    ("repack.writeBitmaps", "true"),
    ("pack.writeBitmapHashCache", "true"),
    // The server, not a background `git gc` spawned by a push, decides when
    // to do maintenance.
    ("gc.auto", "0"),
    ("receive.autogc", "false"),
    // Refuse malformed objects, and the path and `.gitmodules` tricks behind
    // git client vulnerabilities, in pushes and in what mirrors and imports
    // fetch, so nobody can serve them to people cloning from here.
    ("receive.fsckObjects", "true"),
    ("fetch.fsckObjects", "true"),
    ("transfer.fsckObjects", "true"),
];

/// Config keys with several values, all of which every repository keel
/// creates has.
const REPO_MULTI_CONFIG: &[(&str, &str)] = &[
    // Pull request refs are maintained by the server; pushes can't touch them.
    ("receive.hideRefs", "refs/pull"),
    // Replace refs would make git commands see other commits than libgit2
    // does; keel ignores them (see `crate::command`), and nobody can push
    // or fetch them.
    ("receive.hideRefs", "refs/replace"),
    ("uploadpack.hideRefs", "refs/replace"),
];

/// Creates a bare repository whose HEAD points at `default_branch`.
pub fn create_bare(path: &Path, default_branch: &str) -> Result<()> {
    if path.exists() {
        return Err(Error::Other(format!("{} already exists", path.display())));
    }
    let repo = git2::Repository::init_opts(
        path,
        git2::RepositoryInitOptions::new()
            .bare(true)
            .no_reinit(true)
            .initial_head(default_branch),
    )?;
    drop(repo);
    apply_config(path)?;
    // libgit2 creates sample hooks; keel never runs repository hooks.
    let _ = std::fs::remove_dir_all(path.join("hooks"));
    Ok(())
}

/// Creates `dst` as a fork of the bare repository `src`: its branches,
/// tags and HEAD, but not keel's pull request refs. Objects are hardlinked
/// rather than copied, so forking is fast and costs almost no disk space
/// even for large repositories, and the fork stays independent of `src`.
pub fn fork(src: &Path, dst: &Path) -> Result<()> {
    if dst.exists() {
        return Err(Error::Other(format!("{} already exists", dst.display())));
    }
    let out = crate::command()
        .args(["clone", "--bare", "--local", "--quiet", "--"])
        .arg(src)
        .arg(dst)
        .output()?;
    if !out.status.success() {
        let _ = std::fs::remove_dir_all(dst);
        return Err(Error::Other(format!(
            "git clone: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    let finish = || -> Result<()> {
        let repo = git2::Repository::open_bare(dst)?;
        // The fork shouldn't know where its parent lives on disk.
        repo.remote_delete("origin")?;
        // An empty source has no branch for clone to copy HEAD from.
        let source_head = git2::Repository::open_bare(src)?
            .find_reference("HEAD")?
            .symbolic_target()?
            .map(str::to_owned);
        if let Some(target) = source_head {
            repo.set_head(&target)?;
        }
        drop(repo);
        apply_config(dst)?;
        let _ = std::fs::remove_dir_all(dst.join("hooks"));
        Ok(())
    };
    finish().inspect_err(|_| {
        let _ = std::fs::remove_dir_all(dst);
    })
}

/// Copies the bare repository `src` to `dst` for a backup: every ref
/// (including pull request refs) and the objects they need, as real copies
/// rather than hardlinks. Safe while `src` is in use: git reads the refs
/// first and then copies what they reach, so pushes or repacks meanwhile
/// can't leave the copy inconsistent.
pub fn backup(src: &Path, dst: &Path) -> Result<()> {
    if dst.exists() {
        return Err(Error::Other(format!("{} already exists", dst.display())));
    }
    let out = crate::command()
        .args(["clone", "--mirror", "--no-hardlinks", "--quiet", "--"])
        .arg(src)
        .arg(dst)
        .output()?;
    if !out.status.success() {
        let _ = std::fs::remove_dir_all(dst);
        return Err(Error::Other(format!("git clone: {}", String::from_utf8_lossy(&out.stderr).trim())));
    }
    let finish = || -> Result<()> {
        // Only the remote's configuration: deleting the remote (libgit2's
        // `remote_delete`) would delete every ref its `refs/*` refspec
        // covers, which for a mirror is all of them.
        let out = crate::command()
            .arg("--git-dir")
            .arg(dst)
            .args(["config", "--remove-section", "remote.origin"])
            .output()?;
        if !out.status.success() {
            return Err(Error::Other(format!("git config: {}", String::from_utf8_lossy(&out.stderr).trim())));
        }
        apply_config(dst)?;
        let _ = std::fs::remove_dir_all(dst.join("hooks"));
        Ok(())
    };
    finish().inspect_err(|_| {
        let _ = std::fs::remove_dir_all(dst);
    })
}

/// More loose refs than this are packed into `packed-refs`. Each loose ref
/// is a file to open whenever refs are listed, which every clone, fetch
/// and push starts with, and git never packs them by itself here
/// (`gc.auto` is off).
const MAX_LOOSE_REFS: usize = 32;
/// More packs than this are repacked into one.
const MAX_PACKS: usize = 16;
/// More loose objects than this are packed. Small pushes, edits in the
/// browser and merges all leave loose objects: a file each, uncompressed
/// against each other, and outside the bitmap index.
const MAX_LOOSE_OBJECTS: usize = 2000;

/// Runs after every push, before anything reads what was pushed. Quick:
/// - points HEAD at an existing branch if it is unborn (first push was to a
///   branch other than the default),
/// - packs refs once many are loose,
/// - updates the commit-graph incrementally (fast `git log`/last-commit).
///
/// Packing objects takes longer and can wait: see [`repack_if_due`].
pub fn run_maintenance(path: &Path) -> Result<()> {
    fix_unborn_head(path)?;
    apply_config(path)?;
    pack_refs_if_due(path)?;
    git(path, &[
        "commit-graph", "write", "--reachable", "--changed-paths", "--split",
    ])?;
    Ok(())
}

/// Packs the repository's refs into one file once more than
/// [`MAX_LOOSE_REFS`] are files of their own.
pub fn pack_refs_if_due(path: &Path) -> Result<()> {
    if count_files(&path.join("refs"), MAX_LOOSE_REFS + 1) > MAX_LOOSE_REFS {
        git(path, &["pack-refs", "--all"])?;
    }
    Ok(())
}

/// Regular files under `dir`, counted up to `limit`.
fn count_files(dir: &Path, limit: usize) -> usize {
    let mut found = 0;
    let mut dirs = vec![dir.to_owned()];
    while let Some(dir) = dirs.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => dirs.push(entry.path()),
                Ok(_) => {
                    found += 1;
                    if found >= limit {
                        return found;
                    }
                }
                Err(_) => {}
            }
        }
    }
    found
}

/// Loose objects of the repository at `path`, counted up to `limit`.
fn loose_objects(path: &Path, limit: usize) -> usize {
    let Ok(dirs) = std::fs::read_dir(path.join("objects")) else { return 0 };
    let mut found = 0;
    for dir in dirs.flatten() {
        // `objects/3f`, not `objects/pack` or `objects/info`.
        let name = dir.file_name();
        if name.len() != 2 || !name.to_str().is_some_and(|n| n.bytes().all(|b| b.is_ascii_hexdigit())) {
            continue;
        }
        if let Ok(files) = std::fs::read_dir(dir.path()) {
            found += files.flatten().count();
            if found >= limit {
                return found;
            }
        }
    }
    found
}

/// What decides whether a repository is repacked.
struct Packs {
    packs: usize,
    /// There's a bitmap index, which lets git answer a clone or fetch
    /// without walking every object.
    bitmap: bool,
    /// A pack is also another repository's (a fork's or its parent's, by
    /// hardlink): rewriting it would end that, and take its size again.
    shared: bool,
}

impl Packs {
    fn of(path: &Path) -> Self {
        let mut found = Self { packs: 0, bitmap: false, shared: false };
        let Ok(entries) = std::fs::read_dir(path.join("objects/pack")) else { return found };
        for entry in entries.flatten() {
            match entry.path().extension().and_then(|x| x.to_str()) {
                Some("pack") => {
                    found.packs += 1;
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::MetadataExt;
                        found.shared |= entry.metadata().is_ok_and(|m| m.nlink() > 1);
                    }
                }
                Some("bitmap") => found.bitmap = true,
                _ => {}
            }
        }
        found
    }
}

/// Repositories being repacked, and those a repack left without a bitmap
/// index (nothing to index): they aren't repacked for one again.
static REPACKING: std::sync::Mutex<Option<RepackState>> = std::sync::Mutex::new(None);

#[derive(Default)]
struct RepackState {
    running: std::collections::HashSet<std::path::PathBuf>,
    no_bitmap: std::collections::HashSet<std::path::PathBuf>,
}

fn repack_state<T>(f: impl FnOnce(&mut RepackState) -> T) -> T {
    let mut state = REPACKING.lock().unwrap_or_else(|e| e.into_inner());
    f(state.get_or_insert_with(RepackState::default))
}

/// Packs the repository's objects if that's due, and returns whether it
/// did. It keeps clones and fetches cheap and the repository small:
/// - a repository whose packs have no bitmap index gets one pack with one
///   (unless a pack is shared with a fork: see [`Packs::shared`]),
/// - loose objects are packed once there are more than
///   [`MAX_LOOSE_OBJECTS`], into a pack of their own,
/// - more than [`MAX_PACKS`] packs become one again.
///
/// Objects nothing refers to any more go into a pack of their own (a
/// "cruft pack") and are deleted two weeks after they were last written,
/// as `git gc` does: what a push or an edit that's still under way just
/// wrote is never taken away from it.
///
/// Takes seconds for most repositories and minutes for very large ones;
/// reading and pushing carry on meanwhile. Does nothing if the repository
/// is being repacked already.
pub fn repack_if_due(path: &Path) -> Result<bool> {
    let key = path.to_owned();
    let (free, no_bitmap) = repack_state(|s| (s.running.insert(key.clone()), s.no_bitmap.contains(&key)));
    if !free {
        return Ok(false);
    }
    struct Done(std::path::PathBuf);
    impl Drop for Done {
        fn drop(&mut self) {
            repack_state(|s| s.running.remove(&self.0));
        }
    }
    let _done = Done(key.clone());

    let now = Packs::of(path);
    let unindexed = now.packs > 0 && !now.bitmap && !now.shared && !no_bitmap;
    let loose = loose_objects(path, MAX_LOOSE_OBJECTS + 1) > MAX_LOOSE_OBJECTS;
    let mut all = now.packs > MAX_PACKS || unindexed || (loose && now.packs == 0);
    if !all && !loose {
        return Ok(false);
    }
    if !all {
        // Only what's loose, leaving the large pack (and its bitmap) as it
        // is. `repack.writeBitmaps` is for packing everything.
        git(path, &["repack", "-d", "-q", "--no-write-bitmap-index"])?;
        // Objects nothing refers to stay loose that way.
        all = loose_objects(path, MAX_LOOSE_OBJECTS + 1) > MAX_LOOSE_OBJECTS;
    }
    if all {
        repack_all(path)?;
        if !Packs::of(path).bitmap {
            repack_state(|s| s.no_bitmap.insert(key));
        }
    }
    Ok(true)
}

/// Everything into one pack with a bitmap index (see [`repack_if_due`]).
fn repack_all(path: &Path) -> Result<()> {
    git(path, &["repack", "--cruft", "--cruft-expiration=2.weeks.ago", "-d", "--write-bitmap-index", "-q"])
}

/// Writes keel's repository config; also brings repositories created by
/// older versions up to date.
fn apply_config(path: &Path) -> Result<()> {
    let mut config = git2::Repository::open_bare(path)?.config()?.open_level(git2::ConfigLevel::Local)?;
    for (key, value) in REPO_CONFIG {
        if config.get_string(key).ok().as_deref() != Some(value) {
            config.set_str(key, value)?;
        }
    }
    for (key, value) in REPO_MULTI_CONFIG {
        let mut present = false;
        config.multivar(key, None)?.for_each(|e| present |= e.value_bytes() == value.as_bytes())?;
        if !present {
            // Replaces values matching the pattern (none), so adds one.
            config.set_multivar(key, "^$", value)?;
        }
    }
    Ok(())
}

fn fix_unborn_head(path: &Path) -> Result<()> {
    let repo = GitRepo::open(path)?;
    let unborn = repo.repo.head().is_err();
    if unborn && let Some(branch) = repo.default_branch()? {
        repo.repo.set_head(&format!("refs/heads/{branch}"))?;
    }
    Ok(())
}

fn git(path: &Path, args: &[&str]) -> Result<()> {
    let out = crate::command().arg("-C").arg(path).args(args).output()?;
    if !out.status.success() {
        return Err(Error::Other(format!(
            "git {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn backups_keep_refs() {
        let s = crate::read::tests::Scratch::new();
        s.write("a.txt", "a");
        let tip = s.commit("first");
        let out = tempfile::tempdir().unwrap();
        let dst = out.path().join("copy.git");
        super::backup(s.open().repo.path(), &dst).unwrap();
        let copy = crate::GitRepo::open(&dst).unwrap();
        assert_eq!(copy.branch_tip("main"), Some(tip));
        assert_eq!(copy.default_branch().unwrap().as_deref(), Some("main"));
        assert!(git2::Repository::open_bare(&dst).unwrap().find_remote("origin").is_err());
    }
    use super::*;
    use crate::read::tests::Scratch;

    #[test]
    fn checks_pushed_objects() {
        let root = tempfile::tempdir().unwrap();
        let bare = root.path().join("r.git");
        create_bare(&bare, "main").unwrap();
        let s = Scratch::new();
        s.write("x", "1");
        let good = s.commit("good");
        // A tree with a `.git` entry, which would write into the git
        // directory of whoever checks it out on a case-insensitive system.
        let blob = git2::Oid::from_str(&s.git(&["rev-parse", "HEAD:x"])).unwrap();
        let mut tree = b"100644 .GIT\0".to_vec();
        tree.extend_from_slice(blob.as_bytes());
        std::fs::write(s.dir.path().join("tree"), &tree).unwrap();
        let tree = s.git(&["hash-object", "-t", "tree", "--literally", "-w", "tree"]);
        let bad = s.git(&["commit-tree", &tree, "-p", &good.to_string(), "-m", "bad"]);
        let push = |rev: &str| {
            std::process::Command::new("git")
                .current_dir(s.dir.path())
                .args(["push", "-q", bare.to_str().unwrap(), &format!("{rev}:refs/heads/main")])
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .unwrap()
        };
        let refused = push(&bad);
        assert!(!refused.status.success());
        assert!(String::from_utf8_lossy(&refused.stderr).contains("hasDotgit"), "{}", String::from_utf8_lossy(&refused.stderr));
        assert!(push(&good.to_string()).status.success());
    }

    #[test]
    fn hidden_refs() {
        let root = tempfile::tempdir().unwrap();
        let bare = root.path().join("r.git");
        create_bare(&bare, "main").unwrap();
        // An older repository with only the pull request refs hidden.
        let values = |key: &str| {
            let config = git2::Repository::open_bare(&bare).unwrap().config().unwrap();
            let mut values = Vec::new();
            config.multivar(key, None).unwrap().for_each(|e| values.push(e.value().unwrap().to_owned())).unwrap();
            values
        };
        assert_eq!(values("receive.hideRefs"), ["refs/pull", "refs/replace"]);
        let mut config = git2::Repository::open_bare(&bare).unwrap().config().unwrap();
        config.remove_multivar("receive.hideRefs", "replace").unwrap();
        config.remove_multivar("uploadpack.hideRefs", ".*").unwrap();
        drop(config);
        apply_config(&bare).unwrap();
        apply_config(&bare).unwrap();
        assert_eq!(values("receive.hideRefs"), ["refs/pull", "refs/replace"]);
        assert_eq!(values("uploadpack.hideRefs"), ["refs/replace"]);

        // Pushes to refs/replace are refused.
        let s = Scratch::new();
        s.write("x", "1");
        let a = s.commit("a");
        s.write("x", "2");
        let b = s.commit("b");
        s.git(&["replace", &a.to_string(), &b.to_string()]);
        s.git(&["push", "-q", bare.to_str().unwrap(), "main"]);
        let out = std::process::Command::new("git")
            .current_dir(s.dir.path())
            .args(["push", "-q", bare.to_str().unwrap(), "refs/replace/*:refs/replace/*"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(!out.status.success());
        assert!(git2::Repository::open_bare(&bare).unwrap().find_reference(&format!("refs/replace/{a}")).is_err());
    }

    #[test]
    fn packs_refs_and_objects() {
        let root = tempfile::tempdir().unwrap();
        let bare = root.path().join("r.git");
        create_bare(&bare, "main").unwrap();
        let s = Scratch::new();
        s.write("a", "1");
        let first = s.commit("first");
        s.write("a", "2");
        let gone = s.commit("on a branch that's deleted");
        s.git(&["branch", "gone"]);
        s.git(&["reset", "-q", "--hard", &first.to_string()]);
        for i in 0..40 {
            s.git(&["tag", &format!("v{i}")]);
        }
        s.git(&["push", "-q", bare.to_str().unwrap(), "main", "gone", "--tags"]);
        let loose_refs = || count_files(&bare.join("refs"), usize::MAX);
        assert_eq!(loose_refs(), 42);

        // Refs: many loose ones are packed, and still resolve.
        run_maintenance(&bare).unwrap();
        assert_eq!(loose_refs(), 0);
        assert!(bare.join("packed-refs").is_file());
        let repo = GitRepo::open(&bare).unwrap();
        assert_eq!(repo.branch_tip("main"), Some(first));
        assert_eq!(repo.resolve_rev("v39"), Some(first));
        assert_eq!(repo.tags().unwrap().len(), 40);

        // Objects: a few loose ones are left alone.
        assert!(loose_objects(&bare, usize::MAX) > 0);
        assert!(!repack_if_due(&bare).unwrap());
        // A pack without a bitmap index (a large push, an import) is
        // repacked with one, once.
        git(&bare, &["repack", "-a", "-d", "-q", "--no-write-bitmap-index"]).unwrap();
        let packs = Packs::of(&bare);
        assert!(packs.packs == 1 && !packs.bitmap && !packs.shared);
        assert!(repack_if_due(&bare).unwrap());
        assert!(Packs::of(&bare).bitmap);
        assert_eq!(loose_objects(&bare, usize::MAX), 0);
        assert!(!repack_if_due(&bare).unwrap());

        // What nothing refers to any more is kept (for two weeks).
        repo.update_ref("refs/heads/gone", None, Some(gone)).unwrap();
        repack_all(&bare).unwrap();
        let repo = GitRepo::open(&bare).unwrap();
        assert_eq!(repo.commit_summary(gone).unwrap().summary, "on a branch that's deleted");
        assert_eq!(repo.commit_summary(first).unwrap().summary, "first");

        // A fork shares its parent's packs: those aren't rewritten for a
        // bitmap index (the fork has the parent's).
        let forked = root.path().join("f.git");
        fork(&bare, &forked).unwrap();
        assert!(Packs::of(&forked).shared && Packs::of(&forked).bitmap);
        assert!(!repack_if_due(&forked).unwrap());
    }

    #[test]
    fn create_push_and_maintain() {
        let root = tempfile::tempdir().unwrap();
        let bare = root.path().join("r.git");
        create_bare(&bare, "main").unwrap();
        assert!(create_bare(&bare, "main").is_err());
        assert!(!GitRepo::open(&bare).unwrap().has_branches().unwrap());

        // Push only a non-default branch; maintenance should adopt it as HEAD.
        let s = Scratch::new();
        s.write("x", "1");
        s.commit("x");
        s.git(&["push", "-q", bare.to_str().unwrap(), "main:trunk"]);
        run_maintenance(&bare).unwrap();

        let repo = GitRepo::open(&bare).unwrap();
        assert_eq!(repo.default_branch().unwrap().as_deref(), Some("trunk"));
        assert!(bare.join("objects/info/commit-graphs").exists());

        // Forks get branches and HEAD, not pull refs or the parent's path.
        repo.update_ref("refs/pull/1/head", repo.branch_tip("trunk"), None).unwrap();
        let forked = root.path().join("f.git");
        fork(&bare, &forked).unwrap();
        let f = GitRepo::open(&forked).unwrap();
        assert_eq!(f.default_branch().unwrap().as_deref(), Some("trunk"));
        assert_eq!(f.branch_tip("trunk"), repo.branch_tip("trunk"));
        assert_eq!(f.ref_tip("refs/pull/1/head"), None);
        assert!(f.repo.remotes().unwrap().is_empty());
        assert!(fork(&bare, &forked).is_err(), "exists");

        // Forking an empty repository keeps its default branch name.
        let empty = root.path().join("e.git");
        create_bare(&empty, "dev").unwrap();
        fork(&empty, &root.path().join("ef.git")).unwrap();
        let ef = git2::Repository::open_bare(root.path().join("ef.git")).unwrap();
        assert_eq!(ef.find_reference("HEAD").unwrap().symbolic_target().unwrap(), Some("refs/heads/dev"));
    }
}
