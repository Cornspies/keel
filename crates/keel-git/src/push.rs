//! What a push changed, for webhooks: ref snapshots and the commits a ref
//! update added.

use std::collections::BTreeMap;

use git2::Oid;

use crate::{CommitDetail, GitRepo, Result};

/// Branches and tags (full ref names) and what they point at.
pub type Refs = BTreeMap<String, Oid>;

/// One ref a push created, moved or deleted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RefChange {
    /// Full name, e.g. `refs/heads/main`.
    pub name: String,
    pub old: Option<Oid>,
    pub new: Option<Oid>,
}

/// The refs that differ between two snapshots, by name.
pub fn ref_changes(before: &Refs, after: &Refs) -> Vec<RefChange> {
    let mut names: Vec<&String> = before.keys().chain(after.keys()).collect();
    names.sort();
    names.dedup();
    names
        .into_iter()
        .filter_map(|name| {
            let (old, new) = (before.get(name).copied(), after.get(name).copied());
            (old != new).then(|| RefChange { name: name.clone(), old, new })
        })
        .collect()
}

impl GitRepo {
    /// All branches and tags.
    pub fn refs_snapshot(&self) -> Result<Refs> {
        let mut out = Refs::new();
        for glob in ["refs/heads/*", "refs/tags/*"] {
            for r in self.repo.references_glob(glob)? {
                let r = r?;
                if let (Ok(name), Some(target)) = (r.name(), r.target()) {
                    out.insert(name.to_owned(), target);
                }
            }
        }
        Ok(out)
    }

    /// Commits that `change` added, oldest first, at most `limit` of them,
    /// and how many there are in total (counting stops at `count_to`). For
    /// a new branch that's what no ref in `before` had. Also whether the
    /// update was forced (the old tip is no longer contained).
    ///
    /// A tag can point at a file or a folder rather than a commit (git.git
    /// has one): that adds no commits.
    pub fn pushed_commits(
        &self,
        change: &RefChange,
        before: &Refs,
        limit: usize,
        count_to: usize,
    ) -> Result<(Vec<CommitDetail>, usize, bool)> {
        let commit = |id: Oid| Some(self.repo.find_object(id, None).ok()?.peel_to_commit().ok()?.id());
        let Some(new) = change.new.and_then(commit) else {
            return Ok((Vec::new(), 0, false));
        };
        let old = change.old.and_then(commit);
        let mut walk = self.repo.revwalk()?;
        walk.push(new)?;
        match old {
            Some(old) => walk.hide(old)?,
            None => {
                for tip in before.values() {
                    // Tags may point at non-commits; those can't be hidden.
                    let _ = walk.hide(*tip);
                }
            }
        }
        let ids: Vec<Oid> = walk.take(count_to).collect::<std::result::Result<_, _>>()?;
        let forced = match old {
            Some(old) => !self.is_ancestor(old, new)?,
            None => false,
        };
        let mut commits = ids
            .iter()
            .take(limit)
            .map(|id| self.commit_detail(*id))
            .collect::<Result<Vec<_>>>()?;
        commits.reverse();
        Ok((commits, ids.len(), forced))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read::tests::Scratch;

    #[test]
    fn push_changes() {
        let s = Scratch::new();
        s.write("a", "1");
        let first = s.commit("first");
        let repo = s.open();
        let before = repo.refs_snapshot().unwrap();
        assert_eq!(before.get("refs/heads/main"), Some(&first));

        s.write("a", "2");
        let second = s.commit("second");
        s.git(&["tag", "v1"]);
        s.git(&["branch", "topic"]);
        let after = repo.refs_snapshot().unwrap();
        let changes = ref_changes(&before, &after);
        let names: Vec<&str> = changes.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["refs/heads/main", "refs/heads/topic", "refs/tags/v1"]);

        let (commits, total, forced) = repo.pushed_commits(&changes[0], &before, 10, 10_000).unwrap();
        assert_eq!((commits.len(), total, forced), (1, 1, false));
        assert_eq!(commits[0].commit.id, second);
        // A new branch only brings what no ref had before.
        let (commits, total, _) = repo.pushed_commits(&changes[1], &before, 10, 10_000).unwrap();
        assert_eq!((commits.len(), total), (1, 1));

        // Rewinding is a forced update with no new commits.
        s.git(&["reset", "-q", "--hard", &first.to_string()]);
        let rewound = ref_changes(&after, &repo.refs_snapshot().unwrap());
        let (commits, _, forced) = repo.pushed_commits(&rewound[0], &after, 10, 10_000).unwrap();
        assert!(commits.is_empty() && forced);

        s.git(&["branch", "-D", "topic"]);
        let deleted = ref_changes(&after, &repo.refs_snapshot().unwrap());
        assert!(deleted.iter().any(|c| c.name == "refs/heads/topic" && c.new.is_none()));
    }

    #[test]
    fn tags_of_files_add_no_commits() {
        let s = Scratch::new();
        s.write("key.pub", "a public key");
        s.commit("first");
        let repo = s.open();
        let before = repo.refs_snapshot().unwrap();
        // As git.git's `junio-gpg-pub`: a tag whose object is a file.
        let blob = s.git(&["rev-parse", "HEAD:key.pub"]);
        s.git(&["tag", "-a", "key", "-m", "the key", &blob]);
        s.git(&["tag", "plain", &blob]);
        s.write("a", "1");
        let second = s.commit("second");
        let after = repo.refs_snapshot().unwrap();
        let changes = ref_changes(&before, &after);
        let names: Vec<&str> = changes.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["refs/heads/main", "refs/tags/key", "refs/tags/plain"]);
        // Neither breaks the push they came in.
        let (commits, total, _) = repo.pushed_commits(&changes[0], &before, 10, 10_000).unwrap();
        assert_eq!((commits[0].commit.id, total), (second, 1));
        for tag in &changes[1..] {
            let (commits, total, forced) = repo.pushed_commits(tag, &before, 10, 10_000).unwrap();
            assert!(commits.is_empty() && total == 0 && !forced);
        }
        // Counting stops where it's told to.
        let all = RefChange { name: "refs/heads/new".into(), old: None, new: Some(second) };
        let (commits, total, _) = repo.pushed_commits(&all, &Refs::new(), 10, 1).unwrap();
        assert_eq!((commits.len(), total), (1, 1));
    }
}
