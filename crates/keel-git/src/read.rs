use std::collections::HashMap;
use std::path::Path;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};

use git2::{ObjectType, Oid, Repository, Sort, Tree};

use crate::{Error, Result};

/// A bare repository opened for reading.
///
/// libgit2 handles are cheap to open and not `Sync`, so callers open one per
/// request inside a blocking task instead of sharing them.
pub struct GitRepo {
    pub(crate) repo: Repository,
}

#[derive(Clone, Debug)]
pub struct Signature {
    pub name: String,
    pub email: String,
    /// Seconds since the Unix epoch.
    pub time: i64,
}

#[derive(Clone, Debug)]
pub struct CommitSummary {
    pub id: Oid,
    pub summary: String,
    pub author: Signature,
}

impl CommitSummary {
    pub fn short_id(&self) -> String {
        short(self.id)
    }
}

#[derive(Clone, Debug)]
pub struct CommitDetail {
    pub commit: CommitSummary,
    /// The message without the summary line.
    pub body: String,
    pub committer: Signature,
    pub parents: Vec<Oid>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntryKind {
    Dir,
    File,
    Executable,
    Symlink,
    Submodule,
}

#[derive(Clone, Debug)]
pub struct TreeEntry {
    pub name: String,
    pub kind: EntryKind,
    pub id: Oid,
}

#[derive(Clone, Debug)]
pub struct RefInfo {
    /// Short name, e.g. `main` or `v1.0`.
    pub name: String,
    pub commit: CommitSummary,
}

/// The result of splitting a URL tail like `feature/x/src/lib.rs` into a
/// revision and a path inside the tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Resolved {
    pub rev: String,
    pub commit: Oid,
    pub path: String,
}

pub struct Blob {
    pub id: Oid,
    pub data: Vec<u8>,
    pub binary: bool,
}

pub(crate) fn short(id: Oid) -> String {
    let mut s = id.to_string();
    s.truncate(10);
    s
}

fn signature(sig: &git2::Signature<'_>) -> Signature {
    Signature {
        name: String::from_utf8_lossy(sig.name_bytes()).into_owned(),
        email: String::from_utf8_lossy(sig.email_bytes()).into_owned(),
        time: sig.when().seconds(),
    }
}

pub(crate) fn summarize(commit: &git2::Commit<'_>) -> CommitSummary {
    let message = String::from_utf8_lossy(commit.message_bytes());
    CommitSummary {
        id: commit.id(),
        summary: message.lines().next().unwrap_or_default().trim().to_owned(),
        author: signature(&commit.author()),
    }
}

