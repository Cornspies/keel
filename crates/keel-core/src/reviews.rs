//! Code review on pull requests: comments on lines of the diff (with reply
//! threads), suggested changes, review requests and review verdicts.

use keel_git::{Identity, Oid, replace_line};

use crate::activity::{Activity, IssueAction};
use crate::issues::{insert_comment, touch, validate_comment};
use crate::protection::pinned_head;
use crate::{Access, Comment, Core, Error, Issue, Repo, Result, User, invalid, now};

const MAX_REVIEWERS: usize = 10;

/// The first suggested change in a comment: the lines of a ```` ```suggestion ````
/// block, which replace the commented line (empty: delete it).
pub fn suggestion(body: &str) -> Option<String> {
    let mut lines = body.lines();
    let fence = lines.by_ref().find_map(|l| {
        let t = l.trim_start();
        let ticks = t.len() - t.trim_start_matches('`').len();
        (ticks >= 3 && t[ticks..].trim() == "suggestion").then(|| &t[..ticks])
    })?;
    let mut out = Vec::new();
    for l in lines {
        if l.trim_start().starts_with(fence) && l.trim().trim_start_matches('`').is_empty() {
            return Some(out.join("\n"));
        }
        out.push(l);
    }
    None
}

/// Where an inline comment is made.
#[derive(Clone, Debug)]
pub struct InlineAnchor {
    /// The file as shown in the diff (its new path unless deleted).
    pub path: String,
    /// `true` for the new version of the file, `false` for the old one
    /// (deleted lines).
    pub new_side: bool,
    pub line: i64,
    /// The pull request's head when the comment was made.
    pub commit_id: Oid,
    /// The blob of `path` on that side, so later diffs can tell whether the
    /// line still says the same thing.
    pub blob_id: Oid,
    /// A few lines of diff ending at the commented line.
    pub hunk: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Approved,
    ChangesRequested,
}

impl Verdict {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "approve" | "approved" => Some(Self::Approved),
            "request_changes" | "changes_requested" => Some(Self::ChangesRequested),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::ChangesRequested => "changes_requested",
        }
    }
}

