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

/// The largest file [`GitRepo::commit_file_edit`] changes: larger ones aren't
/// shown line by line anywhere, so nothing asks for it.
const MAX_EDITED_BYTES: usize = 16 * 1024 * 1024;

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
        // Edited in memory: not something for a file that large.
        if self.blob_size(entry.id())? > MAX_EDITED_BYTES {
            return Err(Error::Other(format!("{path} is too large to change here")));
        }
        let blob = self.repo.find_blob(entry.id())?;
        let content = edit(blob.content(), entry.id())?;
        let new_blob = self.repo.blob(&content)?;
        let new_tree = git2::build::TreeUpdateBuilder::new()
            .upsert(path, new_blob, mode)
            .create_updated(&self.repo, &tree)?;
        let new_tree = self.repo.find_tree(new_tree)?;
        let sig = git2::Signature::now(who.name, who.email)?;
        let commit = self.create_commit(&sig, &sig, message, &new_tree, &[&parent], who)?;
        // Compare-and-swap, so a concurrent push isn't overwritten.
        self.update_ref(&format!("refs/heads/{branch}"), Some(commit), Some(tip))?;
        Ok(commit)
    }
}

/// A file to write or delete in a commit.
#[derive(Clone, Copy, Debug)]
pub struct FileChange<'a> {
    pub path: &'a str,
    /// The new contents; `None` deletes the file.
    pub content: Option<&'a [u8]>,
    /// Keep the mode (e.g. executable) of this file in the parent: the
    /// path itself, or where a renamed file was.
    pub mode_of: Option<&'a str>,
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
        let changes: Vec<FileChange<'_>> =
            changes.iter().map(|(path, content)| FileChange { path, content: *content, mode_of: Some(path) }).collect();
        self.commit_changes(branch, tip, tip, &changes, message, who)
    }

    /// Commits `changes` on top of `parent` (none: a first commit) as the
    /// new tip of `branch`, which must still point at `old_tip`, or not
    /// exist yet if that's `None`. Returns the new commit.
    pub fn commit_changes(
        &self,
        branch: &str,
        parent: Option<Oid>,
        old_tip: Option<Oid>,
        changes: &[FileChange<'_>],
        message: &str,
        who: &Identity<'_>,
    ) -> Result<Oid> {
        let parent = parent.map(|t| self.repo.find_commit(t)).transpose()?;
        let base = match &parent {
            Some(p) => p.tree()?,
            None => self.repo.find_tree(self.repo.treebuilder(None)?.write()?)?,
        };
        let mode = |path: Option<&str>| {
            let entry = path.and_then(|p| base.get_path(std::path::Path::new(p)).ok());
            match entry.map(|e| e.filemode()) {
                Some(m) if m == i32::from(git2::FileMode::BlobExecutable) => git2::FileMode::BlobExecutable,
                _ => git2::FileMode::Blob,
            }
        };
        let mut update = git2::build::TreeUpdateBuilder::new();
        for c in changes {
            match c.content {
                Some(data) => update.upsert(c.path, self.repo.blob(data)?, mode(c.mode_of)),
                None => update.remove(c.path),
            };
        }
        let tree = self.repo.find_tree(update.create_updated(&self.repo, &base)?)?;
        // A file can't also be a directory: libgit2 would quietly keep both
        // entries, or replace one with the other.
        for c in changes.iter().filter(|c| c.content.is_some()) {
            let ok = matches!(tree.get_path(std::path::Path::new(c.path)), Ok(e) if e.kind() == Some(git2::ObjectType::Blob));
            if !ok {
                return Err(Error::Other(format!("{} can't be both a file and a folder", c.path)));
            }
        }
        let sig = git2::Signature::now(who.name, who.email)?;
        let parents: Vec<&git2::Commit<'_>> = parent.iter().collect();
        let commit = self.create_commit(&sig, &sig, message, &tree, &parents, who)?;
        // Compare-and-swap; the zero id means "must not exist yet".
        self.update_ref(&format!("refs/heads/{branch}"), Some(commit), Some(old_tip.unwrap_or(Oid::ZERO_SHA1)))?;
        Ok(commit)
    }
}

