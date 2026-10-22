//! Issues and their comments. Pull requests are issues too (`is_pull`);
//! see `pulls.rs` for what they add.

use sqlx::SqliteConnection;

use crate::{Core, Error, Result, User, invalid, now};

const MAX_TITLE: usize = 256;
const MAX_BODY: usize = 65_536;

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Issue {
    pub id: i64,
    pub repo_id: i64,
    pub number: i64,
    pub author_id: Option<i64>,
    /// Username of the author; `None` if the account was deleted.
    pub author: Option<String>,
    pub title: String,
    pub body: String,
    pub is_pull: bool,
    /// `open`, `closed` or `merged`.
    pub state: String,
    pub comment_count: i64,
    pub created_at: i64,
    pub updated_at: i64,
    pub closed_at: Option<i64>,
}

impl Issue {
    pub fn is_open(&self) -> bool {
        self.state == "open"
    }

    pub fn is_author(&self, user: &User) -> bool {
        self.author_id == Some(user.id)
    }
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Comment {
    pub id: i64,
    pub issue_id: i64,
    pub author_id: Option<i64>,
    pub author: Option<String>,
    /// `comment`, or an event: `closed`, `reopened`, `merged`.
    pub kind: String,
    pub body: String,
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IssueCounts {
    pub open_issues: i64,
    pub closed_issues: i64,
    pub open_pulls: i64,
    /// Closed or merged.
    pub closed_pulls: i64,
}

macro_rules! issue_select {
    () => {
        "SELECT i.id, i.repo_id, i.number, i.author_id, u.username AS author, i.title, i.body,
            i.is_pull, i.state, i.comment_count, i.created_at, i.updated_at, i.closed_at
         FROM issues i LEFT JOIN users u ON u.id = i.author_id "
    };
}

macro_rules! comment_select {
    () => {
        "SELECT c.id, c.issue_id, c.author_id, u.username AS author, c.kind, c.body,
            c.created_at, c.updated_at
         FROM comments c LEFT JOIN users u ON u.id = c.author_id "
    };
}

fn validate(title: &str, body: &str) -> Result<()> {
    if title.trim().is_empty() || title.len() > MAX_TITLE {
        return invalid(format!("Titles are 1–{MAX_TITLE} characters."));
    }
    if body.len() > MAX_BODY {
        return invalid("That text is too long.");
    }
    Ok(())
}

fn validate_comment(body: &str) -> Result<()> {
    if body.trim().is_empty() {
        return invalid("Write something first.");
    }
    if body.len() > MAX_BODY {
        return invalid("That comment is too long.");
    }
    Ok(())
}

/// Inserts an issue with the next number in the repository. A single
/// statement, so concurrent inserts can't pick the same number.
pub(crate) async fn insert_issue(
    conn: &mut SqliteConnection,
    repo_id: i64,
    author: &User,
    title: &str,
    body: &str,
    is_pull: bool,
) -> Result<(i64, i64)> {
    validate(title, body)?;
    let now = now();
    Ok(sqlx::query_as(
        "INSERT INTO issues (repo_id, number, author_id, title, body, is_pull, created_at, updated_at)
         VALUES (?1, (SELECT COALESCE(MAX(number), 0) + 1 FROM issues WHERE repo_id = ?1),
                 ?2, ?3, ?4, ?5, ?6, ?6)
         RETURNING id, number",
    )
    .bind(repo_id)
    .bind(author.id)
    .bind(title.trim())
    .bind(body.trim_end())
    .bind(is_pull)
    .bind(now)
    .fetch_one(conn)
    .await?)
}

/// Adds a timeline entry and bumps the issue's counters.
pub(crate) async fn insert_comment(
    conn: &mut SqliteConnection,
    issue_id: i64,
    author_id: Option<i64>,
    kind: &str,
    body: &str,
) -> Result<i64> {
    let now = now();
    let (id,): (i64,) = sqlx::query_as(
        "INSERT INTO comments (issue_id, author_id, kind, body, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?5) RETURNING id",
    )
    .bind(issue_id)
    .bind(author_id)
    .bind(kind)
    .bind(body.trim_end())
    .bind(now)
    .fetch_one(&mut *conn)
    .await?;
    sqlx::query(
        "UPDATE issues SET updated_at = ?2, comment_count = comment_count + (?3 = 'comment') WHERE id = ?1",
    )
    .bind(issue_id)
    .bind(now)
    .bind(kind)
    .execute(conn)
    .await?;
    Ok(id)
}

impl Core {
    pub async fn create_issue(&self, repo_id: i64, author: &User, title: &str, body: &str) -> Result<Issue> {
        let mut conn = self.db.acquire().await?;
        let (_, number) = insert_issue(&mut conn, repo_id, author, title, body, false).await?;
        drop(conn);
        self.issue(repo_id, number).await?.ok_or(Error::NotFound)
    }

    pub async fn issue(&self, repo_id: i64, number: i64) -> Result<Option<Issue>> {
        Ok(sqlx::query_as(concat!(issue_select!(), "WHERE i.repo_id = ?1 AND i.number = ?2"))
            .bind(repo_id)
            .bind(number)
            .fetch_optional(&self.db)
            .await?)
    }

    /// Issues or pull requests, newest first. `open` selects open ones,
    /// otherwise closed (and merged) ones.
    pub async fn issues(
        &self,
        repo_id: i64,
        pulls: bool,
        open: bool,
        offset: i64,
        limit: i64,
    ) -> Result<Vec<Issue>> {
        Ok(sqlx::query_as(concat!(
            issue_select!(),
            "WHERE i.repo_id = ?1 AND i.is_pull = ?2 AND ((i.state = 'open') = ?3)
             ORDER BY i.number DESC LIMIT ?4 OFFSET ?5"
        ))
        .bind(repo_id)
        .bind(pulls)
        .bind(open)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.db)
        .await?)
    }

    pub async fn issue_counts(&self, repo_id: i64) -> Result<IssueCounts> {
        let rows: Vec<(bool, bool, i64)> = sqlx::query_as(
            "SELECT is_pull, state = 'open', COUNT(*) FROM issues WHERE repo_id = ?1 GROUP BY 1, 2",
        )
        .bind(repo_id)
        .fetch_all(&self.db)
        .await?;
        let mut c = IssueCounts::default();
        for (pull, open, n) in rows {
            match (pull, open) {
                (false, true) => c.open_issues = n,
                (false, false) => c.closed_issues = n,
                (true, true) => c.open_pulls = n,
                (true, false) => c.closed_pulls = n,
            }
        }
        Ok(c)
    }

    pub async fn comments(&self, issue_id: i64) -> Result<Vec<Comment>> {
        Ok(sqlx::query_as(concat!(comment_select!(), "WHERE c.issue_id = ?1 ORDER BY c.id"))
            .bind(issue_id)
            .fetch_all(&self.db)
            .await?)
    }

    pub async fn comment(&self, issue_id: i64, comment_id: i64) -> Result<Option<Comment>> {
        Ok(sqlx::query_as(concat!(comment_select!(), "WHERE c.issue_id = ?1 AND c.id = ?2"))
            .bind(issue_id)
            .bind(comment_id)
            .fetch_optional(&self.db)
            .await?)
    }

    pub async fn add_comment(&self, issue: &Issue, author: &User, body: &str) -> Result<i64> {
        validate_comment(body)?;
        let mut tx = self.db.begin().await?;
        let id = insert_comment(&mut tx, issue.id, Some(author.id), "comment", body).await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Closes or reopens an issue or pull request, recording who did it.
    /// Merged pull requests can't be reopened.
    pub async fn set_issue_open(&self, issue: &Issue, actor: &User, open: bool) -> Result<()> {
        if issue.state == "merged" || issue.is_open() == open {
            return Ok(());
        }
        let mut tx = self.db.begin().await?;
        sqlx::query("UPDATE issues SET state = ?2, closed_at = ?3 WHERE id = ?1")
            .bind(issue.id)
            .bind(if open { "open" } else { "closed" })
            .bind((!open).then(now))
            .execute(&mut *tx)
            .await?;
        insert_comment(&mut tx, issue.id, Some(actor.id), if open { "reopened" } else { "closed" }, "").await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn edit_issue(&self, issue: &Issue, title: &str, body: &str) -> Result<()> {
        validate(title, body)?;
        sqlx::query("UPDATE issues SET title = ?2, body = ?3, updated_at = ?4 WHERE id = ?1")
            .bind(issue.id)
            .bind(title.trim())
            .bind(body.trim_end())
            .bind(now())
            .execute(&self.db)
            .await?;
        Ok(())
    }

    pub async fn edit_comment(&self, comment: &Comment, body: &str) -> Result<()> {
        validate_comment(body)?;
        sqlx::query("UPDATE comments SET body = ?2, updated_at = ?3 WHERE id = ?1 AND kind = 'comment'")
            .bind(comment.id)
            .bind(body.trim_end())
            .bind(now())
            .execute(&self.db)
            .await?;
        Ok(())
    }

    pub async fn delete_comment(&self, comment: &Comment) -> Result<()> {
        let mut tx = self.db.begin().await?;
        let done = sqlx::query("DELETE FROM comments WHERE id = ?1 AND kind = 'comment'")
            .bind(comment.id)
            .execute(&mut *tx)
            .await?;
        if done.rows_affected() > 0 {
            sqlx::query("UPDATE issues SET comment_count = comment_count - 1 WHERE id = ?1")
                .bind(comment.issue_id)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn issue_lifecycle() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let bob = core.register("bob", "bob@example.com", "password1").await.unwrap();
        let repo = core.create_repo(&ada, "engine", "", false).await.unwrap();
        let other = core.create_repo(&ada, "other", "", false).await.unwrap();

        assert!(core.create_issue(repo.id, &bob, "  ", "").await.is_err());
        let first = core.create_issue(repo.id, &bob, "It crashes", "Steps…").await.unwrap();
        let second = core.create_issue(repo.id, &ada, "Docs", "").await.unwrap();
        let elsewhere = core.create_issue(other.id, &ada, "Other", "").await.unwrap();
        assert_eq!((first.number, second.number, elsewhere.number), (1, 2, 1));
        assert_eq!(first.author.as_deref(), Some("bob"));
        assert!(first.is_author(&bob) && !first.is_author(&ada));

        assert!(core.add_comment(&first, &ada, " ").await.is_err());
        let c = core.add_comment(&first, &ada, "Thanks!").await.unwrap();
        core.set_issue_open(&first, &ada, false).await.unwrap();
        let first = core.issue(repo.id, 1).await.unwrap().unwrap();
        assert_eq!((first.state.as_str(), first.comment_count), ("closed", 1));
        let kinds: Vec<String> = core.comments(first.id).await.unwrap().into_iter().map(|c| c.kind).collect();
        assert_eq!(kinds, ["comment", "closed"]);

        let counts = core.issue_counts(repo.id).await.unwrap();
        assert_eq!((counts.open_issues, counts.closed_issues), (1, 1));
        assert_eq!(core.issues(repo.id, false, true, 0, 10).await.unwrap()[0].number, 2);
        assert_eq!(core.issues(repo.id, false, false, 0, 10).await.unwrap()[0].number, 1);

        let comment = core.comment(first.id, c).await.unwrap().unwrap();
        core.edit_comment(&comment, "Thanks a lot!").await.unwrap();
        assert_eq!(core.comment(first.id, c).await.unwrap().unwrap().body, "Thanks a lot!");
        core.delete_comment(&comment).await.unwrap();
        assert_eq!(core.issue(repo.id, 1).await.unwrap().unwrap().comment_count, 0);

        core.edit_issue(&first, "It crashes on start", "More detail").await.unwrap();
        assert_eq!(core.issue(repo.id, 1).await.unwrap().unwrap().title, "It crashes on start");

        // Deleting the author keeps the issue.
        sqlx::query("DELETE FROM users WHERE id = ?1").bind(bob.id).execute(&core.db).await.unwrap();
        assert_eq!(core.issue(repo.id, 1).await.unwrap().unwrap().author, None);
    }
}
