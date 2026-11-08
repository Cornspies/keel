//! Merging, for pull requests. Everything runs against the bare repository
//! with git's plumbing: `merge-tree --write-tree` computes the merge result
//! without a working tree, `commit-tree` records it, and `update-ref` moves
//! the branch only if it still points where we expect (compare-and-swap),
//! so a concurrent push can't be overwritten.

use std::path::Path;
use std::process::Command;

use git2::Oid;

use crate::{CommitSummary, Error, GitRepo, Result};

/// Name and email recorded on the commits keel makes, and what signs them.
#[derive(Clone, Copy)]
pub struct Identity<'a> {
    pub name: &'a str,
    pub email: &'a str,
    pub signer: Option<&'a dyn Signer>,
}

impl<'a> Identity<'a> {
    /// Unsigned commits by `name <email>`.
    pub fn new(name: &'a str, email: &'a str) -> Self {
        Self { name, email, signer: None }
    }

    /// The same, signed by `signer`.
    pub fn signed_by(self, signer: &'a dyn Signer) -> Self {
        Self { signer: Some(signer), ..self }
    }
}

/// Signs the commits keel makes (merges, squashes, rebases, edits in the
/// browser) with the server's key.
pub trait Signer: Send + Sync {
    /// The ASCII-armored signature of `commit` (the commit object without
    /// a signature), for its `gpgsig` header.
    fn sign(&self, commit: &[u8]) -> std::result::Result<String, String>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MergeStyle {
    /// A merge commit with both tips as parents.
    Merge,
    /// One new commit on top of the base with the combined changes.
    Squash,
    /// The head's commits, replayed one by one on top of the base.
    Rebase,
}

/// Most commits a rebase replays (each takes a `git merge-tree`).
pub const REBASE_LIMIT: usize = 1000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Mergeability {
    /// Merges cleanly; the resulting tree.
    Clean(Oid),
    /// Files with conflicts.
    Conflicts(Vec<String>),
}

fn git(path: &Path) -> Command {
    let mut cmd = crate::command();
    cmd.arg("--git-dir").arg(path);
    cmd
}

fn short(id: Oid) -> String {
    id.to_string()[..10].to_owned()
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

    /// The ids of up to `limit` commits reachable from `head` but not from
    /// `base`, newest first.
    pub fn commit_ids_between(&self, base: Oid, head: Oid, limit: usize) -> Result<Vec<Oid>> {
        let mut walk = self.repo.revwalk()?;
        walk.push(head)?;
        walk.hide(base)?;
        Ok(walk.take(limit).collect::<std::result::Result<Vec<_>, _>>()?)
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
        self.merge_tree(None, base, head)
    }

    /// Merges commit `theirs` into `ours`, from their merge base or from
    /// `merge_base` if given.
    fn merge_tree(&self, merge_base: Option<Oid>, ours: Oid, theirs: Oid) -> Result<Mergeability> {
        let mut cmd = git(self.repo.path());
        cmd.args(["merge-tree", "--write-tree", "--name-only", "--no-messages"]);
        if let Some(b) = merge_base {
            cmd.arg(format!("--merge-base={b}"));
        }
        let out = cmd.arg(ours.to_string()).arg(theirs.to_string()).output()?;
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
        if style == MergeStyle::Rebase {
            let tip = self.rebase(base, head, who)?;
            self.update_ref(&format!("refs/heads/{base_branch}"), Some(tip), Some(base))?;
            return Ok(tip);
        }
        let Mergeability::Clean(tree) = self.mergeability(base, head)? else {
            return Err(Error::Other("the branches have conflicts".into()));
        };
        let tree = self.repo.find_tree(tree)?;
        let mut parents = vec![self.repo.find_commit(base)?];
        if style == MergeStyle::Merge {
            parents.push(self.repo.find_commit(head)?);
        }
        let sig = git2::Signature::now(who.name, who.email)?;
        // As `git commit -m` writes it.
        let message = format!("{}\n", message.trim_end_matches('\n'));
        let commit = self.create_commit(&sig, &sig, &message, &tree, &parents.iter().collect::<Vec<_>>(), who)?;
        self.update_ref(&format!("refs/heads/{base_branch}"), Some(commit), Some(base))?;
        Ok(commit)
    }

    /// Replays the commits on `head` that aren't on `base` on top of
    /// `base`, keeping their authors and messages, with `who` as the
    /// committer; commits that end up changing nothing are dropped. If
    /// `head` already builds on `base` its commits are kept as they are
    /// (signatures and all). Returns the new tip; writes no refs.
    pub fn rebase(&self, base: Oid, head: Oid, who: &Identity<'_>) -> Result<Oid> {
        if self.is_ancestor(base, head)? {
            return Ok(head);
        }
        let mut walk = self.repo.revwalk()?;
        walk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)?;
        walk.push(head)?;
        walk.hide(base)?;
        let commits = walk.take(REBASE_LIMIT + 1).collect::<std::result::Result<Vec<_>, _>>()?;
        if commits.len() > REBASE_LIMIT {
            return Err(Error::Rebase(format!("more than {REBASE_LIMIT} commits to rebase")));
        }
        let committer = git2::Signature::now(who.name, who.email)?;
        let mut tip = self.repo.find_commit(base)?;
        for id in commits {
            let commit = self.repo.find_commit(id)?;
            if commit.parent_count() != 1 {
                return Err(Error::Rebase(format!("{} is a merge commit", short(id))));
            }
            let parent = commit.parent_id(0)?;
            let tree = match self.merge_tree(Some(parent), tip.id(), id)? {
                Mergeability::Clean(tree) => tree,
                Mergeability::Conflicts(files) => {
                    return Err(Error::Rebase(format!("{} conflicts in {}", short(id), files.join(", "))));
                }
            };
            if tree == tip.tree_id() {
                continue;
            }
            let author = commit.author();
            let author = git2::Signature::new(
                &String::from_utf8_lossy(author.name_bytes()),
                &String::from_utf8_lossy(author.email_bytes()),
                &author.when(),
            )?;
            let message = String::from_utf8_lossy(commit.message_raw_bytes());
            let tree = self.repo.find_tree(tree)?;
            let new = self.create_commit(&author, &committer, &message, &tree, &[&tip], who)?;
            tip = self.repo.find_commit(new)?;
        }
        Ok(tip.id())
    }

