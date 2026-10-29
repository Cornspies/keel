//! Which commit last changed each line of a file.

use std::collections::HashMap;
use std::io::Read;
use std::process::Stdio;
use std::time::{Duration, Instant};

use git2::Oid;

use crate::{Error, GitRepo, Result};

/// Revisions listed in this file (in the blamed commit) are skipped, as on
/// GitHub: blame looks past them, e.g. past a commit that reformatted
/// everything.
pub const IGNORE_REVS_FILE: &str = ".git-blame-ignore-revs";
/// At most this many of them are used, to bound the command line.
const MAX_IGNORED_REVS: usize = 100;

/// Lines of the file that one commit last changed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlameHunk {
    pub commit: Oid,
    /// The first line (1-based) in the blamed version, and how many.
    pub start: usize,
    pub lines: usize,
    /// Where those lines start in `commit`'s version of the file.
    pub orig_start: usize,
    /// The file's path in `commit` (it may have been renamed since).
    pub orig_path: String,
    /// The commit that touched the file before `commit` did, with its path
    /// there, for blaming further back; `None` where `commit` added the
    /// lines with the file.
    pub previous: Option<(Oid, String)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BlameCommit {
    pub summary: String,
    /// Through the blamed commit's `.mailmap`.
    pub author: String,
    pub email: String,
    /// Unix time it was authored.
    pub time: i64,
}

/// A file's blame: hunks in line order, and the commits they refer to.
#[derive(Clone, Debug, Default)]
pub struct Blame {
    pub hunks: Vec<BlameHunk>,
    pub commits: HashMap<Oid, BlameCommit>,
}

