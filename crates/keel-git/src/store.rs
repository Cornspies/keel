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

/// Runs after every push. Keeps reads fast without full `git gc` runs:
/// - points HEAD at an existing branch if it is unborn (first push was to a
///   branch other than the default),
/// - updates the commit-graph incrementally (fast `git log`/last-commit),
/// - repacks with bitmaps once loose objects and small packs pile up.
pub fn run_maintenance(path: &Path) -> Result<()> {
    fix_unborn_head(path)?;
    apply_config(path)?;
    git(path, &[
        "commit-graph", "write", "--reachable", "--changed-paths", "--split",
    ])?;
    let packs = std::fs::read_dir(path.join("objects/pack"))?
        .filter(|e| {
            e.as_ref()
                .is_ok_and(|e| e.path().extension().is_some_and(|x| x == "pack"))
        })
        .count();
    if packs > 16 {
        git(path, &["repack", "-a", "-d", "--write-bitmap-index", "-q"])?;
    }
    Ok(())
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
