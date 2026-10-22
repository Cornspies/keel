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
    /// and how many there are in total (counting stops at 10,000). For a
    /// new branch that's what no ref in `before` had. Also whether the
    /// update was forced (the old tip is no longer contained).
    pub fn pushed_commits(
        &self,
        change: &RefChange,
        before: &Refs,
        limit: usize,
    ) -> Result<(Vec<CommitDetail>, usize, bool)> {
        let Some(new) = change.new else {
            return Ok((Vec::new(), 0, false));
        };
        let mut walk = self.repo.revwalk()?;
        walk.push(new)?;
        match change.old {
            Some(old) => walk.hide(old)?,
            None => {
                for tip in before.values() {
                    // Tags may point at non-commits; those can't be hidden.
                    let _ = walk.hide(*tip);
                }
            }
        }
        let ids: Vec<Oid> = walk.take(10_000).collect::<std::result::Result<_, _>>()?;
        let forced = match change.old {
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

        let (commits, total, forced) = repo.pushed_commits(&changes[0], &before, 10).unwrap();
        assert_eq!((commits.len(), total, forced), (1, 1, false));
        assert_eq!(commits[0].commit.id, second);
        // A new branch only brings what no ref had before.
        let (commits, total, _) = repo.pushed_commits(&changes[1], &before, 10).unwrap();
        assert_eq!((commits.len(), total), (1, 1));

        // Rewinding is a forced update with no new commits.
        s.git(&["reset", "-q", "--hard", &first.to_string()]);
        let rewound = ref_changes(&after, &repo.refs_snapshot().unwrap());
        let (commits, _, forced) = repo.pushed_commits(&rewound[0], &after, 10).unwrap();
        assert!(commits.is_empty() && forced);

        s.git(&["branch", "-D", "topic"]);
        let deleted = ref_changes(&after, &repo.refs_snapshot().unwrap());
        assert!(deleted.iter().any(|c| c.name == "refs/heads/topic" && c.new.is_none()));
    }
}