/// `git log` limited to `path`. See [`GitRepo::log`].
fn git_log_path(git_dir: &Path, start: Oid, path: &str, skip: usize, limit: usize) -> Result<Vec<CommitSummary>> {
    let out = Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args(["-c", "log.showSignature=false", "log", "--no-color"])
        .arg("--format=%H%x1f%an%x1f%ae%x1f%at%x1f%s%x1e")
        .arg(format!("--skip={skip}"))
        .arg(format!("--max-count={limit}"))
        .arg(start.to_string())
        .arg("--")
        .arg(path)
        // Paths are literal, never pathspec magic like `:(glob)`.
        .env("GIT_LITERAL_PATHSPECS", "1")
        .output()?;
    if !out.status.success() {
        return Err(Error::Other(format!(
            "git log: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    out.stdout
        .split(|&b| b == 0x1e)
        .map(|r| r.trim_ascii())
        .filter(|r| !r.is_empty())
        .map(|r| {
            parse_log_header(r).ok_or_else(|| {
                Error::Other(format!("unexpected git log output: {:?}", String::from_utf8_lossy(r)))
            })
        })
        .collect()
}

/// Parses `%H%x1f%an%x1f%ae%x1f%at%x1f%s` from `git log`.
fn parse_log_header(header: &[u8]) -> Option<CommitSummary> {
    let text = String::from_utf8_lossy(header);
    let mut f = text.splitn(5, '\x1f');
    let (id, name, email, time, summary) = (f.next()?, f.next()?, f.next()?, f.next()?, f.next()?);
    Some(CommitSummary {
        id: Oid::from_str(id).ok()?,
        summary: summary.trim().to_owned(),
        author: Signature {
            name: name.to_owned(),
            email: email.to_owned(),
            time: time.parse().ok()?,
        },
    })
}

fn entry_kind(entry: &git2::TreeEntry<'_>) -> EntryKind {
    match entry.filemode() {
        0o040000 => EntryKind::Dir,
        0o100755 => EntryKind::Executable,
        0o120000 => EntryKind::Symlink,
        0o160000 => EntryKind::Submodule,
        _ => EntryKind::File,
    }
}

impl GitRepo {
    pub fn open(path: &Path) -> Result<Self> {
        Ok(Self {
            repo: Repository::open_bare(path)?,
        })
    }

    pub fn has_branches(&self) -> Result<bool> {
        Ok(self.repo.branches(Some(git2::BranchType::Local))?.next().is_some())
    }

    /// The branch HEAD points at, falling back to any existing branch when
    /// HEAD is unborn (e.g. the first push was not to `main`).
    pub fn default_branch(&self) -> Result<Option<String>> {
        if let Ok(head) = self.repo.find_reference("HEAD")
            && let Ok(Some(target)) = head.symbolic_target()
            && let Some(name) = target.strip_prefix("refs/heads/")
            && self.repo.find_reference(target).is_ok()
        {
            return Ok(Some(name.to_owned()));
        }
        Ok(self.branch_names()?.into_iter().next())
    }

    fn branch_names(&self) -> Result<Vec<String>> {
        let mut names = Vec::new();
        for branch in self.repo.branches(Some(git2::BranchType::Local))? {
            let (branch, _) = branch?;
            if let Some(name) = branch.name()? {
                names.push(name.to_owned());
            }
        }
        // Prefer conventional names when picking a fallback default.
        names.sort_by_key(|n| (n != "main" && n != "master", n.clone()));
        Ok(names)
    }

    /// Resolves a branch, tag or (abbreviated) commit hash to a commit.
    pub fn resolve_rev(&self, rev: &str) -> Option<Oid> {
        if rev.is_empty() || rev.starts_with('-') {
            return None;
        }
        for prefix in ["refs/heads/", "refs/tags/"] {
            if let Ok(r) = self.repo.find_reference(&format!("{prefix}{rev}"))
                && let Ok(commit) = r.peel_to_commit()
            {
                return Some(commit.id());
            }
        }
        // Only accept hex here: rev-parse syntax (`@{-1}`, `:/foo`, ...) is
        // not something URLs should be able to trigger.
        if (4..=40).contains(&rev.len()) && rev.bytes().all(|b| b.is_ascii_hexdigit()) {
            let obj = self.repo.revparse_single(rev).ok()?;
            return obj.peel_to_commit().ok().map(|c| c.id());
        }
        None
    }

    /// Splits `rest` (everything after `/tree/` in a URL) into the longest
    /// prefix that names a revision and the remaining path. This lets branch
    /// names contain slashes without any special URL syntax.
    pub fn resolve_rev_path(&self, rest: &str) -> Option<Resolved> {
        let rest = rest.trim_matches('/');
        let segments: Vec<&str> = rest.split('/').collect();
        (1..=segments.len()).rev().find_map(|i| {
            let rev = segments[..i].join("/");
            let commit = self.resolve_rev(&rev)?;
            Some(Resolved {
                rev,
                commit,
                path: segments[i..].join("/"),
            })
        })
    }

    pub fn commit_summary(&self, id: Oid) -> Result<CommitSummary> {
        Ok(summarize(&self.repo.find_commit(id)?))
    }

    pub fn commit_detail(&self, id: Oid) -> Result<CommitDetail> {
        let commit = self.repo.find_commit(id)?;
        let message = String::from_utf8_lossy(commit.message_bytes()).into_owned();
        let body = message
            .split_once('\n')
            .map(|(_, rest)| rest.trim().to_owned())
            .unwrap_or_default();
        Ok(CommitDetail {
            commit: summarize(&commit),
            body,
            committer: signature(&commit.committer()),
            parents: commit.parent_ids().collect(),
        })
    }

    fn subtree<'r>(&'r self, commit: Oid, path: &str) -> Result<Option<Tree<'r>>> {
        let root = self.repo.find_commit(commit)?.tree()?;
        if path.is_empty() {
            return Ok(Some(root));
        }
        let Ok(entry) = root.get_path(Path::new(path)) else {
            return Ok(None);
        };
        if entry.kind() != Some(ObjectType::Tree) {
            return Ok(None);
        }
        Ok(Some(self.repo.find_tree(entry.id())?))
    }

    pub fn is_dir(&self, commit: Oid, path: &str) -> Result<bool> {
        Ok(self.subtree(commit, path)?.is_some())
    }

    /// Lists a directory: directories first, then files, case-insensitively.
    /// Returns `None` if `path` is not a directory.
    pub fn tree(&self, commit: Oid, path: &str) -> Result<Option<Vec<TreeEntry>>> {
        let Some(tree) = self.subtree(commit, path)? else {
            return Ok(None);
        };
        let mut entries: Vec<TreeEntry> = tree
            .iter()
            .map(|e| TreeEntry {
                name: String::from_utf8_lossy(e.name_bytes()).into_owned(),
                kind: entry_kind(&e),
                id: e.id(),
            })
            .collect();
        entries.sort_by_cached_key(|e| (e.kind != EntryKind::Dir, e.name.to_lowercase()));
        Ok(Some(entries))
    }

    pub fn blob(&self, commit: Oid, path: &str) -> Result<Option<Blob>> {
        let tree = self.repo.find_commit(commit)?.tree()?;
        let Ok(entry) = tree.get_path(Path::new(path)) else {
            return Ok(None);
        };
        if entry.kind() != Some(ObjectType::Blob) {
            return Ok(None);
        }
        let blob = self.repo.find_blob(entry.id())?;
        Ok(Some(Blob {
            id: blob.id(),
            binary: blob.is_binary(),
            data: blob.content().to_vec(),
        }))
    }

    /// History starting at `start`, optionally limited to commits that
    /// changed `path` (with git's default history simplification).
    pub fn log(
        &self,
        start: Oid,
        path: &str,
        skip: usize,
        limit: usize,
    ) -> Result<Vec<CommitSummary>> {
        if !path.is_empty() {
            return self.log_path(start, path, skip, limit);
        }
        let mut walk = self.repo.revwalk()?;
        // Not `Sort::TIME`: any explicit sorting makes libgit2 walk the
        // entire history up front. The default order is lazy and, like
        // `git log`, never emits a parent before its children.
        walk.set_sorting(Sort::NONE)?;
        walk.push(start)?;
        walk.skip(skip)
            .take(limit)
            .map(|id| Ok(summarize(&self.repo.find_commit(id?)?)))
            .collect()
    }

    /// Path-limited history via `git log`, which uses the changed-path Bloom
    /// filters in the commit-graph to skip commits without diffing trees.
    /// On large repositories this is two orders of magnitude faster than
    /// walking with libgit2.
    fn log_path(&self, start: Oid, path: &str, skip: usize, limit: usize) -> Result<Vec<CommitSummary>> {
        git_log_path(self.repo.path(), start, path, skip, limit)
    }

    /// For each entry of directory `path` at `commit`, finds the most recent
    /// commit that changed it, like `git log -1 -- <path>/<entry>` would,
    /// but with one history walk for the whole directory.
    ///
    /// The walk is `git log --name-only`, streamed and stopped as soon as
    /// every entry is resolved. For subdirectories git skips unrelated
    /// commits using the commit-graph's changed-path Bloom filters. Merges
    /// count when they differ from every parent (`-c`). Each attribution is
    /// checked against the entry's current object id, so a change on a side
    /// branch that a merge discarded is not reported.
    ///
    /// That is exact for files. A directory, however, can differ from every
    /// parent of a merge even though each file inside it matches one of
    /// them; only git's per-path history simplification gets those right.
    /// Directories the walk doesn't settle are therefore looked up with
    /// `git log -1 -- <dir>` (cheap thanks to the Bloom filters), in
    /// parallel, once all files are resolved. So are entries attributed to
    /// a merge: when the same change reached history through several merges
    /// (e.g. into `master` and a maintenance branch), git prefers the
    /// first-parent line, which a date-ordered walk can't tell.
    ///
    /// At most `max_commits` commits are read; if that is not enough, the
    /// entries found so far are returned with `false`.
    pub fn last_commits(
        &self,
        commit: Oid,
        path: &str,
        max_commits: usize,
    ) -> Result<(HashMap<String, CommitSummary>, bool)> {
        let Some(tree) = self.subtree(commit, path)? else {
            return Ok((HashMap::new(), true));
        };
        let mut pending: HashMap<String, (Oid, bool)> = tree
            .iter()
            .map(|e| {
                let name = String::from_utf8_lossy(e.name_bytes()).into_owned();
                (name, (e.id(), e.kind() == Some(ObjectType::Tree)))
            })
            .collect();
        let mut found = HashMap::new();
        let files_pending = |p: &HashMap<String, (Oid, bool)>| p.values().any(|(_, dir)| !dir);

        let mut cmd = Command::new("git");
        cmd.arg("--git-dir")
            .arg(self.repo.path())
            .args(["-c", "log.showSignature=false", "log", "--no-color", "-z"])
            .args(["--name-only", "--no-renames", "-c"])
            .arg("--format=%x01%P%x1f%H%x1f%an%x1f%ae%x1f%at%x1f%s")
            .arg(commit.to_string())
            .env("GIT_LITERAL_PATHSPECS", "1")
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        if !path.is_empty() {
            cmd.arg("--").arg(path);
        }
        let mut child = cmd.spawn()?;
        let stdout = child.stdout.take().expect("piped");
        let prefix = if path.is_empty() { String::new() } else { format!("{path}/") };

        let mut reader = BufReader::new(stdout);
        let mut token = Vec::new();
        let mut current: Option<(CommitSummary, bool)> = None;
        let mut recheck = Vec::new();
        let mut read_commits = 0;
        let mut complete = true;
        loop {
            token.clear();
            if reader.read_until(0, &mut token)? == 0 {
                break;
            }
            if token.last() == Some(&0) {
                token.pop();
            }
            let t = token.strip_prefix(b"\n").unwrap_or(&token);
            if let Some(header) = t.strip_prefix(b"\x01") {
                if !files_pending(&pending) {
                    break;
                }
                if read_commits == max_commits {
                    complete = false;
                    break;
                }
                read_commits += 1;
                current = header
                    .iter()
                    .position(|&b| b == 0x1f)
                    .and_then(|i| {
                        let is_merge = header[..i].contains(&b' ');
                        Some((parse_log_header(&header[i + 1..])?, is_merge))
                    });
                continue;
            }
            let Some((c, is_merge)) = &current else { continue };
            let changed = String::from_utf8_lossy(t);
            let Some(name) = changed
                .strip_prefix(prefix.as_str())
                .and_then(|rest| rest.split('/').next())
            else {
                continue;
            };
            if let Some((target, _)) = pending.get(name)
                && self.entry_id(c.id, &prefix, name) == Some(*target)
            {
                pending.remove(name);
                if *is_merge {
                    recheck.push(name.to_owned());
                }
                found.insert(name.to_owned(), c.clone());
            }
        }
        // Stop git early if we have what we need.
        let _ = child.kill();
        let _ = child.wait();
        if !complete {
            return Ok((found, false));
        }

        let git_dir = self.repo.path();
        let mut rest: Vec<String> = pending.into_keys().collect();
        rest.extend(recheck);
        let lookups = std::thread::scope(|scope| {
            let workers: Vec<_> = rest
                .chunks(rest.len().div_ceil(8).max(1))
                .map(|names| {
                    let prefix = &prefix;
                    scope.spawn(move || {
                        names
                            .iter()
                            .map(|n| {
                                let latest = git_log_path(git_dir, commit, &format!("{prefix}{n}"), 0, 1)?;
                                Ok((n.clone(), latest.into_iter().next()))
                            })
                            .collect::<Result<Vec<_>>>()
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|w| w.join().unwrap_or_else(|_| Err(Error::Other("lookup thread panicked".into()))))
                .collect::<Result<Vec<_>>>()
        })?;
        for (name, latest) in lookups.into_iter().flatten() {
            if let Some(c) = latest {
                found.insert(name, c);
            }
        }
        Ok((found, true))
    }

    /// The object id of `<prefix><name>` at `commit`.
    fn entry_id(&self, commit: Oid, prefix: &str, name: &str) -> Option<Oid> {
        let tree = self.repo.find_commit(commit).ok()?.tree().ok()?;
        let entry = tree.get_path(Path::new(&format!("{prefix}{name}"))).ok()?;
        Some(entry.id())
    }

    pub fn branches(&self) -> Result<Vec<RefInfo>> {
        self.refs("refs/heads/*")
    }

    pub fn tags(&self) -> Result<Vec<RefInfo>> {
        self.refs("refs/tags/*")
    }

    /// References under `glob`, newest commit first.
    fn refs(&self, glob: &str) -> Result<Vec<RefInfo>> {
        let mut out = Vec::new();
        for r in self.repo.references_glob(glob)? {
            let r = r?;
            let Ok(commit) = r.peel_to_commit() else {
                continue; // e.g. a tag pointing at a tree
            };
            let name = String::from_utf8_lossy(r.shorthand_bytes()).into_owned();
            out.push(RefInfo {
                name,
                commit: summarize(&commit),
            });
        }
        out.sort_by_key(|r| std::cmp::Reverse(r.commit.author.time));
        Ok(out)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::process::Command;

    /// Creates a non-bare scratch repository and returns a helper to commit
    /// into it with deterministic timestamps.
    pub struct Scratch {
        pub dir: tempfile::TempDir,
        clock: std::cell::Cell<i64>,
    }

    impl Scratch {
        pub fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let s = Self {
                dir,
                clock: 1_700_000_000.into(),
            };
            s.git(&["init", "-q", "-b", "main"]);
            s
        }

        pub fn git(&self, args: &[&str]) -> String {
            let t = self.clock.get();
            let out = Command::new("git")
                .args(args)
                .current_dir(self.dir.path())
                .env("GIT_AUTHOR_NAME", "Ada")
                .env("GIT_AUTHOR_EMAIL", "ada@example.com")
                .env("GIT_COMMITTER_NAME", "Ada")
                .env("GIT_COMMITTER_EMAIL", "ada@example.com")
                .env("GIT_AUTHOR_DATE", format!("{t} +0000"))
                .env("GIT_COMMITTER_DATE", format!("{t} +0000"))
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8(out.stdout).unwrap().trim().to_owned()
        }

        pub fn write(&self, path: &str, content: &str) {
            let p = self.dir.path().join(path);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        }

        pub fn commit(&self, msg: &str) -> Oid {
            self.clock.set(self.clock.get() + 60);
            self.commit_same_second(msg)
        }

        /// Commits with the previous commit's timestamp, as happens with
        /// scripted or rebased commits.
        pub fn commit_same_second(&self, msg: &str) -> Oid {
            self.git(&["add", "-A"]);
            self.git(&["commit", "-q", "--allow-empty", "-m", msg]);
            Oid::from_str(&self.git(&["rev-parse", "HEAD"])).unwrap()
        }

        pub fn open(&self) -> GitRepo {
            GitRepo {
                repo: Repository::open(self.dir.path()).unwrap(),
            }
        }
    }

    #[test]
    fn resolves_branches_with_slashes() {
        let s = Scratch::new();
        s.write("src/lib.rs", "fn a() {}");
        let c1 = s.commit("first");
        s.git(&["branch", "feature/x"]);
        let repo = s.open();

        let r = repo.resolve_rev_path("feature/x/src/lib.rs").unwrap();
        assert_eq!(r.rev, "feature/x");
        assert_eq!(r.path, "src/lib.rs");
        assert_eq!(r.commit, c1);

        let r = repo.resolve_rev_path("main").unwrap();
        assert_eq!(r.path, "");

        let short = &c1.to_string()[..7];
        assert_eq!(repo.resolve_rev_path(&format!("{short}/src")).unwrap().commit, c1);
        assert!(repo.resolve_rev_path("nope/src").is_none());
        assert!(repo.resolve_rev("HEAD~0").is_none());
    }

    #[test]
    fn last_commits_per_entry() {
        let s = Scratch::new();
        s.write("a.txt", "1");
        s.write("dir/b.txt", "1");
        let c1 = s.commit("add a and b");
        s.write("a.txt", "2");
        let c2 = s.commit("change a");
        s.write("c.txt", "1");
        let c3 = s.commit("add c");

        let repo = s.open();
        let (found, complete) = repo.last_commits(c3, "", 100).unwrap();
        assert!(complete);
        assert_eq!(found["a.txt"].id, c2);
        assert_eq!(found["dir"].id, c1);
        assert_eq!(found["c.txt"].id, c3);

        let (found, _) = repo.last_commits(c3, "dir", 100).unwrap();
        assert_eq!(found["b.txt"].id, c1);
    }

    #[test]
    fn last_commits_ignores_discarded_side_of_merge() {
        let s = Scratch::new();
        s.write("f.txt", "base");
        s.commit("base");
        s.git(&["checkout", "-q", "-b", "side"]);
        s.write("f.txt", "side");
        s.commit("side change");
        s.git(&["checkout", "-q", "main"]);
        s.write("f.txt", "main");
        let main_change = s.commit("main change");
        // Newer side commit, but the merge keeps main's version of f.txt.
        s.git(&["checkout", "-q", "side"]);
        s.write("g.txt", "x");
        s.commit("side again");
        s.git(&["checkout", "-q", "main"]);
        s.git(&["merge", "-q", "-s", "ours", "side", "-m", "merge"]);
        let head = Oid::from_str(&s.git(&["rev-parse", "HEAD"])).unwrap();

        let (found, _) = s.open().last_commits(head, "", 100).unwrap();
        assert_eq!(found["f.txt"].id, main_change);
    }

    #[test]
    fn directory_changed_on_both_sides_of_a_merge() {
        let s = Scratch::new();
        s.write("dir/a", "1");
        s.write("dir/b", "1");
        s.commit("base");
        s.git(&["checkout", "-q", "-b", "side"]);
        s.write("dir/b", "side");
        s.commit("side changes b");
        s.git(&["checkout", "-q", "main"]);
        s.write("dir/a", "main");
        s.commit("main changes a");
        s.git(&["merge", "-q", "--no-edit", "side"]);
        let head = Oid::from_str(&s.git(&["rev-parse", "HEAD"])).unwrap();
        let expected = Oid::from_str(&s.git(&["log", "-1", "--format=%H", "--", "dir"])).unwrap();

        let (found, complete) = s.open().last_commits(head, "", 100).unwrap();
        assert!(complete);
        // Neither branch's commit alone produced the current `dir`; the merge did.
        assert_eq!(found["dir"].id, expected);
        assert_eq!(expected, head);
    }

    #[test]
    fn log_with_path_filter() {
        let s = Scratch::new();
        s.write("a.txt", "1");
        let c1 = s.commit("a");
        s.write("b.txt", "1");
        s.commit("b");
        s.write("a.txt", "2");
        let c3 = s.commit("a again");

        let repo = s.open();
        let all = repo.log(c3, "", 0, 10).unwrap();
        assert_eq!(all.len(), 3);
        let ids: Vec<Oid> = repo.log(c3, "a.txt", 0, 10).unwrap().iter().map(|c| c.id).collect();
        assert_eq!(ids, vec![c3, c1]);
        assert_eq!(repo.log(c3, "", 1, 1).unwrap()[0].summary, "b");
    }

    #[test]
    fn equal_timestamps_keep_children_before_parents() {
        let s = Scratch::new();
        s.write("a.txt", "1");
        let c1 = s.commit("one");
        s.write("a.txt", "2");
        let c2 = s.commit_same_second("two");
        s.write("b.txt", "1");
        let c3 = s.commit_same_second("three");

        let repo = s.open();
        let ids: Vec<Oid> = repo.log(c3, "", 0, 10).unwrap().iter().map(|c| c.id).collect();
        assert_eq!(ids, [c3, c2, c1]);
        assert_eq!(repo.log(c3, "a.txt", 0, 1).unwrap()[0].id, c2);
        assert_eq!(repo.last_commits(c3, "", 100).unwrap().0["a.txt"].id, c2);

        // A budget too small to reach the first commit leaves entries out.
        let (partial, complete) = repo.last_commits(c3, "", 1).unwrap();
        assert!(!complete);
        assert!(!partial.contains_key("a.txt"));
    }

    #[test]
    fn path_log_is_literal() {
        let s = Scratch::new();
        s.write("a b*.txt", "1");
        let c1 = s.commit("glob-looking name");
        s.write("a bc.txt", "1");
        let c2 = s.commit("would match the glob");
        let repo = s.open();
        let ids: Vec<Oid> = repo.log(c2, "a b*.txt", 0, 10).unwrap().iter().map(|c| c.id).collect();
        assert_eq!(ids, [c1]);
        assert!(repo.log(c2, "missing", 0, 10).unwrap().is_empty());
    }

    #[test]
    fn tree_sorting_and_default_branch() {
        let s = Scratch::new();
        s.write("Zeta.txt", "");
        s.write("alpha.txt", "");
        s.write("src/x.rs", "");
        let c = s.commit("files");
        let repo = s.open();
        let names: Vec<String> = repo.tree(c, "").unwrap().unwrap().into_iter().map(|e| e.name).collect();
        assert_eq!(names, ["src", "alpha.txt", "Zeta.txt"]);
        assert!(repo.tree(c, "alpha.txt").unwrap().is_none());
        assert_eq!(repo.default_branch().unwrap().as_deref(), Some("main"));
        assert_eq!(repo.blob(c, "src/x.rs").unwrap().unwrap().data, b"");
    }
}
