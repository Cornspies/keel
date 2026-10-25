//! Draft pull requests, and merging automatically once nothing blocks it.

use keel_git::MergeStyle;

use crate::activity::{Activity, IssueAction};
use crate::issues::insert_comment;
use crate::{Access, Core, Error, Issue, Repo, Result, User, invalid};

pub fn style_name(style: MergeStyle) -> &'static str {
    match style {
        MergeStyle::Merge => "merge",
        MergeStyle::Squash => "squash",
    }
}

impl Core {
    /// Marks an open pull request as a draft, or ready for review. Its
    /// author and people with write access may. Turning it into a draft
    /// also turns auto-merge off.
    pub async fn set_draft(&self, repo: &Repo, issue: &Issue, user: &User, draft: bool) -> Result<()> {
        if !issue.is_pull {
            return Err(Error::NotFound);
        }
        if !issue.is_author(user) && self.access(repo, Some(user)).await? < Access::Write {
            return invalid("Only the author and people with write access can do that.");
        }
        if !issue.is_open() {
            return invalid("This pull request is not open.");
        }
        let mut tx = self.db.begin().await?;
        let changed = sqlx::query("UPDATE pull_requests SET is_draft = ?2 WHERE issue_id = ?1 AND is_draft != ?2")
            .bind(issue.id)
            .bind(draft)
            .execute(&mut *tx)
            .await?;
        if changed.rows_affected() == 0 {
            return Ok(());
        }
        let kind = if draft { "converted_to_draft" } else { "ready_for_review" };
        insert_comment(&mut tx, issue.id, Some(user.id), kind, "").await?;
        let auto_merge_was_on = draft && clear_auto_merge(&mut tx, issue.id).await?;
        if auto_merge_was_on {
            insert_comment(&mut tx, issue.id, Some(user.id), "auto_merge_disabled", "").await?;
        }
        tx.commit().await?;
        let action = if draft { IssueAction::ConvertedToDraft } else { IssueAction::ReadyForReview };
        self.record(Activity::Issue { issue_id: issue.id, actor: Some(user.id), action }).await;
        if auto_merge_was_on {
            let action = IssueAction::AutoMergeDisabled;
            self.record(Activity::Issue { issue_id: issue.id, actor: Some(user.id), action }).await;
        }
        Ok(())
    }

