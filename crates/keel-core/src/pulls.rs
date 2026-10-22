//! Pull requests: issues that propose merging one branch into another.

use keel_git::{GitRepo, Oid};

use crate::issues::{insert_comment, insert_issue};
use crate::{Access, Core, Error, Issue, Repo, Result, User, invalid, now};

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct PullRequest {
    pub issue_id: i64,
    pub base_ref: String,
    pub head_ref: String,
    /// The repository `head_ref` is in: the base repository or a fork of
    /// it (or its parent). `None` if that repository was deleted.
    pub head_repo_id: Option<i64>,
    /// `owner/name` of the head repository.
    pub head_repo: Option<String>,
    /// Merge base: kept up to date while open, frozen when merged, so the
    /// diff of a merged pull request stays what was merged.
    pub base_commit: Option<String>,
    pub head_commit: Option<String>,
    pub merge_commit: Option<String>,
    pub merged_by: Option<String>,
    pub merged_at: Option<i64>,
}

impl PullRequest {
    /// Whether the head branch is in another repository than `issue`'s.
    pub fn is_cross_repo(&self, issue: &Issue) -> bool {
        self.head_repo_id != Some(issue.repo_id)
    }

    /// How to refer to the head branch from the base repository: `topic`,
    /// or `owner:topic` for a branch in a fork.
    pub fn head_label(&self, issue: &Issue) -> String {
        if !self.is_cross_repo(issue) {
            return self.head_ref.clone();
        }
        let owner = self.head_repo.as_deref().and_then(|r| r.split('/').next()).unwrap_or("unknown");
        format!("{owner}:{}", self.head_ref)
    }
}

/// Where a pull request's head commit is kept.
pub fn pull_ref(number: i64) -> String {
    format!("refs/pull/{number}/head")
}

/// Whether pull requests from `head` into `base` are allowed: within one
/// repository, or between a repository and a fork of it.
pub fn related(base: &Repo, head: &Repo) -> bool {
    base.id == head.id || head.forked_from == Some(base.id) || base.forked_from == Some(head.id)
}

