//! Listing files for indexing.

use git2::{Delta, DiffOptions, ObjectType, Oid, TreeWalkMode, TreeWalkResult};

use crate::{GitRepo, Result};

/// A regular file in a tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileRef {
    pub path: String,
    pub id: Oid,
}

/// Files that differ between two commits.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct FileChanges {
    /// Added or modified, with their new contents.
    pub changed: Vec<FileRef>,
    pub deleted: Vec<String>,
}

impl GitRepo {
    /// Every blob in the tree of `commit` (not submodules).
    pub fn files(&self, commit: Oid) -> Result<Vec<FileRef>> {
        let tree = self.repo.find_commit(commit)?.tree()?;
        let mut out = Vec::new();
        tree.walk(TreeWalkMode::PreOrder, |dir, entry| {
            if entry.kind() == Some(ObjectType::Blob) {
                let name = String::from_utf8_lossy(entry.name_bytes());
                out.push(FileRef {
                    path: format!("{dir}{name}"),
                    id: entry.id(),
                });
            }
            TreeWalkResult::Ok
        })?;
        Ok(out)
    }

    /// Files changed between `old` and `new` (renames as delete + add).
    pub fn changed_files(&self, old: Oid, new: Oid) -> Result<FileChanges> {
        let old_tree = self.repo.find_commit(old)?.tree()?;
        let new_tree = self.repo.find_commit(new)?.tree()?;
        let mut opts = DiffOptions::new();
        opts.ignore_submodules(true);
        let diff = self.repo.diff_tree_to_tree(Some(&old_tree), Some(&new_tree), Some(&mut opts))?;
        let mut out = FileChanges::default();
        for delta in diff.deltas() {
            let path = |f: git2::DiffFile<'_>| f.path_bytes().map(|p| String::from_utf8_lossy(p).into_owned());
            match delta.status() {
                Delta::Deleted => out.deleted.extend(path(delta.old_file())),
                _ => {
                    if let Some(p) = path(delta.new_file()) {
                        out.changed.push(FileRef { path: p, id: delta.new_file().id() });
                    }
                }
            }
        }
        Ok(out)
    }

    /// Size of a blob without loading it.
    pub fn blob_size(&self, id: Oid) -> Result<usize> {
        let (size, _) = self.repo.odb()?.read_header(id)?;
        Ok(size)
    }

    /// A blob's content and whether it looks binary.
    pub fn blob_by_id(&self, id: Oid) -> Result<Option<(Vec<u8>, bool)>> {
        match self.repo.find_blob(id) {
            Ok(b) => Ok(Some((b.content().to_vec(), b.is_binary()))),
            Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::read::tests::Scratch;

    #[test]
    fn lists_and_diffs_files() {
        let s = Scratch::new();
        s.write("a.txt", "1");
        s.write("dir/b.txt", "1");
        let c1 = s.commit("one");
        s.write("a.txt", "2");
        std::fs::remove_file(s.dir.path().join("dir/b.txt")).unwrap();
        s.write("c.txt", "x");
        let c2 = s.commit("two");
        let repo = s.open();

        let paths: Vec<String> = repo.files(c1).unwrap().into_iter().map(|f| f.path).collect();
        assert_eq!(paths, ["a.txt", "dir/b.txt"]);
        let changes = repo.changed_files(c1, c2).unwrap();
        let changed: Vec<&str> = changes.changed.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(changed, ["a.txt", "c.txt"]);
        assert_eq!(changes.deleted, ["dir/b.txt"]);
        let id = changes.changed[1].id;
        assert_eq!(repo.blob_size(id).unwrap(), 1);
        assert_eq!(repo.blob_by_id(id).unwrap().unwrap(), (b"x".to_vec(), false));
    }
}
