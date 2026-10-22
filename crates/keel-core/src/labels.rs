//! Labels and assignees of issues and pull requests.

use std::collections::HashMap;

use sqlx::SqliteConnection;

use crate::issues::insert_comment;
use crate::{Core, Issue, Repo, Result, User, invalid, now};

/// The colors a label can have. A fixed palette (styled in CSS) keeps
/// labels readable in light and dark mode.
pub const LABEL_COLORS: &[&str] = &[
    "red", "orange", "yellow", "green", "teal", "blue", "purple", "pink", "brown", "gray",
];

const DEFAULT_LABELS: &[(&str, &str, &str)] = &[
    ("bug", "red", "Something isn't working"),
    ("documentation", "blue", "Improvements or additions to documentation"),
    ("enhancement", "teal", "New feature or request"),
    ("good first issue", "purple", "Good for newcomers"),
    ("help wanted", "green", "Extra attention is needed"),
    ("question", "pink", "Further information is requested"),
];

const MAX_ASSIGNEES: usize = 10;

#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct Label {
    pub id: i64,
    pub repo_id: i64,
    pub name: String,
    pub color: String,
    pub description: String,
}

pub(crate) async fn insert_default_labels(conn: &mut SqliteConnection, repo_id: i64) -> Result<()> {
    for (name, color, description) in DEFAULT_LABELS {
        sqlx::query("INSERT INTO labels (repo_id, name, color, description, created_at) VALUES (?1, ?2, ?3, ?4, ?5)")
            .bind(repo_id)
            .bind(name)
            .bind(color)
            .bind(description)
            .bind(now())
            .execute(&mut *conn)
            .await?;
    }
    Ok(())
}

fn validate_label(name: &str, color: &str, description: &str) -> Result<()> {
    if name.trim().is_empty() || name.trim().chars().count() > 50 {
        return invalid("Label names are 1–50 characters.");
    }
    if name.contains(',') {
        return invalid("Label names can't contain commas.");
    }
    if !LABEL_COLORS.contains(&color) {
        return invalid("Choose one of the colors.");
    }
    if description.chars().count() > 200 {
        return invalid("Descriptions are at most 200 characters.");
    }
    Ok(())
}

impl Core {
    pub async fn labels(&self, repo_id: i64) -> Result<Vec<Label>> {
        Ok(sqlx::query_as(
            "SELECT id, repo_id, name, color, description FROM labels WHERE repo_id = ?1 ORDER BY name",
        )
        .bind(repo_id)
        .fetch_all(&self.db)
        .await?)
    }

    pub async fn label_by_name(&self, repo_id: i64, name: &str) -> Result<Option<Label>> {
        Ok(sqlx::query_as(
            "SELECT id, repo_id, name, color, description FROM labels WHERE repo_id = ?1 AND name = ?2",
        )
        .bind(repo_id)
        .bind(name)
        .fetch_optional(&self.db)
        .await?)
    }

    /// Creates a label, or updates label `id`.
    pub async fn save_label(
        &self,
        repo_id: i64,
        id: Option<i64>,
        name: &str,
        color: &str,
        description: &str,
    ) -> Result<()> {
        validate_label(name, color, description)?;
        let result = match id {
            None => {
                sqlx::query(
                    "INSERT INTO labels (repo_id, name, color, description, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                )
                .bind(repo_id)
                .bind(name.trim())
                .bind(color)
                .bind(description.trim())
                .bind(now())
                .execute(&self.db)
                .await
            }
            Some(id) => {
                sqlx::query("UPDATE labels SET name = ?3, color = ?4, description = ?5 WHERE id = ?2 AND repo_id = ?1")
                    .bind(repo_id)
                    .bind(id)
                    .bind(name.trim())
                    .bind(color)
                    .bind(description.trim())
                    .execute(&self.db)
                    .await
            }
        };
        match result {
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
                invalid(format!("A label named “{}” already exists.", name.trim()))
            }
            r => r.map(|_| ()).map_err(Into::into),
        }
    }

