//! Commit counts for a repository's contributor and activity graphs:
//! commits per day on the default branch, overall and per author.
//!
//! Counted once per repository and kept in memory; after a push only the
//! new commits are counted (unless history was rewritten).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;

use keel_git::{CommitStamp, GitRepo, Oid};

use crate::{Core, Repo, Result};

/// Repositories whose counts are kept.
const CACHED: usize = 64;

pub const DAY: i64 = 86_400;

#[derive(Clone, Debug, Default)]
pub struct Contributor {
    /// The name on their latest commit.
    pub name: String,
    pub email: String,
    pub commits: i64,
    /// Commits per day (days since the Unix epoch, UTC).
    pub days: BTreeMap<i64, i64>,
    latest: i64,
}

#[derive(Clone, Debug)]
pub struct RepoActivity {
    pub branch: String,
    pub tip: Oid,
    /// Commits per day (days since the Unix epoch, UTC), without merges.
    pub days: BTreeMap<i64, i64>,
    /// By lowercased email.
    pub people: HashMap<String, Contributor>,
}

impl RepoActivity {
    fn new(branch: String, tip: Oid) -> Self {
        Self { branch, tip, days: BTreeMap::new(), people: HashMap::new() }
    }

    fn add(&mut self, stamps: &[CommitStamp]) {
        for s in stamps {
            let day = s.time.div_euclid(DAY);
            *self.days.entry(day).or_default() += 1;
            let p = self.people.entry(s.email.to_lowercase()).or_default();
            if p.commits == 0 || s.time >= p.latest {
                p.name.clone_from(&s.name);
                p.email.clone_from(&s.email);
                p.latest = s.time;
            }
            p.commits += 1;
            *p.days.entry(day).or_default() += 1;
        }
    }

    pub fn total(&self) -> i64 {
        self.days.values().sum()
    }

    /// The `n` people with the most commits.
    pub fn top(&self, n: usize) -> Vec<&Contributor> {
        let mut people: Vec<&Contributor> = self.people.values().collect();
        people.sort_by(|a, b| b.commits.cmp(&a.commits).then_with(|| a.name.cmp(&b.name)));
        people.truncate(n);
        people
    }
}

impl Core {
    /// Commit counts for `repo`'s default branch; `None` if it has no
    /// commits.
    pub async fn repo_activity(&self, repo: &Repo) -> Result<Option<Arc<RepoActivity>>> {
        enum Outcome {
            Empty,
            Unchanged,
            Counted(RepoActivity),
        }
        let cached = self.activity_cache.lock().expect("not poisoned").get(&repo.id).cloned();
        let (path, known) = (self.repo_path(repo), cached.clone());
        let outcome = tokio::task::spawn_blocking(move || -> keel_git::Result<Outcome> {
            let git = GitRepo::open(&path)?;
            let Some(branch) = git.default_branch()? else { return Ok(Outcome::Empty) };
            let Some(tip) = git.branch_tip(&branch) else { return Ok(Outcome::Empty) };
            if let Some(c) = known.filter(|c| c.branch == branch) {
                if c.tip == tip {
                    return Ok(Outcome::Unchanged);
                }
                if git.is_ancestor(c.tip, tip)? {
                    let mut next = RepoActivity::clone(&c);
                    next.add(&git.commit_stamps(tip, Some(c.tip))?);
                    next.tip = tip;
                    return Ok(Outcome::Counted(next));
                }
            }
            let mut fresh = RepoActivity::new(branch, tip);
            fresh.add(&git.commit_stamps(tip, None)?);
            Ok(Outcome::Counted(fresh))
        })
        .await
        .map_err(anyhow::Error::from)??;
        let mut cache = self.activity_cache.lock().expect("not poisoned");
        match outcome {
            Outcome::Empty => {
                cache.remove(&repo.id);
                Ok(None)
            }
            Outcome::Unchanged => Ok(cached),
            Outcome::Counted(fresh) => {
                let fresh = Arc::new(fresh);
                if cache.len() >= CACHED && !cache.contains_key(&repo.id) {
                    // Any one will do; counting again is cheap enough.
                    if let Some(&evict) = cache.keys().next() {
                        cache.remove(&evict);
                    }
                }
                cache.insert(repo.id, fresh.clone());
                Ok(Some(fresh))
            }
        }
    }