impl Core {
    pub async fn add_inline_comment(
        &self,
        issue: &Issue,
        author: &User,
        anchor: &InlineAnchor,
        body: &str,
    ) -> Result<i64> {
        if !issue.is_pull {
            return Err(Error::NotFound);
        }
        validate_comment(body)?;
        let mut tx = self.db.begin().await?;
        let (id,): (i64,) = sqlx::query_as(
            "INSERT INTO comments (issue_id, author_id, kind, body, created_at, updated_at,
                path, side, line, commit_id, blob_id, hunk)
             VALUES (?1, ?2, 'inline', ?3, ?4, ?4, ?5, ?6, ?7, ?8, ?9, ?10) RETURNING id",
        )
        .bind(issue.id)
        .bind(author.id)
        .bind(body.trim_end())
        .bind(now())
        .bind(&anchor.path)
        .bind(if anchor.new_side { "new" } else { "old" })
        .bind(anchor.line)
        .bind(anchor.commit_id.to_string())
        .bind(anchor.blob_id.to_string())
        .bind(&anchor.hunk)
        .fetch_one(&mut *tx)
        .await?;
        touch(&mut tx, issue.id).await?;
        tx.commit().await?;
        self.record(Activity::Comment { issue_id: issue.id, comment_id: id }).await;
        Ok(id)
    }

    /// Replies to the thread of inline comment `to` (or of the comment it
    /// replies to).
    pub async fn reply_inline(&self, issue: &Issue, to: &Comment, author: &User, body: &str) -> Result<i64> {
        if to.kind != "inline" || to.issue_id != issue.id {
            return Err(Error::NotFound);
        }
        validate_comment(body)?;
        let root = to.reply_to.unwrap_or(to.id);
        let mut tx = self.db.begin().await?;
        let (id,): (i64,) = sqlx::query_as(
            "INSERT INTO comments (issue_id, author_id, kind, body, created_at, updated_at,
                path, side, line, commit_id, blob_id, hunk, reply_to)
             SELECT issue_id, ?2, 'inline', ?3, ?4, ?4, path, side, line, commit_id, blob_id, hunk, id
             FROM comments WHERE id = ?1
             RETURNING id",
        )
        .bind(root)
        .bind(author.id)
        .bind(body.trim_end())
        .bind(now())
        .fetch_one(&mut *tx)
        .await?;
        touch(&mut tx, issue.id).await?;
        tx.commit().await?;
        self.record(Activity::Comment { issue_id: issue.id, comment_id: id }).await;
        Ok(id)
    }

    /// Approves or requests changes, with an optional comment. Authors
    /// can't review their own pull requests.
    pub async fn add_review(&self, issue: &Issue, author: &User, verdict: Verdict, body: &str) -> Result<i64> {
        if !issue.is_pull {
            return Err(Error::NotFound);
        }
        if issue.is_author(author) {
            return invalid("You can't review your own pull request.");
        }
        if !issue.is_open() {
            return invalid("This pull request is not open.");
        }
        if !body.trim().is_empty() {
            validate_comment(body)?;
        }
        // The commit reviewed (the branch's current tip, as shown to the
        // reviewer), so branch rules can disregard stale reviews.
        let head = match self.pull(issue.id).await? {
            Some(pull) => match self.pull_tips(issue, &pull).await?.1 {
                Some(head) => Some(head.to_string()),
                None => pinned_head(self, issue.repo_id, issue.number).await?,
            },
            None => None,
        };
        let mut tx = self.db.begin().await?;
        let id = insert_comment(&mut tx, issue.id, Some(author.id), verdict.as_str(), body).await?;
        sqlx::query("UPDATE comments SET commit_id = ?2 WHERE id = ?1")
            .bind(id)
            .bind(head)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM review_requests WHERE issue_id = ?1 AND user_id = ?2")
            .bind(issue.id)
            .bind(author.id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        self.record(Activity::Comment { issue_id: issue.id, comment_id: id }).await;
        self.auto_merge_soon(issue.repo_id);
        Ok(id)
    }

    /// Each reviewer's latest verdict, by username.
    pub async fn review_states(&self, issue_id: i64) -> Result<Vec<(String, Verdict)>> {
        let rows: Vec<(String, String)> = sqlx::query_as(
            "SELECT u.username, c.kind FROM comments c JOIN users u ON u.id = c.author_id
             WHERE c.id IN (SELECT MAX(id) FROM comments
                            WHERE issue_id = ?1 AND kind IN ('approved', 'changes_requested')
                            GROUP BY author_id)
             ORDER BY c.id",
        )
        .bind(issue_id)
        .fetch_all(&self.db)
        .await?;
        Ok(rows
            .into_iter()
            .filter_map(|(user, kind)| Some((user, Verdict::parse(&kind)?)))
            .collect())
    }
}

impl Core {
    // -- Review requests ----------------------------------------------------

    /// People asked to review a pull request who haven't yet: (id, name).
    pub async fn review_requests(&self, issue_id: i64) -> Result<Vec<(i64, String)>> {
        Ok(sqlx::query_as(
            "SELECT u.id, u.username FROM review_requests r JOIN users u ON u.id = r.user_id
             WHERE r.issue_id = ?1 ORDER BY u.username",
        )
        .bind(issue_id)
        .fetch_all(&self.db)
        .await?)
    }

    /// Who can be asked to review: people with write access other than
    /// the author.
    pub async fn reviewer_candidates(&self, repo: &Repo, issue: &Issue) -> Result<Vec<User>> {
        let mut users = self.assignable_users(repo).await?;
        users.retain(|u| !issue.is_author(u));
        Ok(users)
    }

    /// Sets who is asked to review (ignoring people who can't be) and
    /// records what changed.
    pub async fn set_review_requests(&self, repo: &Repo, issue: &Issue, actor: &User, user_ids: &[i64]) -> Result<()> {
        if !issue.is_pull {
            return Err(Error::NotFound);
        }
        if user_ids.len() > MAX_REVIEWERS {
            return invalid(format!("At most {MAX_REVIEWERS} people can be asked to review."));
        }
        let candidates = self.reviewer_candidates(repo, issue).await?;
        let current: Vec<i64> = self.review_requests(issue.id).await?.into_iter().map(|(id, _)| id).collect();
        let mut tx = self.db.begin().await?;
        let mut changed = Vec::new();
        for user in candidates {
            let (was, is) = (current.contains(&user.id), user_ids.contains(&user.id));
            if was == is {
                continue;
            }
            if is {
                sqlx::query(
                    "INSERT INTO review_requests (issue_id, user_id, requested_by, created_at) VALUES (?1, ?2, ?3, ?4)",
                )
                .bind(issue.id)
                .bind(user.id)
                .bind(actor.id)
                .bind(now())
                .execute(&mut *tx)
                .await?;
            } else {
                sqlx::query("DELETE FROM review_requests WHERE issue_id = ?1 AND user_id = ?2")
                    .bind(issue.id)
                    .bind(user.id)
                    .execute(&mut *tx)
                    .await?;
            }
            let kind = if is { "review_requested" } else { "review_request_removed" };
            insert_comment(&mut tx, issue.id, Some(actor.id), kind, &user.username).await?;
            changed.push(if is { IssueAction::ReviewRequested(user) } else { IssueAction::ReviewRequestRemoved(user) });
        }
        tx.commit().await?;
        for action in changed {
            self.record(Activity::Issue { issue_id: issue.id, actor: Some(actor.id), action }).await;
        }
        Ok(())
    }

