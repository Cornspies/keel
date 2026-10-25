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

#[cfg(test)]
mod tests {
    use crate::read::tests::Scratch;

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
