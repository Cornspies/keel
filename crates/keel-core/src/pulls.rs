//! Pull requests: issues that propose merging one branch into another.

use keel_git::{GitRepo, Oid};

use crate::issues::{insert_comment, insert_issue};
use crate::{Core, Error, Issue, Repo, Result, User, invalid, now};

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct PullRequest {
    pub issue_id: i64,
    pub base_ref: String,
    pub head_ref: String,
    /// Merge base: kept up to date while open, frozen when merged, so the
    /// diff of a merged pull request stays what was merged.
    pub base_commit: Option<String>,
    pub head_commit: Option<String>,
    pub merge_commit: Option<String>,
    pub merged_by: Option<String>,
    pub merged_at: Option<i64>,
}

/// Where a pull request's head commit is kept.
pub fn pull_ref(number: i64) -> String {
    format!("refs/pull/{number}/head")
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

    /// Opens a pull request to merge branch `head_ref` into `base_ref`.
    pub async fn create_pull(
        &self,
        repo: &Repo,
        author: &User,
        title: &str,
        body: &str,
        base_ref: &str,
        head_ref: &str,
    ) -> Result<Issue> {
        if base_ref == head_ref {
            return invalid("Choose two different branches.");
        }
        let (base_ref_c, head_ref_c) = (base_ref.to_owned(), head_ref.to_owned());
        let tips = self
            .git(repo.id, move |g| Ok((g.branch_tip(&base_ref_c), g.branch_tip(&head_ref_c))))
            .await?;
        let (Some(base), Some(head)) = tips else {
            return invalid("Both branches must exist.");
        };
        if self.git(repo.id, move |g| g.is_ancestor(head, base)).await? {
            return invalid(format!("{base_ref} already contains all commits of {head_ref}."));
        }
        let existing: Option<(i64,)> = sqlx::query_as(
            "SELECT i.number FROM issues i JOIN pull_requests p ON p.issue_id = i.id
             WHERE i.repo_id = ?1 AND i.state = 'open' AND p.base_ref = ?2 AND p.head_ref = ?3",
        )
        .bind(repo.id)
        .bind(base_ref)
        .bind(head_ref)
        .fetch_optional(&self.db)
        .await?;
        if let Some((n,)) = existing {
            return invalid(format!("Pull request #{n} for these branches is already open."));
        }

        let merge_base = self.git(repo.id, move |g| g.merge_base(base, head)).await?;
        let mut tx = self.db.begin().await?;
        let (issue_id, number) = insert_issue(&mut tx, repo.id, author, title, body, true).await?;
        sqlx::query("INSERT INTO pull_requests (issue_id, base_ref, head_ref, base_commit) VALUES (?1, ?2, ?3, ?4)")
            .bind(issue_id)
            .bind(base_ref)
            .bind(head_ref)
            .bind(merge_base.map(|c| c.to_string()))
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.git(repo.id, move |g| g.update_ref(&pull_ref(number), Some(head), None))
            .await?;
        self.issue(repo.id, number).await?.ok_or(Error::NotFound)
    }

    pub async fn pull(&self, issue_id: i64) -> Result<Option<PullRequest>> {
        Ok(sqlx::query_as(
            "SELECT p.issue_id, p.base_ref, p.head_ref, p.base_commit, p.head_commit, p.merge_commit,
                u.username AS merged_by, p.merged_at
             FROM pull_requests p LEFT JOIN users u ON u.id = p.merged_by
             WHERE p.issue_id = ?1",
        )
        .bind(issue_id)
        .fetch_optional(&self.db)
        .await?)
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

    /// After refs changed: points each open pull request's ref at its
    /// branch, and marks pull requests merged whose branch is now contained
    /// in the base branch (merged outside keel, e.g. with `git push`).
    pub async fn sync_pulls(&self, repo_id: i64, pusher: Option<i64>) -> Result<()> {
        let open: Vec<(i64, i64, String, String)> = sqlx::query_as(
            "SELECT i.id, i.number, p.base_ref, p.head_ref
             FROM issues i JOIN pull_requests p ON p.issue_id = i.id
             WHERE i.repo_id = ?1 AND i.state = 'open'",
        )
        .bind(repo_id)
        .fetch_all(&self.db)
        .await?;
        for (issue_id, number, base_ref, head_ref) in open {
            enum Sync {
                Unchanged,
                Open(Option<Oid>),
                Merged(Oid),
            }
            let sync = self
                .git(repo_id, move |g| {
                    let (Some(head), Some(base)) = (g.branch_tip(&head_ref), g.branch_tip(&base_ref)) else {
                        return Ok(Sync::Unchanged);
                    };
                    let pull_ref = pull_ref(number);
                    if g.ref_tip(&pull_ref) != Some(head) {
                        g.update_ref(&pull_ref, Some(head), None)?;
                    }
                    if g.is_ancestor(head, base)? {
                        return Ok(Sync::Merged(head));
                    }
                    Ok(Sync::Open(g.merge_base(base, head)?))
                })
                .await?;
            match sync {
                Sync::Unchanged => {}
                Sync::Open(merge_base) => {
                    sqlx::query("UPDATE pull_requests SET base_commit = ?2 WHERE issue_id = ?1")
                        .bind(issue_id)
                        .bind(merge_base.map(|c| c.to_string()))
                        .execute(&self.db)
                        .await?;
                }
                Sync::Merged(head) => {
                    if let Some(issue) = self.issue(repo_id, number).await?
                        && issue.id == issue_id
                    {
                        self.mark_merged(&issue, pusher, None, head, None).await?;
                    }
                }
            }
        }
        Ok(())
    }
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

        assert!(core.create_pull(&repo, &ada, "Same", "", "main", "main").await.is_err());
        assert!(core.create_pull(&repo, &ada, "Nope", "", "main", "nope").await.is_err());
        assert!(core.create_pull(&repo, &ada, "Behind", "", "feature", "main").await.is_err(), "nothing to merge");
        let issue = core.create_pull(&repo, &ada, "Add b", "", "main", "feature").await.unwrap();
        assert!(issue.is_pull);
        assert!(core.create_pull(&repo, &ada, "Again", "", "main", "feature").await.is_err(), "duplicate");
        let pull = core.pull(issue.id).await.unwrap().unwrap();
        assert_eq!((pull.base_ref.as_str(), pull.head_ref.as_str()), ("main", "feature"));
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
}