    // -- Suggested changes --------------------------------------------------

    /// Commits the change suggested in inline comment `comment` to the pull
    /// request's head branch, as `user` (who needs write access to the
    /// repository that branch is in). Returns the new commit.
    pub async fn apply_suggestion(&self, issue: &Issue, comment: &Comment, user: &User) -> Result<Oid> {
        if comment.kind != "inline" || comment.issue_id != issue.id {
            return Err(Error::NotFound);
        }
        let Some(replacement) = suggestion(&comment.body) else {
            return invalid("That comment doesn't suggest a change.");
        };
        if !issue.is_open() {
            return invalid("This pull request is not open.");
        }
        if comment.side.as_deref() != Some("new") {
            return invalid("Only suggestions on added or unchanged lines can be applied.");
        }
        let pull = self.pull(issue.id).await?.ok_or(Error::NotFound)?;
        let head_repo = match pull.head_repo_id {
            Some(id) => self.repo_by_id(id).await?,
            None => None,
        };
        let Some(head_repo) = head_repo else {
            return invalid("The branch of this pull request no longer exists.");
        };
        if self.access(&head_repo, Some(user)).await? < Access::Write {
            return invalid("Only people who can push to the branch can apply suggestions.");
        }
        if self.is_mirror(head_repo.id).await? {
            return invalid("The branch is in a mirror, which can't be changed here.");
        }
        if !self.may_update_branch(head_repo.id, &pull.head_ref, false, false).await? {
            return invalid(format!("{} is protected; change it through a pull request.", pull.head_ref));
        }
        let (Some(path), Some(line), Some(blob)) = (comment.path.clone(), comment.line, comment.blob_id.clone()) else {
            return Err(Error::NotFound);
        };
        let suggester = comment.author.clone().unwrap_or_else(|| "ghost".into());
        let message = format!("Apply suggestion from code review\n\nSuggested by @{suggester} in #{}.", issue.number);
        let (name, email) = (user.username.clone(), user.email.clone());
        let branch = pull.head_ref.clone();
        let before = self.ref_snapshot(head_repo.id).await?;
        let edited = self
            .git(head_repo.id, move |g| {
                let Some(tip) = g.branch_tip(&branch) else {
                    return Ok(Err("The branch of this pull request no longer exists."));
                };
                let mut outdated = false;
                let who = Identity { name: &name, email: &email };
                let result = g.commit_file_edit(&branch, tip, &path, &message, &who, |text, id| {
                    let text = std::str::from_utf8(text).ok().filter(|_| id.to_string() == blob);
                    match text.and_then(|t| replace_line(t, line as usize, &replacement)) {
                        Some(t) => Ok(t.into_bytes()),
                        None => {
                            outdated = true;
                            Err(keel_git::Error::Other("outdated".into()))
                        }
                    }
                });
                match result {
                    Ok(commit) => Ok(Ok(commit)),
                    Err(_) if outdated => Ok(Err("The file changed since this was suggested.")),
                    Err(e) => Err(e),
                }
            })
            .await;
        let commit = match edited {
            Ok(Ok(commit)) => commit,
            Ok(Err(msg)) => return invalid(msg),
            // E.g. the file is gone, or someone pushed meanwhile.
            Err(Error::Internal(e)) => {
                tracing::info!("applying suggestion {}: {e:#}", comment.id);
                return invalid("The branch changed since this was suggested. Reload and try again.");
            }
            Err(e) => return Err(e),
        };
        self.after_push(head_repo.id, Some(user.id), Some(before)).await?;
        Ok(commit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn inline_comments_and_reviews() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let bob = core.register("bob", "bob@example.com", "password1").await.unwrap();
        let repo = core.create_repo(&ada, "engine", "", false).await.unwrap();
        let issue = core.create_issue(repo.id, &ada, "Not a pull", "").await.unwrap();
        // Pull requests need branches; flip the flag for these tests.
        sqlx::query("UPDATE issues SET is_pull = 1 WHERE id = ?1").bind(issue.id).execute(&core.db).await.unwrap();
        let pull = core.issue(repo.id, 1).await.unwrap().unwrap();

        let anchor = InlineAnchor {
            path: "src/main.rs".into(),
            new_side: true,
            line: 3,
            commit_id: Oid::from_str("1111111111111111111111111111111111111111").unwrap(),
            blob_id: Oid::from_str("2222222222222222222222222222222222222222").unwrap(),
            hunk: "@@ -1,2 +1,3 @@\n a\n+b".into(),
        };
        assert!(core.add_inline_comment(&issue, &bob, &anchor, "x").await.is_err(), "issues have no diff");
        assert!(core.add_inline_comment(&pull, &bob, &anchor, " ").await.is_err());
        let root = core.add_inline_comment(&pull, &bob, &anchor, "Why?").await.unwrap();
        let root = core.comment(pull.id, root).await.unwrap().unwrap();
        assert_eq!((root.path.as_deref(), root.side.as_deref(), root.line), (Some("src/main.rs"), Some("new"), Some(3)));

        let reply = core.reply_inline(&pull, &root, &ada, "Because.").await.unwrap();
        let reply = core.comment(pull.id, reply).await.unwrap().unwrap();
        assert_eq!((reply.reply_to, reply.line), (Some(root.id), Some(3)));
        // Replying to a reply joins the same thread.
        let again = core.reply_inline(&pull, &reply, &bob, "Ok").await.unwrap();
        assert_eq!(core.comment(pull.id, again).await.unwrap().unwrap().reply_to, Some(root.id));
        assert_eq!(core.issue(repo.id, 1).await.unwrap().unwrap().comment_count, 3);

        // Deleting the first comment removes its thread.
        core.delete_comment(&root).await.unwrap();
        assert!(core.comments(pull.id).await.unwrap().is_empty());
        assert_eq!(core.issue(repo.id, 1).await.unwrap().unwrap().comment_count, 0);

        // Reviews.
        assert!(core.add_review(&pull, &ada, Verdict::Approved, "").await.is_err(), "own pull request");
        core.add_review(&pull, &bob, Verdict::ChangesRequested, "Needs tests").await.unwrap();
        assert_eq!(core.review_states(pull.id).await.unwrap(), [("bob".into(), Verdict::ChangesRequested)]);
        core.add_review(&pull, &bob, Verdict::Approved, "").await.unwrap();
        assert_eq!(core.review_states(pull.id).await.unwrap(), [("bob".into(), Verdict::Approved)]);
        assert_eq!(core.issue(repo.id, 1).await.unwrap().unwrap().comment_count, 1, "only reviews with text");

        // Review requests: only people with write access, not the author;
        // reviewing clears the request.
        let eve = core.register("eve", "eve@example.com", "password1").await.unwrap();
        core.set_collaborator(&repo, "bob", Access::Write).await.unwrap();
        core.set_review_requests(&repo, &pull, &ada, &[ada.id, bob.id, eve.id]).await.unwrap();
        assert_eq!(core.review_requests(pull.id).await.unwrap(), [(bob.id, "bob".to_owned())]);
        core.add_review(&pull, &bob, Verdict::Approved, "").await.unwrap();
        assert!(core.review_requests(pull.id).await.unwrap().is_empty());
        core.set_review_requests(&repo, &pull, &ada, &[bob.id]).await.unwrap();
        core.set_review_requests(&repo, &pull, &ada, &[]).await.unwrap();
        let kinds: Vec<String> = core.comments(pull.id).await.unwrap().into_iter().map(|c| c.kind).collect();
        assert!(kinds.ends_with(&["review_requested".into(), "approved".into(), "review_requested".into(), "review_request_removed".into()]));
    }

    #[test]
    fn suggestions() {
        assert_eq!(suggestion("Try:\n```suggestion\nlet x = 1;\n```\nok").as_deref(), Some("let x = 1;"));
        assert_eq!(suggestion("```suggestion\na\nb\n```").as_deref(), Some("a\nb"));
        assert_eq!(suggestion("```suggestion\n```").as_deref(), Some(""), "deletes the line");
        assert_eq!(suggestion("````suggestion\n```\n````").as_deref(), Some("```"));
        assert_eq!(suggestion("```rust\nx\n```"), None);
        assert_eq!(suggestion("```suggestion\nunclosed"), None);
    }
}
