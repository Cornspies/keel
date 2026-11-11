//! Pull requests: issues that propose merging one branch into another.

use keel_git::{GitRepo, Identity, MergeStyle, Oid};

use crate::activity::{Activity, IssueAction};
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
    /// Drafts can't be merged until they're marked ready for review.
    pub is_draft: bool,
    /// Who turned auto-merge on, if it is: it merges as them, the way
    /// below, once nothing blocks it.
    pub auto_merge_by: Option<String>,
    /// `merge` or `squash`.
    pub auto_merge_style: String,
    /// Delete the head branch after auto-merging.
    pub auto_merge_delete: bool,
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

/// Whether a pull request from `head` into `base` would take a private
/// repository's commits into a public one: those are refused.
pub fn private_into_public(base: &Repo, head: &Repo) -> bool {
    base.id != head.id && head.is_private && !base.is_private
}

impl Core {
    pub(crate) async fn git<T: Send + 'static>(
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
        draft: bool,
    ) -> Result<Issue> {
        if !related(repo, head_repo) || self.access(head_repo, Some(author)).await? == Access::None {
            return Err(Error::NotFound);
        }
        if self.is_mirror(repo.id).await? {
            return invalid("This repository is a mirror; open pull requests where it's mirrored from.");
        }
        let cross = repo.id != head_repo.id;
        // Opening it copies the head's commits into the base, where
        // everyone who reads the base can fetch them (see `may_fetch_head`
        // for afterwards).
        if private_into_public(repo, head_repo) {
            return invalid(format!(
                "{}/{} is private and {}/{} isn't: a pull request would make its commits public. \
                 Make {}/{} public first, if they may be.",
                head_repo.owner, head_repo.name, repo.owner, repo.name, head_repo.owner, head_repo.name
            ));
        }
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
            .insert_pull(repo, author, title, body, base_ref, head_repo, head_ref, base, head, draft)
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
        draft: bool,
    ) -> Result<Issue> {
        if self.git(repo.id, move |g| g.is_ancestor(head, base)).await? {
            return invalid(format!("{base_ref} already contains all commits of {head_ref}."));
        }
        let merge_base = self.git(repo.id, move |g| g.merge_base(base, head)).await?;
        let mut tx = self.write_tx().await?;
        let (issue_id, number) = insert_issue(&mut tx, repo.id, author, title, body, true).await?;
        sqlx::query(
            "INSERT INTO pull_requests (issue_id, base_ref, head_ref, head_repo_id, base_commit, is_draft)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(issue_id)
        .bind(base_ref)
        .bind(head_ref)
        .bind(head_repo.id)
        .bind(merge_base.map(|c| c.to_string()))
        .bind(draft)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        self.git(repo.id, move |g| g.update_ref(&pull_ref(number), Some(head), None))
            .await?;
        let issue = self.issue(repo.id, number).await?.ok_or(Error::NotFound)?;
        self.record(Activity::Issue { issue_id: issue.id, actor: Some(author.id), action: IssueAction::Opened })
            .await;
        Ok(issue)
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
                p.base_commit, p.head_commit, p.merge_commit, u.username AS merged_by, p.merged_at,
                p.is_draft, au.username AS auto_merge_by, p.auto_merge_style, p.auto_merge_delete
             FROM pull_requests p
             LEFT JOIN users u ON u.id = p.merged_by
             LEFT JOIN users au ON au.id = p.auto_merge_by
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
            Some(_) if !self.may_fetch_head(issue, pull).await? => None,
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

    /// Whether commits may still be copied from a pull request's head
    /// repository into its base repository. Access was checked when the
    /// pull request was opened, but may be gone since: a private head's
    /// commits only go into a private base, and only while the author can
    /// still read the head. Otherwise the head counts as deleted.
    async fn may_fetch_head(&self, issue: &Issue, pull: &PullRequest) -> Result<bool> {
        let Some(head) = self.repo_by_id(pull.head_repo_id.unwrap_or(-1)).await? else { return Ok(false) };
        if !head.is_private || head.id == issue.repo_id {
            return Ok(true);
        }
        let Some(base) = self.repo_by_id(issue.repo_id).await? else { return Ok(false) };
        if private_into_public(&base, &head) {
            return Ok(false);
        }
        let author = match issue.author_id {
            Some(id) => self.user_by_id(id).await?,
            None => None,
        };
        Ok(self.access(&head, author.as_ref()).await? >= Access::Read)
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
        let mut tx = self.write_tx().await?;
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
        self.record(Activity::Issue { issue_id: issue.id, actor, action: IssueAction::Merged }).await;
        self.move_cards_of(issue.id, false, actor).await
    }

    /// After refs of `repo_id` changed: updates the open pull requests into
    /// or out of it (see [`pull_tips`](Self::pull_tips)), and marks those
    /// into it merged whose head is now contained in the base branch
    /// (merged outside keel, e.g. with `git push`).
    pub async fn sync_pulls(&self, repo_id: i64, pusher: Option<i64>) -> Result<()> {
        let open: Vec<(i64, i64)> = sqlx::query_as(
            "SELECT i.repo_id, i.number FROM issues i JOIN pull_requests p ON p.issue_id = i.id
             JOIN repositories r ON r.id = i.repo_id
             WHERE i.state = 'open' AND (i.repo_id = ?1 OR p.head_repo_id = ?1) AND r.archived_at IS NULL",
        )
        .bind(repo_id)
        .fetch_all(&self.db)
        .await?;
        let mut bases: Vec<i64> = open.iter().map(|(r, _)| *r).collect();
        bases.sort_unstable();
        bases.dedup();
        for (base_repo, number) in open {
            let Some(issue) = self.issue(base_repo, number).await? else { continue };
            let Some(pull) = self.pull(issue.id).await? else { continue };
            let pinned = self.git(base_repo, move |g| Ok(g.ref_tip(&pull_ref(number)))).await?;
            let (Some(base), Some(head)) = self.pull_tips(&issue, &pull).await? else {
                continue;
            };
            if pinned.is_some_and(|p| p != head) {
                let action = IssueAction::Synchronized;
                self.record(Activity::Issue { issue_id: issue.id, actor: pusher, action }).await;
            }
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
        // New commits may have been what auto-merge waited for (a stale
        // review dismissed, conflicts resolved).
        for repo_id in bases {
            self.auto_merge_soon(repo_id);
        }
        Ok(())
    }
}

/// Commits listed in a squash commit's message.
const SQUASH_LIST: usize = 250;

impl Core {
    /// Merges an open pull request as `user` (who needs write access), if
    /// the base branch's rules allow it, and optionally deletes the head
    /// branch (if `user` may). Returns the merge commit.
    pub async fn merge_pull(&self, repo: &Repo, issue: &Issue, user: &User, style: MergeStyle, delete_branch: bool) -> Result<Oid> {
        if self.access(repo, Some(user)).await? < Access::Write {
            return invalid("Only people with write access can merge.");
        }
        if !issue.is_open() {
            return invalid("This pull request is not open.");
        }
        if self.is_mirror(repo.id).await? {
            return invalid("This repository is a mirror and can't be changed here.");
        }
        let pull = self.pull(issue.id).await?.ok_or(Error::NotFound)?;
        if pull.is_draft {
            return invalid("This pull request is still a draft.");
        }
        let (Some(base), Some(head)) = self.pull_tips(issue, &pull).await? else {
            return invalid("A branch is missing.");
        };
        let head_str = head.to_string();
        let check = self.merge_check(repo, issue, &pull, Some(&head_str)).await?;
        if let Some(blocked) = check.blocked() {
            return invalid(format!("{} is protected: {blocked}", pull.base_ref));
        }
        // keel signs the commits it makes; a merge commit (or a
        // fast-forward) also brings the pull request's own.
        let brings_commits = style == MergeStyle::Merge
            || (style == MergeStyle::Rebase && self.git(repo.id, move |g| g.is_ancestor(base, head)).await?);
        if check.rule.as_ref().is_some_and(|r| r.require_signed)
            && brings_commits
            && let Some(why) = self.unverified(repo.id, &pull.base_ref, base, head).await?
        {
            return invalid(format!("{why} Squash and merge instead, which makes one new, signed commit."));
        }
        let number = issue.number;
        let head_label = pull.head_label(issue);
        let commits = self.git(repo.id, move |g| g.commits_between(base, head, SQUASH_LIST)).await?;
        let message = match style {
            MergeStyle::Merge => format!("Merge pull request #{number} from {head_label}\n\n{}", issue.title),
            MergeStyle::Squash => {
                let list: Vec<String> = commits.iter().map(|c| format!("* {}", c.summary)).collect();
                format!("{} (#{number})\n\n{}", issue.title, list.join("\n"))
            }
            // Each commit keeps its own.
            MergeStyle::Rebase => String::new(),
        };
        let base_ref = pull.base_ref.clone();
        let (name, email, signer) = (user.username.clone(), user.email.clone(), self.signer());
        let before = self.ref_snapshot(repo.id).await?;
        let merged = self
            .git(repo.id, move |g| {
                let merge_base = g.merge_base(base, head)?;
                let who = Identity::new(&name, &email).signed_by(&*signer);
                let commit = match g.merge(&base_ref, base, head, style, &message, &who) {
                    Err(keel_git::Error::Rebase(why)) => return Ok(Err(why)),
                    commit => commit?,
                };
                g.update_ref(&pull_ref(number), Some(head), None)?;
                Ok(Ok((merge_base, commit)))
            })
            .await;
        let (merge_base, commit) = match merged {
            Ok(Ok(v)) => v,
            Ok(Err(why)) => return invalid(format!("Couldn’t rebase: {why}. Merge or squash it instead.")),
            Err(Error::Internal(e)) => {
                tracing::info!("merge of {}/{}#{number} failed: {e:#}", repo.owner, repo.name);
                return invalid("Merging failed: the branches changed or conflict. Reload and try again.");
            }
            Err(e) => return Err(e),
        };
        self.mark_merged(issue, Some(user.id), merge_base, head, Some(commit)).await?;
        // Large files uploaded to a fork come along.
        if let Some(head_repo) = pull.head_repo_id {
            self.share_lfs_objects(head_repo, repo.id, merge_base, head).await?;
        }

        if delete_branch
            && let Some(head_repo) = self.repo_by_id(pull.head_repo_id.unwrap_or(-1)).await?
            && self.access(&head_repo, Some(user)).await? >= Access::Write
            && self.may_update_branch(head_repo.id, &pull.head_ref, true, true).await?
        {
            let head_before = self.ref_snapshot(head_repo.id).await?;
            let branch = format!("refs/heads/{}", pull.head_ref);
            // Only if nobody pushed to it meanwhile.
            let deleted = self.git(head_repo.id, move |g| Ok(g.update_ref(&branch, None, Some(head)).is_ok())).await?;
            if deleted && head_repo.id != repo.id {
                self.after_push(head_repo.id, Some(user.id), Some(head_before)).await?;
            }
        }
        // After any branch deletion here, so the push event includes it.
        self.after_push(repo.id, Some(user.id), Some(before)).await?;
        Ok(commit)
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

        assert!(core.create_pull(&repo, &ada, "Same", "", "main", &repo, "main", false).await.is_err());
        assert!(core.create_pull(&repo, &ada, "Nope", "", "main", &repo, "nope", false).await.is_err());
        assert!(core.create_pull(&repo, &ada, "Behind", "", "feature", &repo, "main", false).await.is_err(), "nothing to merge");
        let issue = core.create_pull(&repo, &ada, "Add b", "", "main", &repo, "feature", false).await.unwrap();
        assert!(issue.is_pull);
        assert!(core.create_pull(&repo, &ada, "Again", "", "main", &repo, "feature", false).await.is_err(), "duplicate");
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
    async fn rebase_merge() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let bob = core.register("bob", "bob@example.com", "password1").await.unwrap();
        let repo = core.create_repo(&ada, "engine", "", false).await.unwrap();
        let work = push_branches(&core, &repo);
        // Main moves on, so the feature commit has to be replayed.
        let git = |args: &[&str]| {
            let out = Command::new("git").args(args).current_dir(work.path()).output().unwrap();
            assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        git(&["checkout", "-q", "main"]);
        std::fs::write(work.path().join("c"), "1").unwrap();
        git(&["add", "-A"]);
        git(&["-c", "user.name=Ada", "-c", "user.email=ada@example.com", "commit", "-qm", "main moves"]);
        git(&["push", "-q", core.repo_path(&repo).to_str().unwrap(), "main"]);
        let main = core.git(repo.id, |g| Ok(g.branch_tip("main"))).await.unwrap().unwrap();
        let feature = core.git(repo.id, |g| Ok(g.branch_tip("feature"))).await.unwrap().unwrap();

        let issue = core.create_pull(&repo, &bob, "Add b", "", "main", &repo, "feature", false).await.unwrap();
        let tip = core.merge_pull(&repo, &issue, &ada, MergeStyle::Rebase, true).await.unwrap();
        let detail = core.git(repo.id, move |g| g.commit_detail(tip)).await.unwrap();
        assert_eq!(detail.parents, [main], "linear");
        assert_eq!(detail.commit.summary, "feature");
        assert_ne!(tip, feature);
        let pull = core.pull(issue.id).await.unwrap().unwrap();
        assert_eq!(pull.merge_commit, Some(tip.to_string()));
        assert_eq!(core.issue(repo.id, 1).await.unwrap().unwrap().state, "merged");
        assert_eq!(core.git(repo.id, |g| Ok(g.branch_tip("feature"))).await.unwrap(), None, "deleted");
    }

    #[tokio::test]
    async fn applying_suggestions() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let bob = core.register("bob", "bob@example.com", "password1").await.unwrap();
        let repo = core.create_repo(&ada, "engine", "", false).await.unwrap();
        let _work = push_branches(&core, &repo);
        let issue = core.create_pull(&repo, &ada, "Add b", "", "main", &repo, "feature", false).await.unwrap();
        let tip = core.git(repo.id, |g| Ok(g.branch_tip("feature"))).await.unwrap().unwrap();
        let blob = core.git(repo.id, move |g| Ok(g.blob(tip, "b")?.unwrap().id)).await.unwrap();
        let anchor = crate::InlineAnchor {
            path: "b".into(),
            new_side: true,
            line: 1,
            commit_id: tip,
            blob_id: blob,
            hunk: "@@ -0,0 +1 @@\n+1".into(),
        };
        let plain = core.add_inline_comment(&issue, &bob, &anchor, "Hmm").await.unwrap();
        let plain = core.comment(issue.id, plain).await.unwrap().unwrap();
        assert!(core.apply_suggestion(&issue, &plain, &ada).await.is_err(), "no suggestion");
        let id = core.add_inline_comment(&issue, &bob, &anchor, "```suggestion\ntwo\n```").await.unwrap();
        let c = core.comment(issue.id, id).await.unwrap().unwrap();
        assert!(core.apply_suggestion(&issue, &c, &bob).await.is_err(), "bob can't push");

        core.save_branch_rule(repo.id, &crate::BranchRule {
            id: 0,
            pattern: "feat*".into(),
            require_pull: true,
            required_approvals: 0,
            dismiss_stale: false,
            allow_force_push: false,
            required_checks: String::new(),
            require_signed: false,
        })
        .await
        .unwrap();
        let err = core.apply_suggestion(&issue, &c, &ada).await.unwrap_err();
        assert!(err.to_string().contains("protected"), "{err}");
        let rule = core.branch_rules(repo.id).await.unwrap()[0].id;
        core.delete_branch_rule(repo.id, rule).await.unwrap();

        let commit = core.apply_suggestion(&issue, &c, &ada).await.unwrap();
        let (new_tip, text) = core
            .git(repo.id, move |g| Ok((g.branch_tip("feature"), g.blob(commit, "b")?.unwrap().data)))
            .await
            .unwrap();
        assert_eq!((new_tip, text.as_slice()), (Some(commit), &b"two"[..]));
        core.sync_pulls(repo.id, None).await.unwrap(); // after_push does this in the background
        assert_eq!(core.git(repo.id, |g| Ok(g.ref_tip("refs/pull/1/head"))).await.unwrap(), Some(commit));
        let err = core.apply_suggestion(&issue, &c, &ada).await.unwrap_err();
        assert!(err.to_string().contains("changed"), "{err}");
    }

    #[tokio::test]
    async fn verified_commits() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let repo = core.create_repo(&ada, "engine", "", false).await.unwrap();
        let _work = push_branches(&core, &repo);
        let tip = |b: &'static str| core.git(repo.id, move |g| Ok(g.branch_tip(b).unwrap()));
        let verification = |id: Oid| {
            let core = core.clone();
            async move { core.verify_commits(repo.id, vec![id]).await.unwrap().remove(&id) }
        };
        assert!(verification(tip("feature").await.unwrap()).await.is_none(), "pushed unsigned");

        // Commits keel makes are signed with its key.
        let write = crate::FileWrite { path: "c".into(), content: Some(b"c".to_vec()), from: None, new: true };
        let edit = crate::FileEdit { branch: "main".into(), base: None, new_branch: None, message: "Add c".into(), files: vec![write] };
        let (_, edited) = core.commit_file_edit(&repo, &ada, edit).await.unwrap();
        let v = verification(edited).await.unwrap();
        assert!(v.verified, "{v:?}");
        assert_eq!(v.made_on.as_deref(), Some("keel"));
        assert_eq!(v.describe(), format!("Made on keel and signed with its key {}.", v.key));

        // A rule requiring verified commits: a merge commit would bring the
        // pull request's unsigned ones; a squash is one new, signed commit.
        core.save_branch_rule(repo.id, &crate::BranchRule {
            id: 0,
            pattern: "main".into(),
            require_pull: false,
            required_approvals: 0,
            dismiss_stale: false,
            allow_force_push: false,
            required_checks: String::new(),
            require_signed: true,
        })
        .await
        .unwrap();
        let env = core.receive_pack_env(repo.id).await.unwrap();
        assert!(env.iter().any(|(k, _)| k == "KEEL_DATA_DIR"));
        let issue = core.create_pull(&repo, &ada, "Feature", "", "main", &repo, "feature", false).await.unwrap();
        let err = core.merge_pull(&repo, &issue, &ada, MergeStyle::Merge, false).await.unwrap_err().to_string();
        assert!(err.contains("main requires verified commits; not verified: "), "{err}");
        assert!(err.contains("(unsigned)"), "{err}");
        assert_eq!(tip("main").await.unwrap(), edited);
        let squashed = core.merge_pull(&repo, &issue, &ada, MergeStyle::Squash, false).await.unwrap();
        assert!(verification(squashed).await.unwrap().verified);
        assert_eq!(core.unverified(repo.id, "main", edited, squashed).await.unwrap(), None);
    }

    #[tokio::test]
    async fn pull_from_private_parent() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let bob = core.register("bob", "bob@example.com", "password1").await.unwrap();
        let secret = core.create_repo(&ada, "secret", "", true).await.unwrap();
        let work = push_branches(&core, &secret);
        core.set_collaborator(&secret, "bob", Access::Read).await.unwrap();
        let fork = core.fork_repo(&secret, &bob, "secret").await.unwrap();
        assert!(fork.is_private);
        // Upstream changes into Bob's fork.
        let issue = core.create_pull(&fork, &bob, "Sync", "", "main", &secret, "feature", false).await.unwrap();
        let pull = core.pull(issue.id).await.unwrap().unwrap();
        let pinned = || core.git(fork.id, |g| Ok(g.ref_tip("refs/pull/1/head")));
        let feature = core.git(secret.id, |g| Ok(g.branch_tip("feature"))).await.unwrap();
        assert_eq!(pinned().await.unwrap(), feature);

        // Bob loses access; Ada's new commits stay out of his fork.
        core.remove_collaborator(secret.id, bob.id).await.unwrap();
        let git = |args: &[&str]| {
            let out = Command::new("git").args(args).current_dir(work.path()).env("GIT_CONFIG_GLOBAL", "/dev/null").output().unwrap();
            assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        };
        std::fs::write(work.path().join("b"), "private").unwrap();
        git(&["-c", "user.name=Ada", "-c", "user.email=ada@example.com", "commit", "-qam", "private"]);
        git(&["push", "-q", core.repo_path(&secret).to_str().unwrap(), "feature"]);
        core.sync_pulls(secret.id, Some(ada.id)).await.unwrap();
        assert_eq!(pinned().await.unwrap(), feature);
        assert_eq!(core.pull_tips(&issue, &pull).await.unwrap().1, None);
        let merged = core.merge_pull(&fork, &issue, &bob, keel_git::MergeStyle::Merge, false).await;
        assert!(merged.is_err());
        assert_eq!(pinned().await.unwrap(), feature);
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

        assert!(core.create_pull(&repo, &bob, "x", "", "main", &unrelated, "main", false).await.is_err(), "not a fork");
        let issue = core.create_pull(&repo, &bob, "Add c", "", "main", &fork, "topic", false).await.unwrap();
        let pull = core.pull(issue.id).await.unwrap().unwrap();
        assert!(pull.is_cross_repo(&issue));
        assert_eq!(pull.head_label(&issue), "bob:topic");
        assert_eq!(pull.head_repo.as_deref(), Some("bob/engine"));
        assert_eq!(core.git(repo.id, |g| Ok(g.ref_tip("refs/pull/1/head"))).await.unwrap(), Some(topic));
        let refs = Command::new("git").arg("-C").arg(core.repo_path(&repo)).arg("for-each-ref").output().unwrap();
        let refs = String::from_utf8_lossy(&refs.stdout);
        assert!(!refs.contains("incoming"), "temporary ref removed: {refs}");
        assert_eq!(core.git(repo.id, |g| Ok(g.branch_tip("topic"))).await.unwrap(), None);
        assert!(core.create_pull(&repo, &bob, "Again", "", "main", &fork, "topic", false).await.is_err(), "duplicate");

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

        // Merging it lets the base read the fork's large files.
        let oid = "e".repeat(64);
        std::fs::create_dir_all(core.lfs_path(&oid).parent().unwrap()).unwrap();
        std::fs::write(core.lfs_path(&oid), "big").unwrap();
        core.lfs_link(fork.id, &oid, 3).await.unwrap();
        git(&["checkout", "-qb", "lfs", "topic"]);
        std::fs::write(work.path().join("big.bin"), format!("version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize 3\n")).unwrap();
        git(&["add", "-A"]);
        git(&["-c", "user.name=Bob", "-c", "user.email=bob@example.com", "commit", "-qm", "big"]);
        git(&["push", "-q", core.repo_path(&fork).to_str().unwrap(), "lfs"]);
        let lfs_pull = core.create_pull(&repo, &bob, "Big", "", "main", &fork, "lfs", false).await.unwrap();
        assert_eq!(core.lfs_object(repo.id, &oid).await.unwrap(), None);
        core.merge_pull(&repo, &lfs_pull, &ada, keel_git::MergeStyle::Merge, false).await.unwrap();
        assert_eq!(core.lfs_object(repo.id, &oid).await.unwrap(), Some(3));

        // Making the fork private keeps its new commits out of the public
        // base.
        std::fs::write(work.path().join("c"), "3").unwrap();
        git(&["checkout", "-q", "topic"]);
        git(&["-c", "user.name=Bob", "-c", "user.email=bob@example.com", "commit", "-qam", "public"]);
        git(&["push", "-q", core.repo_path(&fork).to_str().unwrap(), "topic"]);
        let open = core.create_pull(&repo, &bob, "Later", "", "main", &fork, "topic", false).await.unwrap();
        let open_pull = core.pull(open.id).await.unwrap().unwrap();
        let pinned = |n: i64| core.git(repo.id, move |g| Ok(g.ref_tip(&pull_ref(n))));
        let before = pinned(open.number).await.unwrap();
        assert!(before.is_some());
        assert_eq!(core.pull_json(&repo, &open).await.unwrap()["head"]["repo"]["full_name"], "bob/engine");
        core.update_repo(fork.id, "", true).await.unwrap();
        assert!(core.pull_json(&repo, &open).await.unwrap()["head"]["repo"].is_null(), "private now");
        std::fs::write(work.path().join("c"), "4").unwrap();
        git(&["-c", "user.name=Bob", "-c", "user.email=bob@example.com", "commit", "-qam", "secret"]);
        git(&["push", "-q", core.repo_path(&fork).to_str().unwrap(), "topic"]);
        core.sync_pulls(fork.id, Some(bob.id)).await.unwrap();
        assert_eq!(core.pull_tips(&open, &open_pull).await.unwrap().1, None);
        assert_eq!(pinned(open.number).await.unwrap(), before);
        // Nor can a new pull request bring them: opening one would copy
        // the private branch into the public repository.
        git(&["push", "-q", core.repo_path(&fork).to_str().unwrap(), "topic:hidden"]);
        let fork_now = core.repo_by_id(fork.id).await.unwrap().unwrap();
        let hidden = core.git(fork.id, |g| Ok(g.branch_tip("hidden"))).await.unwrap().unwrap();
        let e = core.create_pull(&repo, &bob, "Oops", "", "main", &fork_now, "hidden", false).await.unwrap_err();
        assert!(matches!(&e, Error::Invalid(m) if m.contains("bob/engine is private")), "{e:?}");
        assert!(core.git(repo.id, move |g| Ok(g.commit_summary(hidden).is_err())).await.unwrap(), "not copied");

        // Deleting the fork keeps the pull request.
        core.delete_repo(&fork).await.unwrap();
        let pull = core.pull(issue.id).await.unwrap().unwrap();
        assert_eq!((pull.head_repo_id, pull.head_label(&issue).as_str()), (None, "unknown:topic"));
    }
}
