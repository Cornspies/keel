//! Milestones: groups of issues and pull requests with an optional due
//! date, and their progress.

use crate::activity::{Activity, IssueAction};
use crate::issues::insert_comment;
use crate::{Core, Issue, Result, User, invalid, now};

#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct Milestone {
    pub id: i64,
    pub repo_id: i64,
    pub title: String,
    pub description: String,
    /// Unix time of the due date (midnight UTC).
    pub due_on: Option<i64>,
    /// `open` or `closed`.
    pub state: String,
    pub created_at: i64,
    pub closed_at: Option<i64>,
    /// Issues and pull requests in it, by state.
    pub open_items: i64,
    pub closed_items: i64,
}

impl Milestone {
    pub fn is_open(&self) -> bool {
        self.state == "open"
    }

    /// Done, in percent.
    pub fn progress(&self) -> i64 {
        let total = self.open_items + self.closed_items;
        if total == 0 { 0 } else { self.closed_items * 100 / total }
    }

    pub fn is_overdue(&self, now: i64) -> bool {
        self.is_open() && self.due_on.is_some_and(|d| d + 86_400 <= now)
    }
}

macro_rules! milestone_select {
    () => {
        "SELECT m.id, m.repo_id, m.title, m.description, m.due_on, m.state, m.created_at, m.closed_at,
            (SELECT COUNT(*) FROM issues i WHERE i.milestone_id = m.id AND i.state = 'open') AS open_items,
            (SELECT COUNT(*) FROM issues i WHERE i.milestone_id = m.id AND i.state != 'open') AS closed_items
         FROM milestones m "
    };
}

/// Parses a due date as `YYYY-MM-DD` (empty: none) to midnight UTC.
pub fn parse_due_date(s: &str) -> Result<Option<i64>> {
    let s = s.trim();
    if s.is_empty() {
        return Ok(None);
    }
    let format = time::macros::format_description!("[year]-[month]-[day]");
    match time::Date::parse(s, &format) {
        Ok(d) => Ok(Some(d.midnight().assume_utc().unix_timestamp())),
        Err(_) => invalid("Enter the due date as YYYY-MM-DD."),
    }
}

fn validate(title: &str, description: &str) -> Result<()> {
    if title.trim().is_empty() || title.trim().chars().count() > 100 {
        return invalid("Milestone titles are 1–100 characters.");
    }
    if description.len() > 10_000 {
        return invalid("That description is too long.");
    }
    Ok(())
}

impl Core {
    /// Open (`open`) or closed milestones; open ones by due date, closed
    /// ones most recently closed first.
    pub async fn milestones(&self, repo_id: i64, open: bool) -> Result<Vec<Milestone>> {
        Ok(sqlx::query_as(concat!(
            milestone_select!(),
            "WHERE m.repo_id = ?1 AND (m.state = 'open') = ?2
             ORDER BY CASE WHEN ?2 THEN m.due_on IS NULL END, CASE WHEN ?2 THEN m.due_on END,
                m.closed_at DESC, m.title"
        ))
        .bind(repo_id)
        .bind(open)
        .fetch_all(&self.db)
        .await?)
    }

    pub async fn milestone(&self, repo_id: i64, id: i64) -> Result<Option<Milestone>> {
        Ok(sqlx::query_as(concat!(milestone_select!(), "WHERE m.repo_id = ?1 AND m.id = ?2"))
            .bind(repo_id)
            .bind(id)
            .fetch_optional(&self.db)
            .await?)
    }

    pub async fn milestone_by_title(&self, repo_id: i64, title: &str) -> Result<Option<Milestone>> {
        Ok(sqlx::query_as(concat!(milestone_select!(), "WHERE m.repo_id = ?1 AND m.title = ?2"))
            .bind(repo_id)
            .bind(title)
            .fetch_optional(&self.db)
            .await?)
    }

    pub async fn milestone_counts(&self, repo_id: i64) -> Result<(i64, i64)> {
        Ok(sqlx::query_as(
            "SELECT COALESCE(SUM(state = 'open'), 0), COALESCE(SUM(state = 'closed'), 0)
             FROM milestones WHERE repo_id = ?1",
        )
        .bind(repo_id)
        .fetch_one(&self.db)
        .await?)
    }

