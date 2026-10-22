//! Code review on pull requests: comments on lines of the diff (with reply
//! threads) and review verdicts.

use keel_git::Oid;

use crate::activity::Activity;
use crate::issues::{insert_comment, touch, validate_comment};
use crate::{Comment, Core, Error, Issue, Result, User, invalid, now};

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
        let mut tx = self.db.begin().await?;
        let id = insert_comment(&mut tx, issue.id, Some(author.id), verdict.as_str(), body).await?;
        tx.commit().await?;
        self.record(Activity::Comment { issue_id: issue.id, comment_id: id }).await;
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
    }
}