    /// The accounts commits by these (lowercased) emails belong to: their
    /// verified email (any email if this server can't send mail to verify
    /// them).
    pub async fn commit_authors(&self, emails: &[&str]) -> Result<HashMap<String, String>> {
        // `u.email` is `COLLATE NOCASE`, and compared as it is its unique
        // index finds each address. `lower(u.email) = j.value` reads every
        // account for every address instead: 67 ms with 20,000 accounts.
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT lower(u.email), u.username FROM json_each(?1) j JOIN users u ON u.email = j.value
             WHERE u.kind = 'user' AND u.disabled_at IS NULL AND (u.email_verified_at IS NOT NULL OR ?2)",
        )
        .bind(serde_json::json!(emails).to_string())
        .bind(!self.mail_enabled())
        .fetch_all(&self.db)
        .await?;
        Ok(rows.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[tokio::test]
    async fn counting() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let repo = core.create_repo(&ada, "engine", "", false).await.unwrap();
        assert!(core.repo_activity(&repo).await.unwrap().is_none(), "empty");

        let work = tempfile::tempdir().unwrap();
        let commit = |name: &str, email: &str, time: i64| {
            let out = Command::new("git")
                .args(["commit", "-q", "--allow-empty", "-m", "x"])
                .current_dir(work.path())
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_AUTHOR_NAME", name)
                .env("GIT_AUTHOR_EMAIL", email)
                .env("GIT_AUTHOR_DATE", format!("{time} +0000"))
                .env("GIT_COMMITTER_NAME", name)
                .env("GIT_COMMITTER_EMAIL", email)
                .output()
                .unwrap();
            assert!(out.status.success());
        };
        let push = |args: &[&str]| {
            let out = Command::new("git").args(args).current_dir(work.path()).output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        };
        push(&["init", "-q", "-b", "main"]);
        // Git reads small timestamps as dates; these are days in 2023.
        const D: i64 = 19_700;
        commit("Ada", "ada@example.com", (D + 10) * DAY);
        commit("Bob", "bob@example.com", (D + 10) * DAY + 5);
        commit("Ada L.", "ADA@example.com", (D + 12) * DAY);
        let remote = core.repo_path(&repo);
        push(&["push", "-q", remote.to_str().unwrap(), "main"]);

        let a = core.repo_activity(&repo).await.unwrap().unwrap();
        assert_eq!(a.total(), 3);
        assert_eq!(a.days.get(&(D + 10)), Some(&2));
        let top = a.top(10);
        assert_eq!((top[0].name.as_str(), top[0].commits), ("Ada L.", 2), "latest name, emails ignore case");
        assert_eq!(top[1].name, "Bob");

        // Only the new commits are counted after a push.
        commit("Bob", "bob@example.com", (D + 13) * DAY);
        push(&["push", "-q", remote.to_str().unwrap(), "main"]);
        let b = core.repo_activity(&repo).await.unwrap().unwrap();
        assert_eq!((b.total(), b.top(10)[1].commits), (4, 2));
        assert!(Arc::ptr_eq(&b, &core.repo_activity(&repo).await.unwrap().unwrap()), "unchanged: cached");

        // A rewritten branch is counted again.
        push(&["reset", "-q", "--hard", "HEAD~2"]);
        push(&["push", "-qf", remote.to_str().unwrap(), "main"]);
        assert_eq!(core.repo_activity(&repo).await.unwrap().unwrap().total(), 2);

        let authors = core.commit_authors(&["ada@example.com", "bob@example.com"]).await.unwrap();
        assert_eq!(authors.get("ada@example.com").map(String::as_str), Some("ada"), "no mail: any email counts");
        assert_eq!(authors.len(), 1);
        // Whatever the case the account's address is written in, and found
        // through the index on it.
        sqlx::query("UPDATE users SET email = 'Ada@Example.COM' WHERE id = ?1").bind(ada.id).execute(&core.db).await.unwrap();
        let authors = core.commit_authors(&["ada@example.com", "ada@example.com"]).await.unwrap();
        assert_eq!(authors.get("ada@example.com").map(String::as_str), Some("ada"));
        let plan: Vec<(i64, i64, i64, String)> = sqlx::query_as(
            "EXPLAIN QUERY PLAN SELECT lower(u.email), u.username FROM json_each('[]') j JOIN users u ON u.email = j.value",
        )
        .fetch_all(&core.db)
        .await
        .unwrap();
        assert!(plan.iter().any(|(.., detail)| detail.contains("SEARCH u USING INDEX")), "{plan:?}");
    }
}