impl Core {
    async fn git<T: Send + 'static>(
        &self,
        repo_id: i64,
        f: impl FnOnce(&GitRepo) -> keel_git::Result<T> + Send + 'static,
    ) -> Result<T> {
        let path = self.config.repo_path(repo_id);
        let result = tokio::task::spawn_blocking(move || f(&GitRepo::open(&path)?))
            .await
            .map_err(anyhow::Error::from)?;
        Ok(result?)
    }

    /// Opens a pull request to merge branch `head_ref` of `head_repo` (the
    /// same repository or a fork) into `base_ref` of `repo`.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_pull(
        &self,
        repo: &Repo,
        author: &User,
        title: &str,
        body: &str,
        base_ref: &str,
        head_repo: &Repo,
        head_ref: &str,
    ) -> Result<Issue> {
        if !related(repo, head_repo) || self.access(head_repo, Some(author)).await? == Access::None {
            return Err(Error::NotFound);
        }
        let cross = repo.id != head_repo.id;
        if !cross && base_ref == head_ref {
            return invalid("Choose two different branches.");
        }
        let b = base_ref.to_owned();
        let base = self.git(repo.id, move |g| Ok(g.branch_tip(&b))).await?;
        let h = head_ref.to_owned();
        let head = self.git(head_repo.id, move |g| Ok(g.branch_tip(&h))).await?;
        let (Some(base), Some(head)) = (base, head) else {
            return invalid("Both branches must exist.");
        };
        if let Some(n) = self.open_pull_between(repo.id, base_ref, head_repo.id, head_ref).await? {
            return invalid(format!("Pull request #{n} for these branches is already open."));
        }

        // A fork's commits are fetched into the base repository, under a
        // temporary ref until the pull request has a number.
        let incoming = format!("refs/pull/incoming-{}/head", uuid_like());
        let head = if cross {
            let (from, h, into) = (self.config.repo_path(head_repo.id), head_ref.to_owned(), incoming.clone());
            self.git(repo.id, move |g| g.fetch_branch(&from, &h, &into)).await?
        } else {
            head
        };
        let result = self
            .insert_pull(repo, author, title, body, base_ref, head_repo, head_ref, base, head)
            .await;
        if cross {
            self.git(repo.id, move |g| g.update_ref(&incoming, None, None)).await?;
        }
        result
    }

    #[allow(clippy::too_many_arguments)]
    async fn insert_pull(
        &self,
        repo: &Repo,
        author: &User,
        title: &str,
        body: &str,
        base_ref: &str,
        head_repo: &Repo,
        head_ref: &str,
        base: Oid,
        head: Oid,
    ) -> Result<Issue> {
        if self.git(repo.id, move |g| g.is_ancestor(head, base)).await? {
            return invalid(format!("{base_ref} already contains all commits of {head_ref}."));
        }
        let merge_base = self.git(repo.id, move |g| g.merge_base(base, head)).await?;
        let mut tx = self.db.begin().await?;
        let (issue_id, number) = insert_issue(&mut tx, repo.id, author, title, body, true).await?;
        sqlx::query(
            "INSERT INTO pull_requests (issue_id, base_ref, head_ref, head_repo_id, base_commit)
             VALUES (?1, ?2, ?3, ?4, ?5)",
        )
        .bind(issue_id)
        .bind(base_ref)
        .bind(head_ref)
        .bind(head_repo.id)
        .bind(merge_base.map(|c| c.to_string()))
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        self.git(repo.id, move |g| g.update_ref(&pull_ref(number), Some(head), None))
            .await?;
        self.issue(repo.id, number).await?.ok_or(Error::NotFound)
    }

    /// The number of the open pull request for these branches, if any.
    pub async fn open_pull_between(
        &self,
        repo_id: i64,
        base_ref: &str,
        head_repo_id: i64,
        head_ref: &str,
    ) -> Result<Option<i64>> {
        let row: Option<(i64,)> = sqlx::query_as(
            "SELECT i.number FROM issues i JOIN pull_requests p ON p.issue_id = i.id
             WHERE i.repo_id = ?1 AND i.state = 'open' AND p.base_ref = ?2 AND p.head_repo_id = ?3
               AND p.head_ref = ?4",
        )
        .bind(repo_id)
        .bind(base_ref)
        .bind(head_repo_id)
        .bind(head_ref)
        .fetch_optional(&self.db)
        .await?;
        Ok(row.map(|(n,)| n))
    }

    pub async fn pull(&self, issue_id: i64) -> Result<Option<PullRequest>> {
        Ok(sqlx::query_as(
            "SELECT p.issue_id, p.base_ref, p.head_ref, p.head_repo_id,
                hu.username || '/' || hr.name AS head_repo,
                p.base_commit, p.head_commit, p.merge_commit, u.username AS merged_by, p.merged_at
             FROM pull_requests p
             LEFT JOIN users u ON u.id = p.merged_by
             LEFT JOIN repositories hr ON hr.id = p.head_repo_id
             LEFT JOIN users hu ON hu.id = hr.owner_id
             WHERE p.issue_id = ?1",
        )
        .bind(issue_id)
        .fetch_optional(&self.db)
        .await?)
    }

    /// The current tips of an open pull request's base and head branches
    /// (`None` if deleted), as commits in the base repository: points
    /// `refs/pull/N/head` at the head branch, fetching it from a fork if it
    /// moved.
    pub async fn pull_tips(&self, issue: &Issue, pull: &PullRequest) -> Result<(Option<Oid>, Option<Oid>)> {
        let (base_ref, head_ref, number) = (pull.base_ref.clone(), pull.head_ref.clone(), issue.number);
        let fork = match pull.head_repo_id {
            Some(id) if id != issue.repo_id => Some(self.config.repo_path(id)),
            Some(_) => None,
            None => {
                let base = self.git(issue.repo_id, move |g| Ok(g.branch_tip(&base_ref))).await?;
                return Ok((base, None));
            }
        };
        let fork_tip = match &fork {
            Some(path) => {
                let (path, h) = (path.clone(), head_ref.clone());
                let read = tokio::task::spawn_blocking(move || Ok::<_, keel_git::Error>(GitRepo::open(&path)?.branch_tip(&h)))
                    .await
                    .map_err(anyhow::Error::from)?;
                // A fork whose directory is gone counts as a deleted branch.
                read.unwrap_or(None)
            }
            None => None,
        };
        self.git(issue.repo_id, move |g| {
            let base = g.branch_tip(&base_ref);
            let pinned = pull_ref(number);
            let head = match fork {
                None => g.branch_tip(&head_ref),
                Some(path) => match fork_tip {
                    Some(tip) if g.ref_tip(&pinned) == Some(tip) => Some(tip),
                    Some(_) => Some(g.fetch_branch(&path, &head_ref, &pinned)?),
                    None => None,
                },
            };
            if let Some(head) = head
                && g.ref_tip(&pinned) != Some(head)
            {
                g.update_ref(&pinned, Some(head), None)?;
            }
            Ok((base, head))
        })
        .await
    }

    /// Records a merge: by `actor` through the web UI (with the merge base
    /// and merge commit), or detected after a push (both unknown; the last
    /// recorded merge base is kept).
    pub async fn mark_merged(
        &self,
        issue: &Issue,
        actor: Option<i64>,
        base_commit: Option<Oid>,
        head_commit: Oid,
        merge_commit: Option<Oid>,
    ) -> Result<()> {
        let now = now();
        let mut tx = self.db.begin().await?;
        let done = sqlx::query("UPDATE issues SET state = 'merged', closed_at = ?2 WHERE id = ?1 AND state = 'open'")
            .bind(issue.id)
            .bind(now)
            .execute(&mut *tx)
            .await?;
        if done.rows_affected() == 0 {
            return invalid("This pull request is not open.");
        }
        sqlx::query(
            "UPDATE pull_requests SET base_commit = COALESCE(?2, base_commit), head_commit = ?3,
                merge_commit = ?4, merged_by = ?5, merged_at = ?6 WHERE issue_id = ?1",
        )
        .bind(issue.id)
        .bind(base_commit.map(|c| c.to_string()))
        .bind(head_commit.to_string())
        .bind(merge_commit.map(|c| c.to_string()))
        .bind(actor)
        .bind(now)
        .execute(&mut *tx)
        .await?;
        insert_comment(&mut tx, issue.id, actor, "merged", "").await?;
        tx.commit().await?;
        Ok(())
    }

    /// After refs of `repo_id` changed: updates the open pull requests into
    /// or out of it (see [`pull_tips`](Self::pull_tips)), and marks those
    /// into it merged whose head is now contained in the base branch
    /// (merged outside keel, e.g. with `git push`).
    pub async fn sync_pulls(&self, repo_id: i64, pusher: Option<i64>) -> Result<()> {
        let open: Vec<(i64, i64)> = sqlx::query_as(
            "SELECT i.repo_id, i.number FROM issues i JOIN pull_requests p ON p.issue_id = i.id
             WHERE i.state = 'open' AND (i.repo_id = ?1 OR p.head_repo_id = ?1)",
        )
        .bind(repo_id)
        .fetch_all(&self.db)
        .await?;
        for (base_repo, number) in open {
            let Some(issue) = self.issue(base_repo, number).await? else { continue };
            let Some(pull) = self.pull(issue.id).await? else { continue };
            let (Some(base), Some(head)) = self.pull_tips(&issue, &pull).await? else {
                continue;
            };
            // Only a push to the base repository merges; a fork resetting
            // its branch to something already merged doesn't.
            if base_repo == repo_id && self.git(base_repo, move |g| g.is_ancestor(head, base)).await? {
                self.mark_merged(&issue, pusher, None, head, None).await?;
                continue;
            }
            let merge_base = self.git(base_repo, move |g| g.merge_base(base, head)).await?;
            sqlx::query("UPDATE pull_requests SET base_commit = ?2 WHERE issue_id = ?1")
                .bind(issue.id)
                .bind(merge_base.map(|c| c.to_string()))
                .execute(&self.db)
                .await?;
        }
        Ok(())
    }
}