impl GitRepo {
    /// Starts this empty repository from `template`'s files: for each of
    /// its `branches`, a branch here with a single commit holding the same
    /// files, without the template's history. HEAD points at `head`.
    pub fn start_from_template(&self, template: &GitRepo, branches: &[String], head: &str, message: &str, who: &Identity<'_>) -> Result<()> {
        let src = &template.repo;
        let mut trees = Vec::new();
        let mut pack = src.packbuilder()?;
        for branch in branches {
            let tree = src.find_reference(&format!("refs/heads/{branch}"))?.peel_to_commit()?.tree_id();
            // Recursively, skipping submodules.
            pack.insert_tree(tree)?;
            trees.push((branch, tree));
        }
        if trees.is_empty() {
            return Ok(());
        }
        let mut buf = git2::Buf::new();
        pack.write_buf(&mut buf)?;
        let odb = self.repo.odb()?;
        let mut writer = odb.packwriter()?;
        std::io::Write::write_all(&mut writer, &buf)?;
        writer.commit()?;
        let sig = git2::Signature::now(who.name, who.email)?;
        for (branch, tree) in trees {
            let tree = self.repo.find_tree(tree)?;
            let commit = self.create_commit(&sig, &sig, message, &tree, &[], who)?;
            self.update_ref(&format!("refs/heads/{branch}"), Some(commit), Some(Oid::ZERO_SHA1))?;
        }
        self.repo.set_head(&format!("refs/heads/{head}"))?;
        Ok(())
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
        let who = Identity::new("Ada", "ada@example.com");
        let first = repo.commit_files("main", None, &[("Home.md", Some(b"# Hi"))], "Create Home", &who).unwrap();
        assert!(repo.commit_files("main", None, &[("x.md", Some(b""))], "Again", &who).is_err(), "exists now");
        let second = repo
            .commit_files("main", Some(first), &[("Home.md", None), ("Start.md", Some(b"go"))], "Rename", &who)
            .unwrap();
        assert_eq!(repo.branch_tip("main"), Some(second));
        assert!(repo.blob(second, "Home.md", usize::MAX).unwrap().is_none());
        assert_eq!(repo.blob(second, "Start.md", usize::MAX).unwrap().unwrap().data, b"go");

        // Executable files stay executable, also when renamed; a new branch
        // starts from another one's commit.
        let tree = repo.repo.find_commit(second).unwrap().tree().unwrap();
        let mut b = repo.repo.treebuilder(Some(&tree)).unwrap();
        b.insert("run.sh", repo.repo.blob(b"echo").unwrap(), 0o100755).unwrap();
        let tree = repo.repo.find_tree(b.write().unwrap()).unwrap();
        let sig = git2::Signature::now("Ada", "ada@example.com").unwrap();
        let parent = repo.repo.find_commit(second).unwrap();
        let third = repo.repo.commit(Some("refs/heads/main"), &sig, &sig, "Script", &tree, &[&parent]).unwrap();
        let rename = [
            FileChange { path: "run.sh", content: None, mode_of: None },
            FileChange { path: "bin/run.sh", content: Some(b"echo hi"), mode_of: Some("run.sh") },
        ];
        let fourth = repo.commit_changes("topic", Some(third), None, &rename, "Move", &who).unwrap();
        assert_eq!(repo.branch_tip("topic"), Some(fourth));
        assert_eq!(repo.branch_tip("main"), Some(third));
        let tree = repo.repo.find_commit(fourth).unwrap().tree().unwrap();
        assert_eq!(tree.get_path(std::path::Path::new("bin/run.sh")).unwrap().filemode(), 0o100755);
        // Files and folders don't mix.
        let clash = [FileChange { path: "bin/run.sh/x", content: Some(b""), mode_of: None }];
        assert!(repo.commit_changes("topic", Some(fourth), Some(fourth), &clash, "Clash", &who).is_err());
        let clash = [FileChange { path: "bin", content: Some(b""), mode_of: None }];
        assert!(repo.commit_changes("topic", Some(fourth), Some(fourth), &clash, "Clash", &who).is_err());
        assert!(repo.commit_files("main", Some(first), &[("y.md", Some(b""))], "Stale", &who).is_err());
    }

