//! Changing files from the browser: editing, creating, uploading and
//! deleting them, committed as the person doing it, to the branch or to a
//! new branch for a pull request. Branch protection applies as for pushes.

use keel_git::{FileChange, GitRepo, Identity, Oid};

use crate::{Access, Core, Error, Repo, Result, User, invalid};

/// Longest path accepted, in bytes.
const MAX_PATH: usize = 1024;

/// One file to write or delete.
#[derive(Clone, Debug)]
pub struct FileWrite {
    pub path: String,
    /// `None` deletes the file.
    pub content: Option<Vec<u8>>,
    /// Where the file was before (renaming it).
    pub from: Option<String>,
    /// The file must not exist yet (creating one, rather than uploading).
    pub new: bool,
}

/// A commit made from the browser.
#[derive(Clone, Debug)]
pub struct FileEdit {
    /// The branch the changes were made on.
    pub branch: String,
    /// The commit the person started from, to tell whether someone else
    /// changed the files since; `None` in an empty repository, or to not
    /// check.
    pub base: Option<Oid>,
    /// Commit to this new branch instead of `branch`.
    pub new_branch: Option<String>,
    pub message: String,
    pub files: Vec<FileWrite>,
}

/// A path for a file: no empty, `.` or `..` parts, nothing in `.git`.
pub fn clean_path(path: &str) -> Result<String> {
    let path = path.trim().trim_matches('/');
    if path.is_empty() {
        return invalid("Files need a name.");
    }
    if path.len() > MAX_PATH {
        return invalid("That path is too long.");
    }
    for part in path.split('/') {
        if part.is_empty() || part == "." || part == ".." || part.eq_ignore_ascii_case(".git") {
            return invalid(format!("“{path}” isn't a valid path."));
        }
        if part.chars().any(|c| c.is_control() || c == '\\') || part.len() > 255 {
            return invalid(format!("“{part}” isn't a valid file name."));
        }
    }
    Ok(path.to_owned())
}

/// Why the changes can't be committed on top of `tip`, if they can't.
fn conflict(g: &GitRepo, base: Option<Oid>, tip: Option<Oid>, files: &[FileWrite]) -> keel_git::Result<Option<String>> {
    let blob_id = |commit: Option<Oid>, path: &str| -> keel_git::Result<Option<Oid>> {
        Ok(match commit {
            Some(c) => g.blob(c, path)?.map(|b| b.id),
            None => None,
        })
    };
    for f in files {
        if (f.new || f.from.is_some()) && f.content.is_some() && blob_id(tip, &f.path)?.is_some() {
            return Ok(Some(format!("A file named {} already exists.", f.path)));
        }
        if f.content.is_some() && tip.map(|t| g.is_dir(t, &f.path)).transpose()?.unwrap_or(false) {
            return Ok(Some(format!("{} is a folder.", f.path)));
        }
        // Changed by someone else since the person started? (Unknown
        // without a base.)
        if base.is_some() && base != tip {
            for path in std::iter::once(&f.path).chain(&f.from) {
                if !f.new && blob_id(base, path)? != blob_id(tip, path)? {
                    return Ok(Some(format!(
                        "Someone changed {path} while you were working on it. Copy your changes, reload and make them again."
                    )));
                }
            }
        }
        if f.content.is_none() && blob_id(tip, &f.path)?.is_none() {
            return Ok(Some(format!("{} doesn't exist (any more).", f.path)));
        }
    }
    Ok(None)
}