    /// Writes a commit (no refs), signed if `who` has a signer.
    pub(crate) fn create_commit(
        &self,
        author: &git2::Signature<'_>,
        committer: &git2::Signature<'_>,
        message: &str,
        tree: &git2::Tree<'_>,
        parents: &[&git2::Commit<'_>],
        who: &Identity<'_>,
    ) -> Result<Oid> {
        let Some(signer) = who.signer else {
            return Ok(self.repo.commit(None, author, committer, message, tree, parents)?);
        };
        let buffer = self.repo.commit_create_buffer(author, committer, message, tree, parents)?;
        let content = std::str::from_utf8(&buffer).map_err(|_| Error::Other("commit isn't UTF-8".into()))?;
        let signature = signer.sign(content.as_bytes()).map_err(|e| Error::Other(format!("signing the commit: {e}")))?;
        Ok(self.repo.commit_signed(content, &signature, None)?)
    }

    /// Sets (`new` = `Some`) or deletes a ref. With `old`, only if the ref
    /// currently points there.
    pub fn update_ref(&self, name: &str, new: Option<Oid>, old: Option<Oid>) -> Result<()> {
        // Full ref names only, so the name can't be taken for an option.
        if !name.starts_with("refs/") || !crate::valid_ref_name(name) {
            return Err(Error::Other(format!("{name} isn't a ref name")));
        }
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

    /// Fetches `branch` from the repository at `from` (a fork) into ref
    /// `into` of this one, replacing what was there. Returns the new tip.
    pub fn fetch_branch(&self, from: &Path, branch: &str, into: &str) -> Result<Oid> {
        // Both must be valid ref names, so the refspec can't be read another
        // way (with a `:` in the branch, say).
        if !crate::valid_ref_name(&format!("refs/heads/{branch}")) || !into.starts_with("refs/") || !crate::valid_ref_name(into) {
            return Err(Error::Other(format!("can't fetch {branch} into {into}")));
        }
        run(git(self.repo.path())
            .args(["fetch", "--quiet", "--no-tags", "--no-write-fetch-head", "--"])
            .arg(from)
            .arg(format!("+refs/heads/{branch}:{into}")))?;
        self.ref_tip(into)
            .ok_or_else(|| Error::Other(format!("fetching {branch} created no {into}")))
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
    fn ignores_replace_refs() {
        let (s, repo) = bare_with_branches();
        let main = repo.branch_tip("main").unwrap();
        let feature = repo.branch_tip("feature").unwrap();
        s.git(&["checkout", "-q", "-b", "evil", "feature"]);
        s.write("a.txt", "evil\n");
        let evil = s.commit("evil");
        s.git(&["replace", &feature.to_string(), &evil.to_string()]);
        let Mergeability::Clean(tree) = repo.mergeability(main, feature).unwrap() else { panic!("conflicts") };
        let tree = repo.repo.find_tree(tree).unwrap();
        let blob = tree.get_path(Path::new("a.txt")).unwrap().to_object(&repo.repo).unwrap();
        assert_eq!(blob.as_blob().unwrap().content(), b"feature\n");
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

        let who = Identity::new("Ada", "ada@example.com");
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
    fn rebase() {
        let (s, repo) = bare_with_branches();
        let main = repo.branch_tip("main").unwrap();
        let feature = repo.branch_tip("feature").unwrap();
        let who = Identity::new("Bob", "bob@example.com");
        let tip = repo.merge("main", main, feature, MergeStyle::Rebase, "", &who).unwrap();
        assert_eq!(repo.branch_tip("main"), Some(tip));
        // Linear: the two feature commits on top of main, authors kept.
        let log = s.git(&["log", "--format=%s|%an|%cn|%P", "main"]);
        let lines: Vec<&str> = log.lines().collect();
        assert_eq!(lines[0].rsplit_once('|').unwrap().0, "feature: add new|Ada|Bob");
        assert_eq!(lines[1].rsplit_once('|').unwrap().0, "feature: change a|Ada|Bob");
        assert_eq!(lines[2].split('|').next(), Some("main: change b"));
        assert!(lines.iter().all(|l| l.rsplit('|').next().unwrap().split(' ').count() == 1));
        assert_eq!(s.git(&["show", "main:a.txt"]), "feature");
        assert_eq!(s.git(&["show", "main:b.txt"]), "main");

        // Already on top: kept as is.
        s.git(&["branch", "-f", "more", "main"]);
        s.git(&["checkout", "-q", "more"]);
        s.write("c.txt", "c\n");
        s.commit("more: add c");
        let more = repo.branch_tip("more").unwrap();
        assert_eq!(repo.rebase(tip, more, &who).unwrap(), more);

        // A commit that conflicts with main's change to b.
        s.git(&["checkout", "-q", "-b", "clash", &format!("{main}~1")]);
        s.write("b.txt", "clash\n");
        s.commit("clash: change b");
        let clash = repo.branch_tip("clash").unwrap();
        let err = repo.rebase(tip, clash, &who).unwrap_err().to_string();
        assert!(err.contains("conflicts in b.txt"), "{err}");
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
        let who = Identity::new("Ada", "ada@example.com");
        assert!(repo.merge("main", main, feature, MergeStyle::Merge, "m", &who).is_err());

        repo.update_ref("refs/pull/1/head", Some(feature), None).unwrap();
        assert_eq!(repo.ref_tip("refs/pull/1/head"), Some(feature));
        repo.update_ref("refs/heads/feature", None, Some(feature)).unwrap();
        assert_eq!(repo.branch_tip("feature"), None);
        assert_eq!(repo.ref_tip("refs/pull/1/head"), Some(feature), "survives branch deletion");
    }

    #[test]
    fn fetch_from_fork() {
        let (s, repo) = bare_with_branches();
        let root = tempfile::tempdir().unwrap();
        let fork = root.path().join("fork.git");
        crate::store::fork(s.dir.path().join(".git").as_path(), &fork).unwrap();
        let forked = GitRepo::open(&fork).unwrap();
        // A commit that only exists in the fork.
        let tree = forked.repo.find_commit(forked.branch_tip("feature").unwrap()).unwrap().tree().unwrap();
        let sig = git2::Signature::now("Bob", "bob@example.com").unwrap();
        let parent = forked.repo.find_commit(forked.branch_tip("feature").unwrap()).unwrap();
        let new = forked.repo.commit(Some("refs/heads/topic"), &sig, &sig, "fork only", &tree, &[&parent]).unwrap();

        assert_eq!(repo.fetch_branch(&fork, "topic", "refs/pull/7/head").unwrap(), new);
        assert!(repo.repo.find_commit(new).is_ok());
        assert!(repo.fetch_branch(&fork, "missing", "refs/pull/8/head").is_err());
        assert_eq!(repo.branch_tip("topic"), None, "only the pull ref is written");

        // Comparing without fetching.
        let (s2, base) = bare_with_branches();
        let combined = GitRepo::open_with(&s2.dir.path().join(".git"), &fork).unwrap();
        assert!(base.repo.find_commit(new).is_err());
        let main = combined.branch_tip("main").unwrap();
        assert_eq!(combined.commits_between(main, new, 10).unwrap().last().unwrap().summary, "fork only");
    }
}