    /// Creates a milestone, or updates milestone `id`. Returns its id.
    pub async fn save_milestone(
        &self,
        repo_id: i64,
        id: Option<i64>,
        title: &str,
        description: &str,
        due_on: Option<i64>,
    ) -> Result<i64> {
        validate(title, description)?;
        let result = match id {
            None => sqlx::query_as(
                "INSERT INTO milestones (repo_id, title, description, due_on, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5) RETURNING id",
            )
            .bind(repo_id)
            .bind(title.trim())
            .bind(description.trim_end())
            .bind(due_on)
            .bind(now())
            .fetch_one(&self.db)
            .await
            .map(|(id,): (i64,)| id),
            Some(id) => sqlx::query(
                "UPDATE milestones SET title = ?3, description = ?4, due_on = ?5 WHERE repo_id = ?1 AND id = ?2",
            )
            .bind(repo_id)
            .bind(id)
            .bind(title.trim())
            .bind(description.trim_end())
            .bind(due_on)
            .execute(&self.db)
            .await
            .map(|_| id),
        };
        match result {
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
                invalid(format!("A milestone named “{}” already exists.", title.trim()))
            }
            r => r.map_err(Into::into),
        }
    }

    pub async fn set_milestone_open(&self, repo_id: i64, id: i64, open: bool) -> Result<()> {
        sqlx::query("UPDATE milestones SET state = ?3, closed_at = ?4 WHERE repo_id = ?1 AND id = ?2")
            .bind(repo_id)
            .bind(id)
            .bind(if open { "open" } else { "closed" })
            .bind((!open).then(now))
            .execute(&self.db)
            .await?;
        Ok(())
    }

    /// Deletes a milestone; its issues stay, without a milestone.
    pub async fn delete_milestone(&self, repo_id: i64, id: i64) -> Result<()> {
        sqlx::query("DELETE FROM milestones WHERE repo_id = ?1 AND id = ?2")
            .bind(repo_id)
            .bind(id)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    /// The milestone of an issue.
    pub async fn issue_milestone(&self, issue: &Issue) -> Result<Option<Milestone>> {
        Ok(sqlx::query_as(concat!(
            milestone_select!(),
            "WHERE m.id = (SELECT milestone_id FROM issues WHERE id = ?1)"
        ))
        .bind(issue.id)
        .fetch_optional(&self.db)
        .await?)
    }

    /// Puts an issue in milestone `id` of its repository, or none, and
    /// records the change.
    pub async fn set_issue_milestone(&self, issue: &Issue, actor: &User, id: Option<i64>) -> Result<()> {
        let current = self.issue_milestone(issue).await?;
        let target = match id {
            Some(id) => match self.milestone(issue.repo_id, id).await? {
                Some(m) => Some(m),
                None => return invalid("That milestone doesn't exist."),
            },
            None => None,
        };
        if current.as_ref().map(|m| m.id) == target.as_ref().map(|m| m.id) {
            return Ok(());
        }
        let mut tx = self.write_tx().await?;
        sqlx::query("UPDATE issues SET milestone_id = ?2 WHERE id = ?1")
            .bind(issue.id)
            .bind(target.as_ref().map(|m| m.id))
            .execute(&mut *tx)
            .await?;
        let mut actions = Vec::new();
        if let Some(old) = current {
            insert_comment(&mut tx, issue.id, Some(actor.id), "demilestoned", &old.title).await?;
            actions.push(IssueAction::Demilestoned(old));
        }
        if let Some(new) = target {
            insert_comment(&mut tx, issue.id, Some(actor.id), "milestoned", &new.title).await?;
            actions.push(IssueAction::Milestoned(new));
        }
        tx.commit().await?;
        for action in actions {
            self.record(Activity::Issue { issue_id: issue.id, actor: Some(actor.id), action }).await;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::IssueFilter;

    #[tokio::test]
    async fn milestones() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let repo = core.create_repo(&ada, "engine", "", false).await.unwrap();
        let other = core.create_repo(&ada, "other", "", false).await.unwrap();

        assert_eq!(parse_due_date("2026-11-01").unwrap(), Some(1_793_491_200));
        assert_eq!(parse_due_date(" ").unwrap(), None);
        assert!(parse_due_date("1 Nov").is_err());
        assert!(core.save_milestone(repo.id, None, " ", "", None).await.is_err());
        let v1 = core.save_milestone(repo.id, None, "v1.0", "First", Some(1_793_491_200)).await.unwrap();
        let v2 = core.save_milestone(repo.id, None, "v2.0", "", None).await.unwrap();
        assert!(core.save_milestone(repo.id, None, "V1.0", "", None).await.is_err(), "names are unique");
        let foreign = core.save_milestone(other.id, None, "v1.0", "", None).await.unwrap();
        let titles = |ms: Vec<Milestone>| ms.into_iter().map(|m| m.title).collect::<Vec<_>>();
        assert_eq!(titles(core.milestones(repo.id, true).await.unwrap()), ["v1.0", "v2.0"], "due first");

        let a = core.create_issue(repo.id, &ada, "A", "").await.unwrap();
        let b = core.create_issue(repo.id, &ada, "B", "").await.unwrap();
        assert!(core.set_issue_milestone(&a, &ada, Some(foreign)).await.is_err());
        core.set_issue_milestone(&a, &ada, Some(v1)).await.unwrap();
        core.set_issue_milestone(&b, &ada, Some(v1)).await.unwrap();
        core.set_issue_open(&b, &ada, false).await.unwrap();
        let m = core.milestone(repo.id, v1).await.unwrap().unwrap();
        assert_eq!((m.open_items, m.closed_items, m.progress()), (1, 1, 50));
        assert!(m.is_overdue(1_793_491_200 + 86_400));
        assert!(!m.is_overdue(1_793_491_200 + 3_600));
        let filter = IssueFilter { milestone: Some(v1), ..IssueFilter::default() };
        assert_eq!(core.filtered_counts(repo.id, false, filter).await.unwrap(), (1, 1));

        core.set_issue_milestone(&a, &ada, Some(v2)).await.unwrap();
        core.set_issue_milestone(&a, &ada, Some(v2)).await.unwrap();
        let events: Vec<(String, String)> =
            core.comments(a.id).await.unwrap().into_iter().map(|c| (c.kind, c.body)).collect();
        assert_eq!(events, [
            ("milestoned".into(), "v1.0".into()),
            ("demilestoned".into(), "v1.0".into()),
            ("milestoned".into(), "v2.0".into()),
        ]);

        core.set_milestone_open(repo.id, v1, false).await.unwrap();
        assert_eq!(titles(core.milestones(repo.id, false).await.unwrap()), ["v1.0"]);
        assert_eq!(core.milestone_counts(repo.id).await.unwrap(), (1, 1));
        core.delete_milestone(repo.id, v2).await.unwrap();
        assert_eq!(core.issue_milestone(&a).await.unwrap(), None);
    }
}