/// A unique-enough name for a temporary ref.
fn uuid_like() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("{nanos:x}-{}", NEXT.fetch_add(1, Ordering::Relaxed))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// Pushes branches `main` and `feature` (one extra commit) to `repo`.
    fn push_branches(core: &Core, repo: &Repo) -> tempfile::TempDir {
        let work = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(work.path())
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_AUTHOR_NAME", "Ada")
                .env("GIT_AUTHOR_EMAIL", "ada@example.com")
                .env("GIT_COMMITTER_NAME", "Ada")
                .env("GIT_COMMITTER_EMAIL", "ada@example.com")
                .output()
                .unwrap();
            assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        let remote = core.repo_path(repo);
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(work.path().join("a"), "1").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "base"]);
        git(&["checkout", "-qb", "feature"]);
        std::fs::write(work.path().join("b"), "1").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "feature"]);
        git(&["push", "-q", remote.to_str().unwrap(), "main", "feature"]);
        work
    }

    #[tokio::test]
    async fn pull_lifecycle() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let repo = core.create_repo(&ada, "engine", "", false).await.unwrap();
        let _work = push_branches(&core, &repo);

        assert!(core.create_pull(&repo, &ada, "Same", "", "main", &repo, "main").await.is_err());
        assert!(core.create_pull(&repo, &ada, "Nope", "", "main", &repo, "nope").await.is_err());
        assert!(core.create_pull(&repo, &ada, "Behind", "", "feature", &repo, "main").await.is_err(), "nothing to merge");
        let issue = core.create_pull(&repo, &ada, "Add b", "", "main", &repo, "feature").await.unwrap();
        assert!(issue.is_pull);
        assert!(core.create_pull(&repo, &ada, "Again", "", "main", &repo, "feature").await.is_err(), "duplicate");
        let pull = core.pull(issue.id).await.unwrap().unwrap();
        assert_eq!((pull.base_ref.as_str(), pull.head_ref.as_str()), ("main", "feature"));
        assert_eq!(pull.head_label(&issue), "feature");
        let head = core.git(repo.id, |g| Ok(g.ref_tip("refs/pull/1/head"))).await.unwrap();
        assert!(head.is_some());

        // Merging outside keel (fast-forward main to feature) is detected.
        core.git(repo.id, move |g| g.update_ref("refs/heads/main", head, None)).await.unwrap();
        core.sync_pulls(repo.id, Some(ada.id)).await.unwrap();
        let issue = core.issue(repo.id, 1).await.unwrap().unwrap();
        assert_eq!(issue.state, "merged");
        let pull = core.pull(issue.id).await.unwrap().unwrap();
        assert_eq!(pull.merged_by.as_deref(), Some("ada"));
        assert_eq!(pull.head_commit, head.map(|h| h.to_string()));
        assert_ne!(pull.base_commit, pull.head_commit, "merge base from before the merge");
        assert!(core.set_issue_open(&issue, &ada, true).await.is_ok());
        assert_eq!(core.issue(repo.id, 1).await.unwrap().unwrap().state, "merged", "can't reopen");
    }

    #[tokio::test]
    async fn pull_from_fork() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let bob = core.register("bob", "bob@example.com", "password1").await.unwrap();
        let eve = core.register("eve", "eve@example.com", "password1").await.unwrap();
        let repo = core.create_repo(&ada, "engine", "", false).await.unwrap();
        let unrelated = core.create_repo(&eve, "engine", "", false).await.unwrap();
        let work = push_branches(&core, &repo);
        let fork = core.fork_repo(&repo, &bob, "engine").await.unwrap();

        // Bob adds a commit on a new branch in his fork.
        let git = |args: &[&str]| {
            let out = Command::new("git").args(args).current_dir(work.path()).output().unwrap();
            assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        git(&["checkout", "-qb", "topic", "main"]);
        std::fs::write(work.path().join("c"), "1").unwrap();
        git(&["add", "-A"]);
        git(&["-c", "user.name=Bob", "-c", "user.email=bob@example.com", "commit", "-qm", "topic"]);
        git(&["push", "-q", core.repo_path(&fork).to_str().unwrap(), "topic"]);
        let topic = core.git(fork.id, |g| Ok(g.branch_tip("topic"))).await.unwrap().unwrap();

        assert!(core.create_pull(&repo, &bob, "x", "", "main", &unrelated, "main").await.is_err(), "not a fork");
        let issue = core.create_pull(&repo, &bob, "Add c", "", "main", &fork, "topic").await.unwrap();
        let pull = core.pull(issue.id).await.unwrap().unwrap();
        assert!(pull.is_cross_repo(&issue));
        assert_eq!(pull.head_label(&issue), "bob:topic");
        assert_eq!(pull.head_repo.as_deref(), Some("bob/engine"));
        assert_eq!(core.git(repo.id, |g| Ok(g.ref_tip("refs/pull/1/head"))).await.unwrap(), Some(topic));
        let refs = Command::new("git").arg("-C").arg(core.repo_path(&repo)).arg("for-each-ref").output().unwrap();
        let refs = String::from_utf8_lossy(&refs.stdout);
        assert!(!refs.contains("incoming"), "temporary ref removed: {refs}");
        assert_eq!(core.git(repo.id, |g| Ok(g.branch_tip("topic"))).await.unwrap(), None);
        assert!(core.create_pull(&repo, &bob, "Again", "", "main", &fork, "topic").await.is_err(), "duplicate");

        // A push to the fork moves the pull request.
        std::fs::write(work.path().join("c"), "2").unwrap();
        git(&["-c", "user.name=Bob", "-c", "user.email=bob@example.com", "commit", "-qam", "more"]);
        git(&["push", "-q", core.repo_path(&fork).to_str().unwrap(), "topic"]);
        let topic2 = core.git(fork.id, |g| Ok(g.branch_tip("topic"))).await.unwrap().unwrap();
        core.sync_pulls(fork.id, Some(bob.id)).await.unwrap();
        assert_eq!(core.git(repo.id, |g| Ok(g.ref_tip("refs/pull/1/head"))).await.unwrap(), Some(topic2));
        assert_eq!(core.pull_tips(&issue, &pull).await.unwrap().1, Some(topic2));

        // Pushing those commits to the base marks it merged.
        git(&["push", "-q", core.repo_path(&repo).to_str().unwrap(), "topic:main"]);
        core.sync_pulls(repo.id, Some(ada.id)).await.unwrap();
        assert_eq!(core.issue(repo.id, 1).await.unwrap().unwrap().state, "merged");

        // Deleting the fork keeps the pull request.
        core.delete_repo(&fork).await.unwrap();
        let pull = core.pull(issue.id).await.unwrap().unwrap();
        assert_eq!((pull.head_repo_id, pull.head_label(&issue).as_str()), (None, "unknown:topic"));
    }
}