impl Core {
    /// Commits `edit` as `user`. Returns the branch committed to and the
    /// new commit.
    pub async fn commit_file_edit(&self, repo: &Repo, user: &User, mut edit: FileEdit) -> Result<(String, Oid)> {
        if self.access(repo, Some(user)).await? < Access::Write {
            return Err(Error::NotFound);
        }
        if self.is_mirror(repo.id).await? {
            return invalid("This repository is a mirror; change the repository it mirrors.");
        }
        if edit.files.is_empty() {
            return invalid("There's nothing to commit.");
        }
        for f in &mut edit.files {
            f.path = clean_path(&f.path)?;
            if let Some(from) = &f.from {
                let from = clean_path(from)?;
                f.from = (from != f.path).then_some(from);
            }
        }
        let message = edit.message.trim().replace("\r\n", "\n");
        if message.is_empty() {
            return invalid("Commits need a message.");
        }
        // "A new branch" named like the branch itself is that branch, and
        // its protection applies.
        let new_branch = edit.new_branch.take().filter(|name| name.trim() != edit.branch);
        edit.new_branch = new_branch;
        let to_new_branch = edit.new_branch.is_some();
        let target = match edit.new_branch.as_deref().map(str::trim) {
            Some(name) => {
                if !keel_git::valid_branch_name(name) {
                    return invalid(format!("“{name}” isn't a valid branch name."));
                }
                name.to_owned()
            }
            None => {
                if !self.may_update_branch(repo.id, &edit.branch, false, false).await? {
                    return invalid(format!(
                        "{} is protected: commit to a new branch and open a pull request instead.",
                        edit.branch
                    ));
                }
                edit.branch.clone()
            }
        };
        let before = self.ref_snapshot(repo.id).await?;
        let (name, email) = (user.username.clone(), user.email.clone());
        let to = target.clone();
        let committed = self
            .git(repo.id, move |g| {
                let tip = g.branch_tip(&edit.branch);
                if tip.is_none() && (edit.base.is_some() || g.has_branches()?) {
                    return Ok(Err(format!("The branch {} doesn't exist (any more).", edit.branch)));
                }
                if to_new_branch && g.branch_tip(&to).is_some() {
                    return Ok(Err(format!("A branch named {to} already exists.")));
                }
                if let Some(why) = conflict(g, edit.base, tip, &edit.files)? {
                    return Ok(Err(why));
                }
                let mut changes = Vec::new();
                for f in &edit.files {
                    if let Some(from) = &f.from {
                        changes.push(FileChange { path: from, content: None, mode_of: None });
                    }
                    let mode_of = Some(f.from.as_deref().unwrap_or(&f.path));
                    changes.push(FileChange { path: &f.path, content: f.content.as_deref(), mode_of });
                }
                let who = Identity { name: &name, email: &email };
                let old = if to_new_branch { None } else { tip };
                match g.commit_changes(&to, tip, old, &changes, &message, &who) {
                    Ok(commit) => Ok(Ok(commit)),
                    Err(keel_git::Error::Other(msg)) => Ok(Err(format!("Can't commit: {msg}."))),
                    // E.g. a push moved the branch meanwhile.
                    Err(_) if g.branch_tip(&to) != old => Ok(Err(
                        "The branch changed while you were working. Copy your changes, reload and make them again.".to_owned(),
                    )),
                    Err(e) => Err(e),
                }
            })
            .await?;
        let commit = committed.or_else(invalid)?;
        self.after_push(repo.id, Some(user.id), Some(before)).await?;
        Ok((target, commit))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(path: &str, text: &str) -> FileWrite {
        FileWrite { path: path.into(), content: Some(text.as_bytes().to_vec()), from: None, new: false }
    }

    #[test]
    fn paths() {
        assert_eq!(clean_path(" /docs/a b.md/ ").unwrap(), "docs/a b.md");
        for bad in ["", "/", "a//b", "../x", "a/./b", ".git/config", "a/.GIT/x", "a\\b", "x\ny"] {
            assert!(clean_path(bad).is_err(), "{bad:?}");
        }
    }

    #[tokio::test]
    async fn editing_files() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let bob = core.register("bob", "bob@example.com", "password1").await.unwrap();
        let repo = core.create_repo(&ada, "engine", "", false).await.unwrap();
        let edit = |branch: &str, base, files: Vec<FileWrite>| FileEdit {
            branch: branch.into(),
            base,
            new_branch: None,
            message: "Change".into(),
            files,
        };
        let read = |rev: Oid, path: &str| {
            let g = GitRepo::open(&core.repo_path(&repo)).unwrap();
            g.blob(rev, path).unwrap().map(|b| String::from_utf8(b.data).unwrap())
        };

        // The first file of an empty repository.
        let new_readme = FileWrite { new: true, ..write("README.md", "# Engine\n") };
        let (branch, first) = core.commit_file_edit(&repo, &ada, edit("main", None, vec![new_readme.clone()])).await.unwrap();
        assert_eq!(branch, "main");
        assert_eq!(read(first, "README.md").as_deref(), Some("# Engine\n"));
        assert!(core.commit_file_edit(&repo, &ada, edit("main", Some(first), vec![new_readme])).await.is_err(), "exists");
        // Readers can't.
        assert!(matches!(core.commit_file_edit(&repo, &bob, edit("main", Some(first), vec![write("a", "")])).await, Err(Error::NotFound)));

        // Edit and rename; then an edit based on the old version conflicts.
        let moved = FileWrite { from: Some("README.md".into()), ..write("docs/README.md", "# Engine!\n") };
        let (_, second) = core.commit_file_edit(&repo, &ada, edit("main", Some(first), vec![moved])).await.unwrap();
        assert_eq!(read(second, "README.md"), None);
        assert_eq!(read(second, "docs/README.md").as_deref(), Some("# Engine!\n"));
        let stale = edit("main", Some(first), vec![write("README.md", "late")]);
        let err = core.commit_file_edit(&repo, &ada, stale).await.unwrap_err();
        assert!(err.to_string().contains("Someone changed"), "{err}");
        let onto = FileWrite { from: Some("docs/README.md".into()), ..write("NOTES", "x") };
        let (_, notes) = core.commit_file_edit(&repo, &ada, edit("main", Some(second), vec![write("NOTES", "n")])).await.unwrap();
        assert!(core.commit_file_edit(&repo, &ada, edit("main", Some(notes), vec![onto])).await.is_err(), "renaming onto a file");
        // Other files may have changed meanwhile.
        let (_, third) = core.commit_file_edit(&repo, &ada, edit("main", Some(first), vec![write("LICENSE", "MIT")])).await.unwrap();

        // To a new branch; protected branches only allow that.
        core.save_branch_rule(repo.id, &crate::BranchRule {
            id: 0,
            pattern: "main".into(),
            require_pull: true,
            required_approvals: 0,
            dismiss_stale: false,
            allow_force_push: false,
            required_checks: String::new(),
        })
            .await
            .unwrap();
        let direct = core.commit_file_edit(&repo, &ada, edit("main", Some(third), vec![write("x", "1")])).await.unwrap_err();
        assert!(direct.to_string().contains("protected"), "{direct}");
        // Also when "committing to a new branch" named like the protected one.
        for name in ["main", " main "] {
            let mut sneaky = edit("main", Some(third), vec![write("x", "1")]);
            sneaky.new_branch = Some(name.into());
            let err = core.commit_file_edit(&repo, &ada, sneaky).await.unwrap_err();
            assert!(err.to_string().contains("protected"), "{err}");
        }
        let g = GitRepo::open(&core.repo_path(&repo)).unwrap();
        assert_eq!(g.branch_tip("main"), Some(third));
        let mut branched = edit("main", Some(third), vec![write("x", "1")]);
        branched.new_branch = Some("ada-patch-1".into());
        let (branch, commit) = core.commit_file_edit(&repo, &ada, branched.clone()).await.unwrap();
        assert_eq!(branch, "ada-patch-1");
        assert_eq!(read(commit, "NOTES").as_deref(), Some("n"));
        assert!(core.commit_file_edit(&repo, &ada, branched).await.is_err(), "the branch exists now");

        // Deleting.
        let mut delete = edit("ada-patch-1", Some(commit), vec![FileWrite { content: None, ..write("x", "") }]);
        delete.message = "Delete x".into();
        let (_, deleted) = core.commit_file_edit(&repo, &ada, delete.clone()).await.unwrap();
        assert_eq!(read(deleted, "x"), None);
        delete.base = Some(deleted);
        assert!(core.commit_file_edit(&repo, &ada, delete).await.is_err(), "already gone");
    }
}
