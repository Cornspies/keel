use git2::{Delta, DiffFindOptions, DiffOptions, Oid, Patch};

use crate::{GitRepo, Result};

/// git's default `diff.renameLimit`: once added × deleted files are more
/// than its square, renames are only found where the content is the same.
const RENAME_LIMIT: usize = 1000;
/// Files larger than this are shown as binary: changed, without their
/// lines. Diffing one means holding both versions in memory, and a diff
/// that long isn't read anyway.
const MAX_DIFF_FILE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffStatus {
    Added,
    Deleted,
    Modified,
    Renamed,
    Copied,
    TypeChange,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiffLineKind {
    Context,
    Added,
    Removed,
    /// "\ No newline at end of file"
    NoNewline,
}

#[derive(Clone, Debug)]
pub struct DiffLine {
    pub kind: DiffLineKind,
    pub old_no: Option<u32>,
    pub new_no: Option<u32>,
    pub content: String,
}

#[derive(Clone, Debug)]
pub struct Hunk {
    pub header: String,
    pub lines: Vec<DiffLine>,
}

#[derive(Clone, Debug)]
pub struct FileDiff {
    pub old_path: Option<String>,
    pub new_path: Option<String>,
    pub status: DiffStatus,
    /// Blob ids of the two sides (`None` for an added or deleted side).
    pub old_id: Option<Oid>,
    pub new_id: Option<Oid>,
    pub additions: usize,
    pub deletions: usize,
    pub binary: bool,
    /// The patch was too large to include; only stats are available.
    pub collapsed: bool,
    pub hunks: Vec<Hunk>,
}

impl FileDiff {
    /// The path to show for this file (the new one, unless it was deleted).
    pub fn path(&self) -> &str {
        self.new_path
            .as_deref()
            .or(self.old_path.as_deref())
            .unwrap_or_default()
    }
}

#[derive(Clone, Debug, Default)]
pub struct CommitDiff {
    pub files: Vec<FileDiff>,
    pub additions: usize,
    pub deletions: usize,
    /// More files changed than `max_files`; the rest were left out.
    pub truncated: bool,
}

impl GitRepo {
    /// Diff of `commit` against its first parent (or the empty tree for a
    /// root commit). Rendering huge diffs is what makes code hosts slow, so
    /// the work is bounded: at most `max_files` files, and once `max_lines`
    /// diff lines have been collected, further files are reported with
    /// stats only.
    pub fn diff_commit(&self, commit: Oid, max_files: usize, max_lines: usize) -> Result<CommitDiff> {
        let parent = self.repo.find_commit(commit)?.parent_ids().next();
        self.diff_range(parent, commit, max_files, max_lines)
    }

    /// Diff between two commits (`old` = `None` for the empty tree), with
    /// the same bounds as [`diff_commit`](Self::diff_commit).
    pub fn diff_range(&self, old: Option<Oid>, new: Oid, max_files: usize, max_lines: usize) -> Result<CommitDiff> {
        self.diff_filtered(old, new, max_files, max_lines, None)
    }

    /// One file of [`diff_range`](Self::diff_range), by its displayed path.
    /// Renames are detected across the whole diff first, so the file looks
    /// exactly as it does there.
    pub fn diff_file(&self, old: Option<Oid>, new: Oid, path: &str, max_lines: usize) -> Result<Option<FileDiff>> {
        Ok(self.diff_filtered(old, new, 1, max_lines, Some(path))?.files.pop())
    }

    fn diff_filtered(
        &self,
        old: Option<Oid>,
        new: Oid,
        max_files: usize,
        max_lines: usize,
        only: Option<&str>,
    ) -> Result<CommitDiff> {
        let new_tree = self.repo.find_commit(new)?.tree()?;
        let old_tree = match old {
            Some(c) => Some(self.repo.find_commit(c)?.tree()?),
            None => None,
        };

        let mut opts = DiffOptions::new();
        opts.context_lines(3).ignore_submodules(true).max_size(MAX_DIFF_FILE_BYTES as i64);
        let mut diff = self
            .repo
            .diff_tree_to_tree(old_tree.as_ref(), Some(&new_tree), Some(&mut opts))?;
        // Finding renames compares every added file with every deleted
        // one: past git's limit (`diff.renameLimit`), only exact renames,
        // as git does.
        let (mut added, mut deleted) = (0usize, 0usize);
        for delta in diff.deltas() {
            match delta.status() {
                Delta::Added => added += 1,
                Delta::Deleted => deleted += 1,
                _ => {}
            }
        }
        // Comparing reads the files whole, so also when one is large.
        let large = added > 0 && deleted > 0 && {
            let odb = self.repo.odb()?;
            let large = |id: Oid| odb.read_header(id).is_ok_and(|(size, _)| size > MAX_DIFF_FILE_BYTES);
            diff.deltas().any(|delta| match delta.status() {
                Delta::Added => large(delta.new_file().id()),
                Delta::Deleted => large(delta.old_file().id()),
                _ => false,
            })
        };
        let mut find = DiffFindOptions::new();
        find.renames(true).exact_match_only(large || added.saturating_mul(deleted) > RENAME_LIMIT * RENAME_LIMIT);
        diff.find_similar(Some(&mut find))?;

        let mut out = CommitDiff::default();
        let mut budget = max_lines;
        for idx in 0..diff.deltas().len() {
            if out.files.len() == max_files {
                out.truncated = true;
                break;
            }
            if let Some(only) = only {
                let delta = diff.get_delta(idx).expect("index in range");
                let shown = match delta.status() {
                    Delta::Deleted => delta.old_file(),
                    _ => delta.new_file(),
                };
                if shown.path_bytes() != Some(only.as_bytes()) {
                    continue;
                }
            }
            let Some(patch) = Patch::from_diff(&diff, idx)? else {
                continue;
            };
            let file = file_diff(&patch, &mut budget)?;
            out.additions += file.additions;
            out.deletions += file.deletions;
            out.files.push(file);
        }
        Ok(out)
    }
}

fn path_of(file: git2::DiffFile<'_>) -> Option<String> {
    file.path_bytes()
        .map(|p| String::from_utf8_lossy(p).into_owned())
}

fn file_diff(patch: &Patch<'_>, budget: &mut usize) -> Result<FileDiff> {
    let delta = patch.delta();
    let status = match delta.status() {
        Delta::Added | Delta::Untracked => DiffStatus::Added,
        Delta::Deleted => DiffStatus::Deleted,
        Delta::Renamed => DiffStatus::Renamed,
        Delta::Copied => DiffStatus::Copied,
        Delta::Typechange => DiffStatus::TypeChange,
        _ => DiffStatus::Modified,
    };
    let old_path = (status != DiffStatus::Added).then(|| path_of(delta.old_file())).flatten();
    let new_path = (status != DiffStatus::Deleted).then(|| path_of(delta.new_file())).flatten();
    let (_, additions, deletions) = patch.line_stats()?;
    let binary = delta.flags().is_binary();

    let total_lines: usize = (0..patch.num_hunks())
        .map(|h| patch.num_lines_in_hunk(h).unwrap_or(0))
        .sum();
    let collapsed = !binary && total_lines > *budget;

    let mut hunks = Vec::new();
    if !binary && !collapsed {
        *budget -= total_lines;
        for h in 0..patch.num_hunks() {
            let (hunk, n) = patch.hunk(h)?;
            let mut lines = Vec::with_capacity(n);
            for l in 0..n {
                let line = patch.line_in_hunk(h, l)?;
                let kind = match line.origin() {
                    '+' => DiffLineKind::Added,
                    '-' => DiffLineKind::Removed,
                    ' ' => DiffLineKind::Context,
                    _ => DiffLineKind::NoNewline,
                };
                let content = if kind == DiffLineKind::NoNewline {
                    "\\ No newline at end of file".to_owned()
                } else {
                    let s = String::from_utf8_lossy(line.content());
                    s.strip_suffix('\n').unwrap_or(&s).to_owned()
                };
                lines.push(DiffLine {
                    kind,
                    old_no: line.old_lineno(),
                    new_no: line.new_lineno(),
                    content,
                });
            }
            hunks.push(Hunk {
                header: String::from_utf8_lossy(hunk.header()).trim_end().to_owned(),
                lines,
            });
        }
    }

    let id = |f: git2::DiffFile<'_>| Some(f.id()).filter(|id| !id.is_zero());
    Ok(FileDiff {
        old_id: (status != DiffStatus::Added).then(|| id(delta.old_file())).flatten(),
        new_id: (status != DiffStatus::Deleted).then(|| id(delta.new_file())).flatten(),
        old_path,
        new_path,
        status,
        additions,
        deletions,
        binary,
        collapsed,
        hunks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read::tests::Scratch;

    #[test]
    fn diff_of_commit() {
        let s = Scratch::new();
        s.write("a.txt", "one\ntwo\nthree\n");
        s.write("old.txt", "some content that is long enough to be detected as a rename\n");
        let root = s.commit("root");
        s.write("a.txt", "one\n2\nthree\n");
        std::fs::rename(s.dir.path().join("old.txt"), s.dir.path().join("new.txt")).unwrap();
        s.write("bin.dat", "\0\x01\x02");
        let c = s.commit("change");
        let repo = s.open();

        let d = repo.diff_commit(root, 100, 1000).unwrap();
        assert_eq!(d.files.len(), 2);
        assert!(d.files.iter().all(|f| f.status == DiffStatus::Added));

        let d = repo.diff_commit(c, 100, 1000).unwrap();
        assert_eq!((d.additions, d.deletions), (1, 1));
        let a = d.files.iter().find(|f| f.path() == "a.txt").unwrap();
        let changed: Vec<_> = a.hunks[0]
            .lines
            .iter()
            .filter(|l| l.kind != DiffLineKind::Context)
            .map(|l| (l.kind, l.content.as_str()))
            .collect();
        assert_eq!(changed, [(DiffLineKind::Removed, "two"), (DiffLineKind::Added, "2")]);
        let renamed = d.files.iter().find(|f| f.path() == "new.txt").unwrap();
        assert_eq!(renamed.status, DiffStatus::Renamed);
        assert_eq!(renamed.old_path.as_deref(), Some("old.txt"));
        assert!(d.files.iter().find(|f| f.path() == "bin.dat").unwrap().binary);

        assert!(renamed.old_id.is_some() && renamed.new_id.is_some());

        let one = repo.diff_file(Some(root), c, "new.txt", 1000).unwrap().unwrap();
        assert_eq!(one.status, DiffStatus::Renamed, "rename detection still applies");
        assert!(repo.diff_file(Some(root), c, "old.txt", 1000).unwrap().is_none());

        let d = repo.diff_commit(c, 1, 1000).unwrap();
        assert!(d.truncated);

        // A very large file is reported as changed, without its lines
        // (and without being read).
        s.write("big.txt", &"line\n".repeat(MAX_DIFF_FILE_BYTES / 5 + 1));
        s.write("old.txt", "back again\n");
        std::fs::remove_file(s.dir.path().join("new.txt")).unwrap();
        let big = s.commit("a large file");
        let d = s.open().diff_commit(big, 100, 1000).unwrap();
        let file = d.files.iter().find(|f| f.path() == "big.txt").unwrap();
        assert!(file.binary && file.hunks.is_empty() && file.status == DiffStatus::Added);
        assert_eq!(d.files.len(), 3, "the others are still shown");
        let d = repo.diff_commit(c, 100, 2).unwrap();
        assert!(d.files.iter().find(|f| f.path() == "a.txt").unwrap().collapsed);
    }
}