    pub async fn delete_label(&self, repo_id: i64, id: i64) -> Result<()> {
        sqlx::query("DELETE FROM labels WHERE id = ?2 AND repo_id = ?1")
            .bind(repo_id)
            .bind(id)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    /// How many open issues and pull requests carry each label, by label id.
    pub async fn label_counts(&self, repo_id: i64) -> Result<HashMap<i64, i64>> {
        let rows: Vec<(i64, i64)> = sqlx::query_as(
            "SELECT il.label_id, COUNT(*) FROM issue_labels il
             JOIN labels l ON l.id = il.label_id JOIN issues i ON i.id = il.issue_id
             WHERE l.repo_id = ?1 AND i.state = 'open' GROUP BY il.label_id",
        )
        .bind(repo_id)
        .fetch_all(&self.db)
        .await?;
        Ok(rows.into_iter().collect())
    }

    /// Labels of each of `issues`, by issue id, sorted by name.
    pub async fn labels_of(&self, issues: &[i64]) -> Result<HashMap<i64, Vec<Label>>> {
        let mut out: HashMap<i64, Vec<Label>> = HashMap::new();
        if issues.is_empty() {
            return Ok(out);
        }
        // One query for the whole page via a JSON array parameter.
        let ids = serde_json::to_string(issues).map_err(anyhow::Error::from)?;
        let rows: Vec<(i64, i64, i64, String, String, String)> = sqlx::query_as(
            "SELECT il.issue_id, l.id, l.repo_id, l.name, l.color, l.description
             FROM issue_labels il JOIN labels l ON l.id = il.label_id
             WHERE il.issue_id IN (SELECT value FROM json_each(?1))
             ORDER BY l.name",
        )
        .bind(ids)
        .fetch_all(&self.db)
        .await?;
        for (issue_id, id, repo_id, name, color, description) in rows {
            out.entry(issue_id)
                .or_default()
                .push(Label { id, repo_id, name, color, description });
        }
        Ok(out)
    }

    /// Sets the labels of an issue to `label_ids` (ignoring ids of other
    /// repositories) and records what changed in the timeline.
    pub async fn set_issue_labels(&self, issue: &Issue, actor: &User, label_ids: &[i64]) -> Result<()> {
        let available = self.labels(issue.repo_id).await?;
        let current: Vec<i64> = self
            .labels_of(&[issue.id])
            .await?
            .remove(&issue.id)
            .unwrap_or_default()
            .into_iter()
            .map(|l| l.id)
            .collect();
        let mut tx = self.db.begin().await?;
        for label in &available {
            let (was, is) = (current.contains(&label.id), label_ids.contains(&label.id));
            if was == is {
                continue;
            }
            let sql = if is {
                "INSERT INTO issue_labels (issue_id, label_id) VALUES (?1, ?2)"
            } else {
                "DELETE FROM issue_labels WHERE issue_id = ?1 AND label_id = ?2"
            };
            sqlx::query(sql).bind(issue.id).bind(label.id).execute(&mut *tx).await?;
            let kind = if is { "labeled" } else { "unlabeled" };
            insert_comment(&mut tx, issue.id, Some(actor.id), kind, &label.name).await?;
        }
        tx.commit().await?;
        Ok(())
    }

    // -- Assignees ----------------------------------------------------------

    /// People who can be assigned: everyone with write access.
    pub async fn assignable_users(&self, repo: &Repo) -> Result<Vec<User>> {
        Ok(sqlx::query_as(concat!(
            "SELECT ",
            user_columns!(),
            " FROM users WHERE kind = 'user' AND (
                id = ?2
                OR id IN (SELECT user_id FROM collaborators WHERE repo_id = ?1 AND role != 'read')
                OR id IN (SELECT user_id FROM org_members WHERE org_id = ?2))
             ORDER BY username"
        ))
        .bind(repo.id)
        .bind(repo.owner_id)
        .fetch_all(&self.db)
        .await?)
    }

    /// Usernames assigned to each of `issues`, by issue id.
    pub async fn assignees_of(&self, issues: &[i64]) -> Result<HashMap<i64, Vec<String>>> {
        let mut out: HashMap<i64, Vec<String>> = HashMap::new();
        if issues.is_empty() {
            return Ok(out);
        }
        let ids = serde_json::to_string(issues).map_err(anyhow::Error::from)?;
        let rows: Vec<(i64, String)> = sqlx::query_as(
            "SELECT a.issue_id, u.username FROM issue_assignees a JOIN users u ON u.id = a.user_id
             WHERE a.issue_id IN (SELECT value FROM json_each(?1)) ORDER BY u.username",
        )
        .bind(ids)
        .fetch_all(&self.db)
        .await?;
        for (issue_id, name) in rows {
            out.entry(issue_id).or_default().push(name);
        }
        Ok(out)
    }