    /// Turns auto-merge on for an open pull request: once its base
    /// branch's rules are met it is merged as `user` (who needs write
    /// access), right away if they already are.
    pub async fn enable_auto_merge(&self, repo: &Repo, issue: &Issue, user: &User, style: MergeStyle, delete_branch: bool) -> Result<()> {
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
            return invalid("Drafts can't be merged automatically; mark it ready for review first.");
        }
        let mut tx = self.db.begin().await?;
        let was_on = pull.auto_merge_by.is_some();
        sqlx::query(
            "UPDATE pull_requests SET auto_merge_by = ?2, auto_merge_style = ?3, auto_merge_delete = ?4 WHERE issue_id = ?1",
        )
        .bind(issue.id)
        .bind(user.id)
        .bind(style_name(style))
        .bind(delete_branch)
        .execute(&mut *tx)
        .await?;
        if !was_on {
            insert_comment(&mut tx, issue.id, Some(user.id), "auto_merge_enabled", "").await?;
        }
        tx.commit().await?;
        if !was_on {
            let action = IssueAction::AutoMergeEnabled;
            self.record(Activity::Issue { issue_id: issue.id, actor: Some(user.id), action }).await;
        }
        self.run_auto_merge(repo.id).await
    }

    /// Turns auto-merge off, as `user` (the author, who turned it on, or
    /// someone with write access).
    pub async fn disable_auto_merge(&self, repo: &Repo, issue: &Issue, user: &User) -> Result<()> {
        let pull = self.pull(issue.id).await?.ok_or(Error::NotFound)?;
        let Some(by) = &pull.auto_merge_by else { return Ok(()) };
        if !issue.is_author(user) && by != &user.username && self.access(repo, Some(user)).await? < Access::Write {
            return invalid("Only the author and people with write access can do that.");
        }
        self.stop_auto_merge(issue, Some(user.id)).await
    }

    async fn stop_auto_merge(&self, issue: &Issue, actor: Option<i64>) -> Result<()> {
        let mut tx = self.db.begin().await?;
        if clear_auto_merge(&mut tx, issue.id).await? {
            insert_comment(&mut tx, issue.id, actor, "auto_merge_disabled", "").await?;
            tx.commit().await?;
            self.record(Activity::Issue { issue_id: issue.id, actor, action: IssueAction::AutoMergeDisabled }).await;
        }
        Ok(())
    }

    /// Checks the auto-merge pull requests into `repo_id` again soon, in
    /// the background (after something they wait for may have changed).
    pub(crate) fn auto_merge_soon(&self, repo_id: i64) {
        let core = self.clone();
        tokio::spawn(async move {
            if let Err(e) = core.run_auto_merge(repo_id).await {
                tracing::warn!(repo = repo_id, "auto-merge failed: {e}");
            }
        });
    }

    /// Merges the pull requests into `repo_id` with auto-merge on whose
    /// base branch's rules are now met. Those that still wait (on checks,
    /// reviews or conflicts) stay as they are; auto-merge turns off if
    /// whoever turned it on can't merge any more.
    pub async fn run_auto_merge(&self, repo_id: i64) -> Result<()> {
        let _one_at_a_time = self.auto_merge_lock.lock().await;
        let waiting: Vec<(i64, i64, String, bool)> = sqlx::query_as(
            "SELECT i.number, p.auto_merge_by, p.auto_merge_style, p.auto_merge_delete
             FROM pull_requests p JOIN issues i ON i.id = p.issue_id
             WHERE i.repo_id = ?1 AND i.state = 'open' AND p.auto_merge_by IS NOT NULL AND NOT p.is_draft
             ORDER BY i.number",
        )
        .bind(repo_id)
        .fetch_all(&self.db)
        .await?;
        if waiting.is_empty() {
            return Ok(());
        }
        let Some(repo) = self.repo_by_id(repo_id).await? else { return Ok(()) };
        for (number, by, style, delete_branch) in waiting {
            let Some(issue) = self.issue(repo_id, number).await? else { continue };
            let user = self.user_by_id(by).await?.filter(|u| !u.disabled);
            let Some(user) = user else {
                self.stop_auto_merge(&issue, None).await?;
                continue;
            };
            if self.access(&repo, Some(&user)).await? < Access::Write {
                self.stop_auto_merge(&issue, None).await?;
                continue;
            }
            let Some(pull) = self.pull(issue.id).await? else { continue };
            let (Some(_), Some(head)) = self.pull_tips(&issue, &pull).await? else { continue };
            let head = head.to_string();
            if self.merge_check(&repo, &issue, &pull, Some(&head)).await?.blocked().is_some() {
                continue;
            }
            let style = if style == "squash" { MergeStyle::Squash } else { MergeStyle::Merge };
            match self.merge_pull(&repo, &issue, &user, style, delete_branch).await {
                Ok(_) => tracing::info!("auto-merged {}/{}#{number}", repo.owner, repo.name),
                // Conflicts, or the branches moved meanwhile: a later push
                // tries again.
                Err(Error::Invalid(why)) => tracing::debug!("{}/{}#{number} not auto-merged: {why}", repo.owner, repo.name),
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

/// Turns auto-merge off; whether it was on.
async fn clear_auto_merge(conn: &mut sqlx::SqliteConnection, issue_id: i64) -> Result<bool> {
    let done = sqlx::query("UPDATE pull_requests SET auto_merge_by = NULL WHERE issue_id = ?1 AND auto_merge_by IS NOT NULL")
        .bind(issue_id)
        .execute(conn)
        .await?;
    Ok(done.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[tokio::test]
    async fn drafts_and_auto_merge() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let bob = core.register("bob", "bob@example.com", "password1").await.unwrap();
        let repo = core.create_repo(&ada, "engine", "", false).await.unwrap();
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
        git(&["init", "-q", "-b", "main"]);
        std::fs::write(work.path().join("a"), "1").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "base"]);
        git(&["checkout", "-qb", "feature"]);
        std::fs::write(work.path().join("b"), "1").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "feature"]);
        git(&["push", "-q", core.repo_path(&repo).to_str().unwrap(), "main", "feature"]);

        let issue = core.create_pull(&repo, &ada, "Add b", "", "main", &repo, "feature", true).await.unwrap();
        assert!(issue.is_draft);
        let err = core.merge_pull(&repo, &issue, &ada, MergeStyle::Merge, false).await.unwrap_err();
        assert!(err.to_string().contains("draft"), "{err}");
        assert!(core.enable_auto_merge(&repo, &issue, &ada, MergeStyle::Squash, true).await.is_err(), "drafts can't");
        assert!(core.set_draft(&repo, &issue, &bob, false).await.is_err(), "bob is neither author nor writer");
        core.set_draft(&repo, &issue, &ada, false).await.unwrap();
        let issue = core.issue(repo.id, 1).await.unwrap().unwrap();
        assert!(!issue.is_draft);

        // A rule needs a check to pass; auto-merge waits for it.
        core.save_branch_rule(repo.id, &crate::BranchRule {
            id: 0,
            pattern: "main".into(),
            require_pull: true,
            required_approvals: 0,
            dismiss_stale: false,
            allow_force_push: false,
            required_checks: "ci".into(),
        })
        .await
        .unwrap();
        assert!(core.enable_auto_merge(&repo, &issue, &bob, MergeStyle::Squash, true).await.is_err(), "bob can't merge");
        core.enable_auto_merge(&repo, &issue, &ada, MergeStyle::Squash, true).await.unwrap();
        let pull = core.pull(issue.id).await.unwrap().unwrap();
        assert_eq!((pull.auto_merge_by.as_deref(), pull.auto_merge_style.as_str()), (Some("ada"), "squash"));
        assert_eq!(core.issue(repo.id, 1).await.unwrap().unwrap().state, "open");

        // Converting to a draft turns it off; turning it on again works.
        core.set_draft(&repo, &issue, &ada, true).await.unwrap();
        assert_eq!(core.pull(issue.id).await.unwrap().unwrap().auto_merge_by, None);
        core.set_draft(&repo, &issue, &ada, false).await.unwrap();
        core.enable_auto_merge(&repo, &issue, &ada, MergeStyle::Squash, true).await.unwrap();

        let head = core.git(repo.id, |g| Ok(g.branch_tip("feature"))).await.unwrap().unwrap().to_string();
        core.set_status(&repo, &bob, &head, "pending", "ci", "", "").await.unwrap();
        core.run_auto_merge(repo.id).await.unwrap();
        assert_eq!(core.issue(repo.id, 1).await.unwrap().unwrap().state, "open", "still pending");
        core.set_status(&repo, &bob, &head, "success", "ci", "", "").await.unwrap();
        core.run_auto_merge(repo.id).await.unwrap();
        let merged = core.issue(repo.id, 1).await.unwrap().unwrap();
        assert_eq!(merged.state, "merged");
        let pull = core.pull(issue.id).await.unwrap().unwrap();
        assert_eq!(pull.merged_by.as_deref(), Some("ada"), "merged as whoever turned it on");
        assert_eq!(core.git(repo.id, |g| Ok(g.branch_tip("feature"))).await.unwrap(), None, "branch deleted");
        let kinds: Vec<String> = core.comments(issue.id).await.unwrap().into_iter().map(|c| c.kind).collect();
        assert_eq!(
            kinds,
            [
                "ready_for_review",
                "auto_merge_enabled",
                "converted_to_draft",
                "auto_merge_disabled",
                "ready_for_review",
                "auto_merge_enabled",
                "merged"
            ]
        );
    }
}
