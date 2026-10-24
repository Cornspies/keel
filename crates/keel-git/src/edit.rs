//! Committing small edits without a working tree, e.g. applying a
//! suggested change from code review.

use git2::Oid;

use crate::{Error, GitRepo, Identity, Result};

/// Replaces line `line` (1-based) of `text` with `replacement` (which may
/// be empty, deleting the line), keeping the file's line endings. `None` if
/// there is no such line.
pub fn replace_line(text: &str, line: usize, replacement: &str) -> Option<String> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let target = *lines.get(line.checked_sub(1)?)?;
    let ending = if target.ends_with("\r\n") {
        "\r\n"
    } else if target.ends_with('\n') {
        "\n"
    } else {
        ""
    };
    let newline = if ending.is_empty() { "\n" } else { ending };
    let mut out = String::with_capacity(text.len() + replacement.len());
    for l in &lines[..line - 1] {
        out.push_str(l);
    }
    if !replacement.is_empty() {
        let new: Vec<&str> = replacement.lines().collect();
        for (i, l) in new.iter().enumerate() {
            out.push_str(l);
            out.push_str(if i + 1 == new.len() { ending } else { newline });
        }
    }
    for l in &lines[line..] {
        out.push_str(l);
    }
    Some(out)
}

impl GitRepo {
    /// Commits a new version of `path` on top of `branch`, which must still
    /// point at `tip`, and moves the branch. `edit` gets the file's current
    /// contents and blob id and returns the new contents, or an error to
    /// abort. Returns the new commit.
    pub fn commit_file_edit(
        &self,
        branch: &str,
        tip: Oid,
        path: &str,
        message: &str,
        who: &Identity<'_>,
        edit: impl FnOnce(&[u8], Oid) -> Result<Vec<u8>>,
    ) -> Result<Oid> {
        let parent = self.repo.find_commit(tip)?;
        let tree = parent.tree()?;
        let entry = tree.get_path(std::path::Path::new(path))?;
        if entry.kind() != Some(git2::ObjectType::Blob) {
            return Err(Error::Other(format!("{path} is not a file")));
        }
        let mode = match entry.filemode() {
            0o100755 => git2::FileMode::BlobExecutable,
            0o100644 => git2::FileMode::Blob,
            _ => return Err(Error::Other(format!("{path} is not a regular file"))),
        };
        let blob = self.repo.find_blob(entry.id())?;
        let content = edit(blob.content(), entry.id())?;
        let new_blob = self.repo.blob(&content)?;
        let new_tree = git2::build::TreeUpdateBuilder::new()
            .upsert(path, new_blob, mode)
            .create_updated(&self.repo, &tree)?;
        let new_tree = self.repo.find_tree(new_tree)?;
        let sig = git2::Signature::now(who.name, who.email)?;
        let commit = self.repo.commit(None, &sig, &sig, message, &new_tree, &[&parent])?;
        // Compare-and-swap, so a concurrent push isn't overwritten.
        self.update_ref(&format!("refs/heads/{branch}"), Some(commit), Some(tip))?;
        Ok(commit)
    }
}

impl GitRepo {
    /// Commits `changes` (`Some(contents)` writes a file, `None` deletes
    /// it) on top of `branch`, which must still point at `tip`, or not
    /// exist yet if `tip` is `None`. Returns the new commit.
    pub fn commit_files(
        &self,
        branch: &str,
        tip: Option<Oid>,
        changes: &[(&str, Option<&[u8]>)],
        message: &str,
        who: &Identity<'_>,
    ) -> Result<Oid> {
        let parent = tip.map(|t| self.repo.find_commit(t)).transpose()?;
        let base = match &parent {
            Some(p) => p.tree()?,
            None => self.repo.find_tree(self.repo.treebuilder(None)?.write()?)?,
        };
        let mut update = git2::build::TreeUpdateBuilder::new();
        for (path, content) in changes {
            match content {
                Some(data) => update.upsert(*path, self.repo.blob(data)?, git2::FileMode::Blob),
                None => update.remove(*path),
            };
        }
        let tree = self.repo.find_tree(update.create_updated(&self.repo, &base)?)?;
        let sig = git2::Signature::now(who.name, who.email)?;
        let parents: Vec<&git2::Commit<'_>> = parent.iter().collect();
        let commit = self.repo.commit(None, &sig, &sig, message, &tree, &parents)?;
        // Compare-and-swap; the zero id means "must not exist yet".
        self.update_ref(&format!("refs/heads/{branch}"), Some(commit), Some(tip.unwrap_or(Oid::ZERO_SHA1)))?;
        Ok(commit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read::tests::Scratch;

    #[test]
    fn replacing_lines() {
        assert_eq!(replace_line("a\nb\nc\n", 2, "B").as_deref(), Some("a\nB\nc\n"));
        assert_eq!(replace_line("a\nb\nc\n", 2, "B1\nB2").as_deref(), Some("a\nB1\nB2\nc\n"));
        assert_eq!(replace_line("a\nb\nc\n", 2, "").as_deref(), Some("a\nc\n"));
        assert_eq!(replace_line("a\r\nb\r\n", 1, "x\ny").as_deref(), Some("x\r\ny\r\nb\r\n"));
        assert_eq!(replace_line("a\nb", 2, "x\ny").as_deref(), Some("a\nx\ny"), "no final newline");
        assert_eq!(replace_line("a\n", 2, "x"), None);
        assert_eq!(replace_line("a\n", 0, "x"), None);
    }

    #[test]
    fn committing_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wiki.git");
        crate::create_bare(&path, "main").unwrap();
        let repo = GitRepo::open(&path).unwrap();
        let who = Identity { name: "Ada", email: "ada@example.com" };
        let first = repo.commit_files("main", None, &[("Home.md", Some(b"# Hi"))], "Create Home", &who).unwrap();
        assert!(repo.commit_files("main", None, &[("x.md", Some(b""))], "Again", &who).is_err(), "exists now");
        let second = repo
            .commit_files("main", Some(first), &[("Home.md", None), ("Start.md", Some(b"go"))], "Rename", &who)
            .unwrap();
        assert_eq!(repo.branch_tip("main"), Some(second));
        assert!(repo.blob(second, "Home.md").unwrap().is_none());
        assert_eq!(repo.blob(second, "Start.md").unwrap().unwrap().data, b"go");
        assert!(repo.commit_files("main", Some(first), &[("y.md", Some(b""))], "Stale", &who).is_err());
    }

    #[test]
    fn committing_an_edit() {
        let s = Scratch::new();
        s.write("src/lib.rs", "one\ntwo\n");
        let tip = s.commit("first");
        let repo = s.open();
        let who = Identity { name: "Ada", email: "ada@example.com" };
        let edit = |text: &[u8], _: Oid| Ok(replace_line(std::str::from_utf8(text).unwrap(), 2, "2").unwrap().into_bytes());
        let commit = repo.commit_file_edit("main", tip, "src/lib.rs", "Apply", &who, edit).unwrap();
        assert_eq!(repo.branch_tip("main"), Some(commit));
        assert_eq!(repo.blob(commit, "src/lib.rs").unwrap().unwrap().data, b"one\n2\n");
        assert_eq!(repo.commit_summary(commit).unwrap().summary, "Apply");

        // The branch moved meanwhile: nothing is overwritten.
        assert!(repo.commit_file_edit("main", tip, "src/lib.rs", "Again", &who, edit).is_err());
        assert_eq!(repo.branch_tip("main"), Some(commit));
        assert!(repo.commit_file_edit("main", commit, "src", "Dir", &who, edit).is_err());
    }
}