    /// Sets who is assigned to an issue. Only people with write access can
    /// be assigned; others in `user_ids` are ignored.
    pub async fn set_assignees(&self, repo: &Repo, issue: &Issue, actor: &User, user_ids: &[i64]) -> Result<()> {
        let candidates = self.assignable_users(repo).await?;
        if user_ids.len() > MAX_ASSIGNEES {
            return invalid(format!("At most {MAX_ASSIGNEES} people can be assigned."));
        }
        let current = self.assignees_of(&[issue.id]).await?.remove(&issue.id).unwrap_or_default();
        let mut tx = self.db.begin().await?;
        for user in &candidates {
            let (was, is) = (current.contains(&user.username), user_ids.contains(&user.id));
            if was == is {
                continue;
            }
            let sql = if is {
                "INSERT INTO issue_assignees (issue_id, user_id) VALUES (?1, ?2)"
            } else {
                "DELETE FROM issue_assignees WHERE issue_id = ?1 AND user_id = ?2"
            };
            sqlx::query(sql).bind(issue.id).bind(user.id).execute(&mut *tx).await?;
            let kind = if is { "assigned" } else { "unassigned" };
            insert_comment(&mut tx, issue.id, Some(actor.id), kind, &user.username).await?;
        }
        tx.commit().await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn labels_and_assignees() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let bob = core.register("bob", "bob@example.com", "password1").await.unwrap();
        let eve = core.register("eve", "eve@example.com", "password1").await.unwrap();
        let repo = core.create_repo(&ada, "engine", "", false).await.unwrap();
        let other = core.create_repo(&ada, "other", "", false).await.unwrap();

        // Labels.
        assert!(core.save_label(repo.id, None, "Bug", "red", "").await.is_err(), "exists");
        assert!(core.save_label(repo.id, None, "x", "chartreuse", "").await.is_err());
        assert!(core.save_label(repo.id, None, "a,b", "red", "").await.is_err());
        core.save_label(repo.id, None, "perf", "orange", "Speed").await.unwrap();
        let labels = core.labels(repo.id).await.unwrap();
        assert_eq!(labels.len(), 7);
        let bug = core.label_by_name(repo.id, "BUG").await.unwrap().unwrap();
        let perf = core.label_by_name(repo.id, "perf").await.unwrap().unwrap();
        core.save_label(repo.id, Some(perf.id), "performance", "yellow", "").await.unwrap();
        let foreign = core.labels(other.id).await.unwrap()[0].id;

        let issue = core.create_issue(repo.id, &bob, "Slow", "").await.unwrap();
        core.set_issue_labels(&issue, &ada, &[bug.id, perf.id, foreign]).await.unwrap();
        let names = |m: HashMap<i64, Vec<Label>>| m[&issue.id].iter().map(|l| l.name.clone()).collect::<Vec<_>>();
        assert_eq!(names(core.labels_of(&[issue.id]).await.unwrap()), ["bug", "performance"]);
        assert_eq!(core.label_counts(repo.id).await.unwrap()[&bug.id], 1);
        core.set_issue_labels(&issue, &ada, &[perf.id]).await.unwrap();
        assert_eq!(names(core.labels_of(&[issue.id]).await.unwrap()), ["performance"]);
        core.delete_label(repo.id, perf.id).await.unwrap();
        assert!(core.labels_of(&[issue.id]).await.unwrap().is_empty());

        // Assignees: only people with write access.
        core.set_collaborator(&repo, "bob", crate::Access::Write).await.unwrap();
        core.set_collaborator(&repo, "eve", crate::Access::Read).await.unwrap();
        let names: Vec<String> = core.assignable_users(&repo).await.unwrap().into_iter().map(|u| u.username).collect();
        assert_eq!(names, ["ada", "bob"]);
        core.set_assignees(&repo, &issue, &ada, &[bob.id, eve.id]).await.unwrap();
        assert_eq!(core.assignees_of(&[issue.id]).await.unwrap()[&issue.id], ["bob"]);
        core.set_assignees(&repo, &issue, &ada, &[ada.id]).await.unwrap();
        assert_eq!(core.assignees_of(&[issue.id]).await.unwrap()[&issue.id], ["ada"]);

        let events: Vec<(String, String)> = core
            .comments(issue.id)
            .await
            .unwrap()
            .into_iter()
            .map(|c| (c.kind, c.body))
            .collect();
        assert_eq!(events, [
            ("labeled".into(), "bug".into()),
            ("labeled".into(), "performance".into()),
            ("unlabeled".into(), "bug".into()),
            ("assigned".into(), "bob".into()),
            ("assigned".into(), "ada".into()),
            ("unassigned".into(), "bob".into()),
        ]);
        assert_eq!(core.issue(repo.id, 1).await.unwrap().unwrap().comment_count, 0, "events aren't comments");
    }
}
