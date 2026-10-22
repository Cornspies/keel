//! Merging, for pull requests. Everything runs against the bare repository
//! with git's plumbing: `merge-tree --write-tree` computes the merge result
//! without a working tree, `commit-tree` records it, and `update-ref` moves
//! the branch only if it still points where we expect (compare-and-swap),
//! so a concurrent push can't be overwritten.

use std::path::Path;
use std::process::Command;

use git2::Oid;

use crate::{CommitSummary, Error, GitRepo, Result};

/// Name and email recorded on merge commits.
pub struct Identity<'a> {
    pub name: &'a str,
    pub email: &'a str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeStyle {
    /// A merge commit with both tips as parents.
    Merge,
    /// One new commit on top of the base with the combined changes.
    Squash,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mergeability {
    /// Merges cleanly; the resulting tree.
    Clean(Oid),
    /// Files with conflicts.
    Conflicts(Vec<String>),
}

fn git(path: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("--git-dir").arg(path).env("GIT_CONFIG_NOSYSTEM", "1");
    cmd
}

fn run(cmd: &mut Command) -> Result<String> {
    let out = cmd.output()?;
    if !out.status.success() {
        return Err(Error::Other(format!(
            "git failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

impl GitRepo {
    pub fn merge_base(&self, a: Oid, b: Oid) -> Result<Option<Oid>> {
        match self.repo.merge_base(a, b) {
            Ok(oid) => Ok(Some(oid)),
            Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    pub fn is_ancestor(&self, ancestor: Oid, of: Oid) -> Result<bool> {
        Ok(ancestor == of || self.repo.graph_descendant_of(of, ancestor)?)
    }

    /// Commits reachable from `head` but not from `base`, oldest first.
    pub fn commits_between(&self, base: Oid, head: Oid, limit: usize) -> Result<Vec<CommitSummary>> {
        let mut walk = self.repo.revwalk()?;
        walk.push(head)?;
        walk.hide(base)?;
        let mut out = walk
            .take(limit)
            .map(|id| Ok(crate::read::summarize(&self.repo.find_commit(id?)?)))
            .collect::<Result<Vec<_>>>()?;
        out.reverse();
        Ok(out)
    }

    /// Would merging `head` into `base` succeed without conflicts?
    pub fn mergeability(&self, base: Oid, head: Oid) -> Result<Mergeability> {
        let out = git(self.repo.path())
            .args(["merge-tree", "--write-tree", "--name-only", "--no-messages"])
            .arg(base.to_string())
            .arg(head.to_string())
            .output()?;
        let stdout = String::from_utf8_lossy(&out.stdout);
        let mut lines = stdout.lines();
        let tree = lines.next().and_then(|l| Oid::from_str(l.trim()).ok());
        match (out.status.code(), tree) {
            (Some(0), Some(tree)) => Ok(Mergeability::Clean(tree)),
            (Some(1), _) => Ok(Mergeability::Conflicts(
                lines.take_while(|l| !l.is_empty()).map(str::to_owned).collect(),
            )),
            _ => Err(Error::Other(format!(
                "git merge-tree: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))),
        }
    }

    /// Merges `head` into branch `base_branch`, which must still point at
    /// `base`. Returns the new tip of the base branch.
    pub fn merge(
        &self,
        base_branch: &str,
        base: Oid,
        head: Oid,
        style: MergeStyle,
        message: &str,
        who: &Identity<'_>,
    ) -> Result<Oid> {
        let Mergeability::Clean(tree) = self.mergeability(base, head)? else {
            return Err(Error::Other("the branches have conflicts".into()));
        };
        let mut cmd = git(self.repo.path());
        cmd.args(["commit-tree", &tree.to_string(), "-p", &base.to_string()]);
        if style == MergeStyle::Merge {
            cmd.args(["-p", &head.to_string()]);
        }
        cmd.args(["-m", message])
            .env("GIT_AUTHOR_NAME", who.name)
            .env("GIT_AUTHOR_EMAIL", who.email)
            .env("GIT_COMMITTER_NAME", who.name)
            .env("GIT_COMMITTER_EMAIL", who.email);
        let commit = Oid::from_str(&run(&mut cmd)?)?;
        self.update_ref(&format!("refs/heads/{base_branch}"), Some(commit), Some(base))?;
        Ok(commit)
    }

    /// Sets (`new` = `Some`) or deletes a ref. With `old`, only if the ref
    /// currently points there.
    pub fn update_ref(&self, name: &str, new: Option<Oid>, old: Option<Oid>) -> Result<()> {
        let mut cmd = git(self.repo.path());
        cmd.arg("update-ref");
        match new {
            Some(new) => cmd.arg(name).arg(new.to_string()),
            None => cmd.arg("-d").arg(name),
        };
        if let Some(old) = old {
            cmd.arg(old.to_string());
        }
        run(&mut cmd).map(|_| ())
    }

    /// The commit a branch points at.
    pub fn branch_tip(&self, branch: &str) -> Option<Oid> {
        self.repo
            .find_reference(&format!("refs/heads/{branch}"))
            .ok()?
            .peel_to_commit()
            .ok()
            .map(|c| c.id())
    }

    /// The commit any ref points at, e.g. `refs/pull/1/head`.
    pub fn ref_tip(&self, name: &str) -> Option<Oid> {
        self.repo.find_reference(name).ok()?.peel_to_commit().ok().map(|c| c.id())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read::tests::Scratch;

    fn bare_with_branches() -> (Scratch, GitRepo) {
        let s = Scratch::new();
        s.write("a.txt", "base\n");
        s.write("b.txt", "base\n");
        s.commit("base");
        s.git(&["checkout", "-q", "-b", "feature"]);
        s.write("a.txt", "feature\n");
        s.commit("feature: change a");
        s.write("new.txt", "x\n");
        s.commit("feature: add new");
        s.git(&["checkout", "-q", "main"]);
        s.write("b.txt", "main\n");
        s.commit("main: change b");
        let repo = s.open();
        (s, repo)
    }

    #[test]
    fn merge_commit_and_squash() {
        let (s, repo) = bare_with_branches();
        let main = repo.branch_tip("main").unwrap();
        let feature = repo.branch_tip("feature").unwrap();
        let base = repo.merge_base(main, feature).unwrap().unwrap();
        let commits: Vec<String> = repo.commits_between(main, feature, 10).unwrap().into_iter().map(|c| c.summary).collect();
        assert_eq!(commits, ["feature: change a", "feature: add new"]);
        assert_eq!(repo.diff_range(Some(base), feature, 10, 100).unwrap().files.len(), 2);
        assert!(matches!(repo.mergeability(main, feature).unwrap(), Mergeability::Clean(_)));

        let who = Identity { name: "Ada", email: "ada@example.com" };
        // Stale expectation: refused, branch unchanged.
        assert!(repo.merge("main", base, feature, MergeStyle::Merge, "m", &who).is_err());
        assert_eq!(repo.branch_tip("main"), Some(main));

        let merged = repo.merge("main", main, feature, MergeStyle::Merge, "Merge feature", &who).unwrap();
        assert_eq!(repo.branch_tip("main"), Some(merged));
        assert!(repo.is_ancestor(feature, merged).unwrap());
        let detail = repo.commit_detail(merged).unwrap();
        assert_eq!(detail.parents, [main, feature]);
        assert_eq!(detail.commit.author.name, "Ada");
        assert_eq!(s.git(&["show", "main:a.txt"]), "feature");
        assert_eq!(s.git(&["show", "main:b.txt"]), "main");

        // Squash: one parent, same content.
        repo.update_ref("refs/heads/main", Some(main), None).unwrap();
        let squashed = repo.merge("main", main, feature, MergeStyle::Squash, "Feature (#1)", &who).unwrap();
        assert_eq!(repo.commit_detail(squashed).unwrap().parents, [main]);
        assert_eq!(s.git(&["show", "main:new.txt"]), "x");
    }

    #[test]
    fn conflicts_and_refs() {
        let (s, repo) = bare_with_branches();
        s.git(&["checkout", "-q", "main"]);
        s.write("a.txt", "main\n");
        s.commit("main: change a");
        let main = repo.branch_tip("main").unwrap();
        let feature = repo.branch_tip("feature").unwrap();
        assert_eq!(repo.mergeability(main, feature).unwrap(), Mergeability::Conflicts(vec!["a.txt".into()]));
        let who = Identity { name: "Ada", email: "ada@example.com" };
        assert!(repo.merge("main", main, feature, MergeStyle::Merge, "m", &who).is_err());

        repo.update_ref("refs/pull/1/head", Some(feature), None).unwrap();
        assert_eq!(repo.ref_tip("refs/pull/1/head"), Some(feature));
        repo.update_ref("refs/heads/feature", None, Some(feature)).unwrap();
        assert_eq!(repo.branch_tip("feature"), None);
        assert_eq!(repo.ref_tip("refs/pull/1/head"), Some(feature), "survives branch deletion");
    }
}
