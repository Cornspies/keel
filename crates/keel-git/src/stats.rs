//! Who committed when, for contributor and activity graphs.

use git2::Oid;

use crate::{Error, GitRepo, Result};

/// A commit's author and time.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommitStamp {
    /// Unix time the commit was authored.
    pub time: i64,
    pub name: String,
    pub email: String,
}

impl GitRepo {
    /// The commits reachable from `tip` but not from `since`, without
    /// merges, newest first. Names and emails go through `tip`'s
    /// `.mailmap`.
    ///
    /// Uses `git log`, which reads the commit-graph and is several times
    /// faster than walking with libgit2.
    pub fn commit_stamps(&self, tip: Oid, since: Option<Oid>) -> Result<Vec<CommitStamp>> {
        let mut cmd = crate::command();
        cmd.arg("--git-dir")
            .arg(self.repo.path())
            .arg("-c")
            .arg(format!("mailmap.blob={tip}:.mailmap"))
            .args(["log", "--no-merges", "--format=%at%x00%aN%x00%aE", &tip.to_string()]);
        if let Some(since) = since {
            cmd.arg(format!("^{since}"));
        }
        let out = cmd.arg("--").output()?;
        if !out.status.success() {
            return Err(Error::Other(format!("git log: {}", String::from_utf8_lossy(&out.stderr).trim())));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        Ok(text
            .lines()
            .filter_map(|line| {
                let mut parts = line.splitn(3, '\0');
                let time = parts.next()?.parse().ok()?;
                Some(CommitStamp { time, name: parts.next()?.to_owned(), email: parts.next()?.to_owned() })
            })
            .collect())
    }
}

impl GitRepo {
    /// Every file at `commit` with its size in bytes (submodules left
    /// out), via `git ls-tree`, which reads sizes from object headers
    /// without loading the files.
    pub fn file_sizes(&self, commit: Oid) -> Result<Vec<(String, u64)>> {
        let out = crate::command()
            .arg("--git-dir")
            .arg(self.repo.path())
            .args(["ls-tree", "-r", "-l", "-z", "--full-tree"])
            .arg(commit.to_string())
            .output()?;
        if !out.status.success() {
            return Err(Error::Other(format!("git ls-tree: {}", String::from_utf8_lossy(&out.stderr).trim())));
        }
        Ok(out
            .stdout
            .split(|&b| b == 0)
            .filter_map(|entry| {
                // `<mode> <type> <id> <size>\t<path>`, the size padded.
                let entry = std::str::from_utf8(entry).ok()?;
                let (meta, path) = entry.split_once('\t')?;
                let mut fields = meta.split_whitespace();
                let kind = fields.nth(1)?;
                let size = fields.nth(1)?.parse().ok()?;
                (kind == "blob").then(|| (path.to_owned(), size))
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use crate::read::tests::Scratch;

    #[test]
    fn file_sizes() {
        let s = Scratch::new();
        s.write("src/main.rs", "fn main() {}\n");
        s.write("a b.txt", "hello");
        let tip = s.commit("files");
        let mut sizes = s.open().file_sizes(tip).unwrap();
        sizes.sort();
        assert_eq!(sizes, [("a b.txt".to_owned(), 5), ("src/main.rs".to_owned(), 13)]);
    }

    #[test]
    fn stamps() {
        let s = Scratch::new();
        let first = s.commit("one");
        s.git(&["-c", "user.name=Bob", "-c", "user.email=bob@example.com", "commit", "-q", "--allow-empty", "--author=Bob <bob@example.com>", "-m", "two"]);
        s.write(".mailmap", "Ada Lovelace <ada@example.com>\n");
        let tip = s.commit("three");
        let repo = s.open();
        let all = repo.commit_stamps(tip, None).unwrap();
        assert_eq!(all.len(), 3);
        assert_eq!((all[1].name.as_str(), all[1].email.as_str()), ("Bob", "bob@example.com"));
        assert_eq!(all[2].time, 1_700_000_060);
        assert_eq!(all[0].name, "Ada Lovelace", "from .mailmap");
        let newer = repo.commit_stamps(tip, Some(first)).unwrap();
        assert_eq!(newer.len(), 2);
    }
}