impl GitRepo {
    /// Blames `path` as of `commit`, with `git blame`, which uses the
    /// commit-graph's changed-path Bloom filters to skip commits that didn't
    /// touch the file. `None` if it takes longer than `timeout` (git is
    /// stopped then).
    pub fn blame(&self, commit: Oid, path: &str, timeout: Duration) -> Result<Option<Blame>> {
        let mut cmd = crate::command();
        cmd.arg("--git-dir")
            .arg(self.repo.path())
            .args(["-c", "core.quotePath=false", "-c"])
            .arg(format!("mailmap.blob={commit}:.mailmap"))
            .args(["blame", "--incremental"]);
        for rev in self.ignored_revs(commit)? {
            cmd.arg("--ignore-rev").arg(rev.to_string());
        }
        cmd.arg(commit.to_string())
            .arg("--")
            .arg(path)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = cmd.spawn()?;
        let (Some(mut stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
            return Err(Error::Other("git blame: no output".into()));
        };
        // Read while waiting, so a full pipe doesn't stall git.
        let out = std::thread::spawn(move || {
            let mut buf = Vec::new();
            stdout.read_to_end(&mut buf).map(|_| buf)
        });
        let err = std::thread::spawn(move || {
            let mut buf = String::new();
            let _ = stderr.read_to_string(&mut buf);
            buf
        });
        let deadline = Instant::now() + timeout;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let out = out.join().map_err(|_| Error::Other("git blame: reading failed".into()))??;
        if !status.success() {
            let err = err.join().unwrap_or_default();
            return Err(Error::Other(format!("git blame: {}", err.trim())));
        }
        Ok(Some(parse_incremental(&String::from_utf8_lossy(&out))))
    }

    /// The commits [`IGNORE_REVS_FILE`] lists at `commit`: full object ids
    /// of commits in this repository (git refuses unknown ones).
    fn ignored_revs(&self, commit: Oid) -> Result<Vec<Oid>> {
        let Some(blob) = self.blob(commit, IGNORE_REVS_FILE)? else { return Ok(Vec::new()) };
        let text = String::from_utf8_lossy(&blob.data);
        Ok(text
            .lines()
            .map(|l| l.split('#').next().unwrap_or_default().trim())
            .filter(|l| l.len() == 40 || l.len() == 64)
            .filter_map(|l| Oid::from_str(l).ok())
            .filter(|id| self.repo.find_commit(*id).is_ok())
            .take(MAX_IGNORED_REVS)
            .collect())
    }
}

/// Parses `git blame --incremental`: for each hunk a line
/// `<commit> <orig line> <line> <count>`, the commit's details, and last
/// `filename <path>`. Adjacent hunks of one commit are joined.
fn parse_incremental(text: &str) -> Blame {
    let mut blame = Blame::default();
    let mut hunk: Option<BlameHunk> = None;
    let mut info = BlameCommit { summary: String::new(), author: String::new(), email: String::new(), time: 0 };
    for line in text.lines() {
        let (key, value) = line.split_once(' ').unwrap_or((line, ""));
        if hunk.is_none() {
            let mut nums = value.split(' ').map(|n| n.parse::<usize>().ok());
            if let (Ok(commit), Some(Some(orig_start)), Some(Some(start)), Some(Some(lines))) =
                (Oid::from_str(key), nums.next(), nums.next(), nums.next())
            {
                hunk = Some(BlameHunk { commit, start, lines, orig_start, orig_path: String::new(), previous: None });
                info = blame.commits.get(&commit).cloned().unwrap_or(BlameCommit {
                    summary: String::new(),
                    author: String::new(),
                    email: String::new(),
                    time: 0,
                });
            }
            continue;
        }
        let h = hunk.as_mut().expect("in a hunk");
        match key {
            "author" => info.author = value.to_owned(),
            "author-mail" => info.email = value.trim_start_matches('<').trim_end_matches('>').to_owned(),
            "author-time" => info.time = value.parse().unwrap_or(0),
            "summary" => info.summary = value.to_owned(),
            "previous" => {
                h.previous = value
                    .split_once(' ')
                    .and_then(|(id, path)| Some((Oid::from_str(id).ok()?, unquote(path))));
            }
            "filename" => {
                let mut done = hunk.take().expect("in a hunk");
                done.orig_path = unquote(value);
                blame.commits.insert(done.commit, info.clone());
                blame.hunks.push(done);
            }
            _ => {}
        }
    }
    blame.hunks.sort_by_key(|h| h.start);
    let mut joined: Vec<BlameHunk> = Vec::with_capacity(blame.hunks.len());
    for h in blame.hunks {
        match joined.last_mut() {
            Some(last)
                if last.commit == h.commit
                    && last.orig_path == h.orig_path
                    && last.start + last.lines == h.start
                    && last.orig_start + last.lines == h.orig_start =>
            {
                last.lines += h.lines;
            }
            _ => joined.push(h),
        }
    }
    blame.hunks = joined;
    blame
}

/// Undoes git's quoting of unusual paths (`"a\tb"`, octal for other bytes).
fn unquote(path: &str) -> String {
    let Some(inner) = path.strip_prefix('"').and_then(|p| p.strip_suffix('"')) else { return path.to_owned() };
    let mut out = Vec::with_capacity(inner.len());
    let mut bytes = inner.bytes().peekable();
    while let Some(b) = bytes.next() {
        if b != b'\\' {
            out.push(b);
            continue;
        }
        match bytes.next() {
            Some(b'n') => out.push(b'\n'),
            Some(b't') => out.push(b'\t'),
            Some(b'r') => out.push(b'\r'),
            Some(b'a') => out.push(7),
            Some(b'b') => out.push(8),
            Some(b'f') => out.push(12),
            Some(b'v') => out.push(11),
            Some(d @ b'0'..=b'7') => {
                let mut n = u32::from(d - b'0');
                for _ in 0..2 {
                    match bytes.peek() {
                        Some(&d @ b'0'..=b'7') => {
                            n = n * 8 + u32::from(d - b'0');
                            bytes.next();
                        }
                        _ => break,
                    }
                }
                out.push(n as u8);
            }
            Some(other) => out.push(other),
            None => out.push(b'\\'),
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read::tests::Scratch;

    const LONG: Duration = Duration::from_secs(30);

    #[test]
    fn blames_lines() {
        let s = Scratch::new();
        s.write("src/lib.rs", "one\ntwo\nthree\n");
        let first = s.commit("Add lib");
        s.write("src/lib.rs", "one\nTWO\nthree\nfour\n");
        s.git(&["-c", "user.name=Bob", "-c", "user.email=bob@example.com", "commit", "-qam", "Shout", "--author=Bob <bob@example.com>"]);
        let second = Oid::from_str(&s.git(&["rev-parse", "HEAD"])).unwrap();
        // Renames are followed.
        s.git(&["mv", "src/lib.rs", "lib.rs"]);
        let tip = s.commit("Move");
        let repo = s.open();

        let b = repo.blame(tip, "lib.rs", LONG).unwrap().unwrap();
        let spans: Vec<(Oid, usize, usize)> = b.hunks.iter().map(|h| (h.commit, h.start, h.lines)).collect();
        assert_eq!(spans, [(first, 1, 1), (second, 2, 1), (first, 3, 1), (second, 4, 1)]);
        assert_eq!(b.hunks[0].orig_path, "src/lib.rs");
        assert_eq!(b.hunks[0].previous, None, "added with the file");
        assert_eq!(b.hunks[1].previous, Some((first, "src/lib.rs".to_owned())));
        let bob = &b.commits[&second];
        assert_eq!((bob.summary.as_str(), bob.author.as_str(), bob.email.as_str()), ("Shout", "Bob", "bob@example.com"));
        assert_eq!(b.commits[&first].time, 1_700_000_060);

        // Commits in .git-blame-ignore-revs are looked past.
        s.write(IGNORE_REVS_FILE, &format!("# formatting\n{second}\n{}\nnot a commit\n", "0".repeat(40)));
        let ignoring = s.commit("Ignore the shouting");
        let b = repo.blame(ignoring, "lib.rs", LONG).unwrap().unwrap();
        let spans: Vec<(Oid, usize, usize)> = b.hunks.iter().map(|h| (h.commit, h.start, h.lines)).collect();
        // Line 4 is new in the ignored commit, with nothing earlier to blame.
        assert_eq!(spans, [(first, 1, 3), (second, 4, 1)]);

        assert!(repo.blame(tip, "missing.rs", LONG).is_err());
        assert!(repo.blame(tip, "lib.rs", Duration::ZERO).unwrap().is_none(), "out of time");
    }

    #[test]
    fn joins_adjacent_hunks() {
        let c = "1".repeat(40);
        let text = format!(
            "{c} 1 1 2\nauthor Ada\nauthor-mail <ada@example.com>\nauthor-time 5\nsummary s\nfilename a\n\
             {c} 3 3 1\nauthor Ada\nauthor-mail <ada@example.com>\nauthor-time 5\nsummary s\nfilename a\n"
        );
        let b = parse_incremental(&text);
        assert_eq!(b.hunks.len(), 1);
        assert_eq!((b.hunks[0].start, b.hunks[0].lines), (1, 3));
    }

    #[test]
    fn unquotes_paths() {
        assert_eq!(unquote("plain/path.rs"), "plain/path.rs");
        assert_eq!(unquote(r#""tab\there.rs""#), "tab\there.rs");
        assert_eq!(unquote(r#""caf\303\251.rs""#), "café.rs");
        assert_eq!(unquote(r#""quote\"d""#), "quote\"d");
    }
}