    #[test]
    fn committing_an_edit() {
        let s = Scratch::new();
        s.write("src/lib.rs", "one\ntwo\n");
        let tip = s.commit("first");
        let repo = s.open();
        let who = Identity::new("Ada", "ada@example.com");
        let edit = |text: &[u8], _: Oid| Ok(replace_line(std::str::from_utf8(text).unwrap(), 2, "2").unwrap().into_bytes());
        let commit = repo.commit_file_edit("main", tip, "src/lib.rs", "Apply", &who, edit).unwrap();
        assert_eq!(repo.branch_tip("main"), Some(commit));
        assert_eq!(repo.blob(commit, "src/lib.rs", usize::MAX).unwrap().unwrap().data, b"one\n2\n");
        assert_eq!(repo.commit_summary(commit).unwrap().summary, "Apply");

        // The branch moved meanwhile: nothing is overwritten.
        assert!(repo.commit_file_edit("main", tip, "src/lib.rs", "Again", &who, edit).is_err());
        assert_eq!(repo.branch_tip("main"), Some(commit));
        assert!(repo.commit_file_edit("main", commit, "src", "Dir", &who, edit).is_err());
    }

    /// Signs with a made-up signature that says what it signed.
    struct Fake;

    impl crate::Signer for Fake {
        fn sign(&self, commit: &[u8]) -> std::result::Result<String, String> {
            let hex: String = commit.iter().take(8).map(|b| format!("{b:02x}")).collect();
            Ok(format!("-----BEGIN SSH SIGNATURE-----\n{hex}\n-----END SSH SIGNATURE-----"))
        }
    }

    #[test]
    fn signed_commits() {
        let s = Scratch::new();
        s.write("a.txt", "a\n");
        let tip = s.commit("first");
        let repo = s.open();
        let who = Identity::new("Ada", "ada@example.com").signed_by(&Fake);
        let commit = repo.commit_files("main", Some(tip), &[("b.txt", Some(b"b"))], "Add b", &who).unwrap();
        let found = repo.signed_commits(&[commit]).unwrap();
        assert_eq!(found.len(), 1);
        // The signature covers the commit as git checks it: without itself.
        assert_eq!(found[0].signature, crate::Signer::sign(&Fake, &found[0].payload).unwrap().into_bytes());
        assert_eq!(s.git(&["show", "-s", "--format=%s %an", &commit.to_string()]), "Add b Ada");
        s.git(&["fsck", "--strict"]);
    }

    #[test]
    fn starting_from_a_template() {
        let s = Scratch::new();
        s.write("README.md", "# Template");
        s.write("src/main.rs", "fn main() {}");
        s.commit("one");
        s.write("README.md", "# Template v2");
        s.commit("two");
        s.git(&["checkout", "-qb", "docs"]);
        s.write("docs/index.md", "docs");
        s.commit("docs");
        let template = s.open();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("new.git");
        crate::create_bare(&path, "main").unwrap();
        let repo = GitRepo::open(&path).unwrap();
        let who = Identity::new("Bob", "bob@example.com");
        let branches = ["main".to_owned(), "docs".to_owned()];
        repo.start_from_template(&template, &branches, "docs", "Initial commit", &who).unwrap();
        let main = repo.branch_tip("main").unwrap();
        assert_eq!(repo.repo.find_commit(main).unwrap().parent_count(), 0);
        let c = repo.commit_summary(main).unwrap();
        assert_eq!((c.summary.as_str(), c.author.name.as_str()), ("Initial commit", "Bob"));
        assert_eq!(repo.blob(main, "README.md", usize::MAX).unwrap().unwrap().data, b"# Template v2");
        assert_eq!(repo.blob(main, "src/main.rs", usize::MAX).unwrap().unwrap().data, b"fn main() {}");
        let docs = repo.branch_tip("docs").unwrap();
        assert_eq!(repo.blob(docs, "docs/index.md", usize::MAX).unwrap().unwrap().data, b"docs");
        assert_eq!(repo.default_branch().unwrap().as_deref(), Some("docs"));
        assert!(repo.repo.revparse_single("HEAD~1").is_err(), "no history");
    }
}
