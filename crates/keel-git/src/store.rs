use std::path::Path;
use std::process::Command;

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
    let mut config = repo.config()?;
    for (key, value) in REPO_CONFIG {
        config.set_str(key, value)?;
    }
    // libgit2 creates sample hooks; keel never runs repository hooks.
    let _ = std::fs::remove_dir_all(path.join("hooks"));
    Ok(())
}

/// Runs after every push. Keeps reads fast without full `git gc` runs:
/// - points HEAD at an existing branch if it is unborn (first push was to a
///   branch other than the default),
/// - updates the commit-graph incrementally (fast `git log`/last-commit),
/// - repacks with bitmaps once loose objects and small packs pile up.
pub fn run_maintenance(path: &Path) -> Result<()> {
    fix_unborn_head(path)?;
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

fn fix_unborn_head(path: &Path) -> Result<()> {
    let repo = GitRepo::open(path)?;
    let unborn = repo.repo.head().is_err();
    if unborn && let Some(branch) = repo.default_branch()? {
        repo.repo.set_head(&format!("refs/heads/{branch}"))?;
    }
    Ok(())
}

fn git(path: &Path, args: &[&str]) -> Result<()> {
    let out = Command::new("git").arg("-C").arg(path).args(args).output()?;
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
    use super::*;
    use crate::read::tests::Scratch;

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
    }
}
