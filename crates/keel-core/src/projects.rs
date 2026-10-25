//! Project boards: a repository's issues, pull requests and notes as cards
//! in columns. Cards follow their issues: closing one moves its cards to
//! the board's done column, reopening it back to the first column.

use sqlx::SqliteConnection;

use crate::{Core, Error, Issue, Result, User, invalid, now};

const MAX_COLUMNS: i64 = 20;
const MAX_CARDS: i64 = 2500;
const MAX_NOTE: usize = 2000;

/// Columns a new board can start with: name, and whether it's the done
/// column.
pub const KANBAN: [(&str, bool); 3] = [("To do", false), ("In progress", false), ("Done", true)];

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Project {
    pub id: i64,
    pub repo_id: i64,
    pub number: i64,
    pub title: String,
    pub description: String,
    pub closed_at: Option<i64>,
    pub creator: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    /// Issue and pull request cards, by whether they're open.
    pub open_items: i64,
    pub closed_items: i64,
    pub notes: i64,
}

impl Project {
    pub fn is_open(&self) -> bool {
        self.closed_at.is_none()
    }

    /// Done, in percent.
    pub fn progress(&self) -> i64 {
        let total = self.open_items + self.closed_items;
        if total == 0 { 0 } else { self.closed_items * 100 / total }
    }
}

macro_rules! project_select {
    () => {
        "SELECT p.id, p.repo_id, p.number, p.title, p.description, p.closed_at, u.username AS creator,
            p.created_at, p.updated_at,
            (SELECT COUNT(*) FROM project_cards c JOIN issues i ON i.id = c.issue_id
             WHERE c.project_id = p.id AND i.state = 'open') AS open_items,
            (SELECT COUNT(*) FROM project_cards c JOIN issues i ON i.id = c.issue_id
             WHERE c.project_id = p.id AND i.state != 'open') AS closed_items,
            (SELECT COUNT(*) FROM project_cards c WHERE c.project_id = p.id AND c.note IS NOT NULL) AS notes
         FROM projects p LEFT JOIN users u ON u.id = p.creator_id "
    };
}

#[derive(Clone, Debug, PartialEq, Eq, sqlx::FromRow)]
pub struct ProjectColumn {
    pub id: i64,
    pub name: String,
    pub position: i64,
    /// Closed issues' cards move here.
    pub is_done: bool,
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct ProjectCard {
    pub id: i64,
    pub column_id: i64,
    pub position: i64,
    pub note: Option<String>,
    pub issue_id: Option<i64>,
    pub issue_number: Option<i64>,
    pub issue_title: Option<String>,
    /// `open`, `closed` or `merged`.
    pub issue_state: Option<String>,
    pub issue_is_pull: Option<bool>,
    pub issue_is_draft: Option<bool>,
}

/// Where an issue is on a board, for its sidebar.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct IssueCard {
    pub number: i64,
    pub title: String,
    pub column: String,
}

fn validate_project(title: &str, description: &str) -> Result<()> {
    if title.trim().is_empty() {
        return invalid("A project needs a name.");
    }
    if title.chars().count() > 100 {
        return invalid("Project names can be at most 100 characters.");
    }
    if description.len() > 10_000 {
        return invalid("That description is too long.");
    }
    Ok(())
}

fn validate_column(name: &str) -> Result<()> {
    match name.trim().chars().count() {
        0 => invalid("A column needs a name."),
        51.. => invalid("Column names can be at most 50 characters."),
        _ => Ok(()),
    }
}

/// Puts card `card` at `position` (clamped) of `column`, closing the gap
/// it leaves.
async fn place(conn: &mut SqliteConnection, card: i64, column: i64, position: i64) -> Result<()> {
    let (old_column, old_position): (i64, i64) =
        sqlx::query_as("SELECT column_id, position FROM project_cards WHERE id = ?1")
            .bind(card)
            .fetch_one(&mut *conn)
            .await?;
    sqlx::query("UPDATE project_cards SET position = position - 1 WHERE column_id = ?1 AND position > ?2")
        .bind(old_column)
        .bind(old_position)
        .execute(&mut *conn)
        .await?;
    let (others,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM project_cards WHERE column_id = ?1 AND id != ?2")
        .bind(column)
        .bind(card)
        .fetch_one(&mut *conn)
        .await?;
    let position = position.clamp(0, others);
    sqlx::query("UPDATE project_cards SET position = position + 1 WHERE column_id = ?1 AND position >= ?2 AND id != ?3")
        .bind(column)
        .bind(position)
        .bind(card)
        .execute(&mut *conn)
        .await?;
    sqlx::query("UPDATE project_cards SET column_id = ?2, position = ?3 WHERE id = ?1")
        .bind(card)
        .bind(column)
        .bind(position)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

async fn touch(conn: &mut SqliteConnection, project_id: i64) -> Result<()> {
    sqlx::query("UPDATE projects SET updated_at = ?2 WHERE id = ?1").bind(project_id).bind(now()).execute(conn).await?;
    Ok(())
}

impl Core {
    /// A repository's open or closed projects, recently changed first.
    pub async fn projects(&self, repo_id: i64, open: bool) -> Result<Vec<Project>> {
        Ok(sqlx::query_as(concat!(
            project_select!(),
            "WHERE p.repo_id = ?1 AND (p.closed_at IS NULL) = ?2 ORDER BY p.updated_at DESC, p.id DESC"
        ))
        .bind(repo_id)
        .bind(open)
        .fetch_all(&self.db)
        .await?)
    }

    /// Open and closed project counts.
    pub async fn project_counts(&self, repo_id: i64) -> Result<(i64, i64)> {
        Ok(sqlx::query_as(
            "SELECT COALESCE(SUM(closed_at IS NULL), 0), COALESCE(SUM(closed_at IS NOT NULL), 0) FROM projects WHERE repo_id = ?1",
        )
        .bind(repo_id)
        .fetch_one(&self.db)
        .await?)
    }

    pub async fn project(&self, repo_id: i64, number: i64) -> Result<Option<Project>> {
        Ok(sqlx::query_as(concat!(project_select!(), "WHERE p.repo_id = ?1 AND p.number = ?2"))
            .bind(repo_id)
            .bind(number)
            .fetch_optional(&self.db)
            .await?)
    }

    /// Creates a project, with the [`KANBAN`] columns if `kanban`.
    pub async fn create_project(&self, repo_id: i64, user: &User, title: &str, description: &str, kanban: bool) -> Result<Project> {
        validate_project(title, description)?;
        let mut tx = self.db.begin().await?;
        crate::issues::ensure_unarchived(&mut *tx, repo_id).await?;
        let now = now();
        let (id, number): (i64, i64) = sqlx::query_as(
            "INSERT INTO projects (repo_id, number, title, description, creator_id, created_at, updated_at)
             VALUES (?1, (SELECT COALESCE(MAX(number), 0) + 1 FROM projects WHERE repo_id = ?1), ?2, ?3, ?4, ?5, ?5)
             RETURNING id, number",
        )
        .bind(repo_id)
        .bind(title.trim())
        .bind(description.trim_end())
        .bind(user.id)
        .bind(now)
        .fetch_one(&mut *tx)
        .await?;
        if kanban {
            for (position, (name, done)) in KANBAN.into_iter().enumerate() {
                sqlx::query("INSERT INTO project_columns (project_id, name, position, is_done) VALUES (?1, ?2, ?3, ?4)")
                    .bind(id)
                    .bind(name)
                    .bind(position as i64)
                    .bind(done)
                    .execute(&mut *tx)
                    .await?;
            }
        }
        tx.commit().await?;
        self.project(repo_id, number).await?.ok_or(Error::NotFound)
    }

    pub async fn edit_project(&self, project: &Project, title: &str, description: &str) -> Result<()> {
        validate_project(title, description)?;
        sqlx::query("UPDATE projects SET title = ?2, description = ?3, updated_at = ?4 WHERE id = ?1")
            .bind(project.id)
            .bind(title.trim())
            .bind(description.trim_end())
            .bind(now())
            .execute(&self.db)
            .await?;
        Ok(())
    }

    pub async fn set_project_open(&self, project: &Project, open: bool) -> Result<()> {
        sqlx::query("UPDATE projects SET closed_at = CASE WHEN ?2 THEN NULL ELSE COALESCE(closed_at, ?3) END, updated_at = ?3 WHERE id = ?1")
            .bind(project.id)
            .bind(open)
            .bind(now())
            .execute(&self.db)
            .await?;
        Ok(())
    }

    pub async fn delete_project(&self, project: &Project) -> Result<()> {
        sqlx::query("DELETE FROM projects WHERE id = ?1").bind(project.id).execute(&self.db).await?;
        Ok(())
    }

    pub async fn project_columns(&self, project_id: i64) -> Result<Vec<ProjectColumn>> {
        Ok(sqlx::query_as("SELECT id, name, position, is_done FROM project_columns WHERE project_id = ?1 ORDER BY position, id")
            .bind(project_id)
            .fetch_all(&self.db)
            .await?)
    }

    /// The board's cards, by column and position.
    pub async fn project_cards(&self, project_id: i64) -> Result<Vec<ProjectCard>> {
        Ok(sqlx::query_as(
            "SELECT c.id, c.column_id, c.position, c.note, c.issue_id, i.number AS issue_number, i.title AS issue_title,
                i.state AS issue_state, i.is_pull AS issue_is_pull, pr.is_draft AS issue_is_draft
             FROM project_cards c LEFT JOIN issues i ON i.id = c.issue_id LEFT JOIN pull_requests pr ON pr.issue_id = c.issue_id
             WHERE c.project_id = ?1 ORDER BY c.column_id, c.position, c.id",
        )
        .bind(project_id)
        .fetch_all(&self.db)
        .await?)
    }

    /// A column of `project`, or `NotFound`.
    async fn column_of(&self, project: &Project, column_id: i64) -> Result<ProjectColumn> {
        sqlx::query_as("SELECT id, name, position, is_done FROM project_columns WHERE id = ?1 AND project_id = ?2")
            .bind(column_id)
            .bind(project.id)
            .fetch_optional(&self.db)
            .await?
            .ok_or(Error::NotFound)
    }

    pub async fn add_project_column(&self, project: &Project, name: &str) -> Result<i64> {
        validate_column(name)?;
        let mut tx = self.db.begin().await?;
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM project_columns WHERE project_id = ?1")
            .bind(project.id)
            .fetch_one(&mut *tx)
            .await?;
        if count >= MAX_COLUMNS {
            return invalid(format!("A board can have at most {MAX_COLUMNS} columns."));
        }
        let (id,): (i64,) =
            sqlx::query_as("INSERT INTO project_columns (project_id, name, position) VALUES (?1, ?2, ?3) RETURNING id")
                .bind(project.id)
                .bind(name.trim())
                .bind(count)
                .fetch_one(&mut *tx)
                .await?;
        touch(&mut tx, project.id).await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Renames a column, and makes it the done column (or not). A board
    /// has at most one.
    pub async fn edit_project_column(&self, project: &Project, column_id: i64, name: &str, done: bool) -> Result<()> {
        validate_column(name)?;
        self.column_of(project, column_id).await?;
        let mut tx = self.db.begin().await?;
        if done {
            sqlx::query("UPDATE project_columns SET is_done = 0 WHERE project_id = ?1").bind(project.id).execute(&mut *tx).await?;
        }
        sqlx::query("UPDATE project_columns SET name = ?2, is_done = ?3 WHERE id = ?1")
            .bind(column_id)
            .bind(name.trim())
            .bind(done)
            .execute(&mut *tx)
            .await?;
        touch(&mut tx, project.id).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Moves a column to `position` (clamped) among the board's columns.
    pub async fn move_project_column(&self, project: &Project, column_id: i64, position: i64) -> Result<()> {
        let column = self.column_of(project, column_id).await?;
        let mut tx = self.db.begin().await?;
        let mut ids: Vec<(i64,)> = sqlx::query_as("SELECT id FROM project_columns WHERE project_id = ?1 AND id != ?2 ORDER BY position, id")
            .bind(project.id)
            .bind(column.id)
            .fetch_all(&mut *tx)
            .await?;
        let position = position.clamp(0, ids.len() as i64) as usize;
        ids.insert(position, (column.id,));
        for (i, (id,)) in ids.into_iter().enumerate() {
            sqlx::query("UPDATE project_columns SET position = ?2 WHERE id = ?1").bind(id).bind(i as i64).execute(&mut *tx).await?;
        }
        touch(&mut tx, project.id).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Deletes a column with its cards.
    pub async fn delete_project_column(&self, project: &Project, column_id: i64) -> Result<()> {
        let column = self.column_of(project, column_id).await?;
        let mut tx = self.db.begin().await?;
        sqlx::query("DELETE FROM project_columns WHERE id = ?1").bind(column.id).execute(&mut *tx).await?;
        sqlx::query("UPDATE project_columns SET position = position - 1 WHERE project_id = ?1 AND position > ?2")
            .bind(project.id)
            .bind(column.position)
            .execute(&mut *tx)
            .await?;
        touch(&mut tx, project.id).await?;
        tx.commit().await?;
        Ok(())
    }

    async fn insert_card(&self, project: &Project, column_id: i64, issue_id: Option<i64>, note: Option<&str>, user: &User) -> Result<i64> {
        self.column_of(project, column_id).await?;
        let mut tx = self.db.begin().await?;
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM project_cards WHERE project_id = ?1")
            .bind(project.id)
            .fetch_one(&mut *tx)
            .await?;
        if count >= MAX_CARDS {
            return invalid(format!("A board can have at most {MAX_CARDS} cards."));
        }
        let inserted = sqlx::query_as::<_, (i64,)>(
            "INSERT INTO project_cards (project_id, column_id, position, issue_id, note, creator_id, created_at)
             VALUES (?1, ?2, (SELECT COUNT(*) FROM project_cards WHERE column_id = ?2), ?3, ?4, ?5, ?6) RETURNING id",
        )
        .bind(project.id)
        .bind(column_id)
        .bind(issue_id)
        .bind(note)
        .bind(user.id)
        .bind(now())
        .fetch_one(&mut *tx)
        .await;
        let (id,) = match inserted {
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => return invalid("That’s already on this board."),
            r => r?,
        };
        touch(&mut tx, project.id).await?;
        tx.commit().await?;
        Ok(id)
    }

    /// Adds an issue or pull request of the project's repository to the
    /// bottom of a column.
    pub async fn add_issue_card(&self, project: &Project, column_id: i64, issue: &Issue, user: &User) -> Result<i64> {
        if issue.repo_id != project.repo_id {
            return Err(Error::NotFound);
        }
        self.insert_card(project, column_id, Some(issue.id), None, user).await
    }

    /// Adds a note to the bottom of a column.
    pub async fn add_note_card(&self, project: &Project, column_id: i64, note: &str, user: &User) -> Result<i64> {
        let note = note.trim();
        if note.is_empty() {
            return invalid("Write a note, or #number for an issue or pull request.");
        }
        if note.len() > MAX_NOTE {
            return invalid(format!("Notes can be at most {MAX_NOTE} characters."));
        }
        self.insert_card(project, column_id, None, Some(note), user).await
    }

    /// A card of `project`: its column and position.
    async fn card_of(&self, project: &Project, card_id: i64) -> Result<(i64, i64)> {
        sqlx::query_as("SELECT column_id, position FROM project_cards WHERE id = ?1 AND project_id = ?2")
            .bind(card_id)
            .bind(project.id)
            .fetch_optional(&self.db)
            .await?
            .ok_or(Error::NotFound)
    }

    /// Moves a card to `position` (clamped) of a column.
    pub async fn move_card(&self, project: &Project, card_id: i64, column_id: i64, position: i64) -> Result<()> {
        self.card_of(project, card_id).await?;
        self.column_of(project, column_id).await?;
        let mut tx = self.db.begin().await?;
        place(&mut tx, card_id, column_id, position).await?;
        touch(&mut tx, project.id).await?;
        tx.commit().await?;
        Ok(())
    }

    pub async fn edit_note_card(&self, project: &Project, card_id: i64, note: &str) -> Result<()> {
        self.card_of(project, card_id).await?;
        let note = note.trim();
        if note.is_empty() || note.len() > MAX_NOTE {
            return invalid(format!("Notes need 1 to {MAX_NOTE} characters."));
        }
        sqlx::query("UPDATE project_cards SET note = ?2 WHERE id = ?1 AND note IS NOT NULL")
            .bind(card_id)
            .bind(note)
            .execute(&self.db)
            .await?;
        Ok(())
    }

    pub async fn delete_card(&self, project: &Project, card_id: i64) -> Result<()> {
        let (column, position) = self.card_of(project, card_id).await?;
        let mut tx = self.db.begin().await?;
        sqlx::query("DELETE FROM project_cards WHERE id = ?1").bind(card_id).execute(&mut *tx).await?;
        sqlx::query("UPDATE project_cards SET position = position - 1 WHERE column_id = ?1 AND position > ?2")
            .bind(column)
            .bind(position)
            .execute(&mut *tx)
            .await?;
        touch(&mut tx, project.id).await?;
        tx.commit().await?;
        Ok(())
    }

    /// The open boards an issue is on, and in which column.
    pub async fn issue_cards(&self, issue_id: i64) -> Result<Vec<IssueCard>> {
        Ok(sqlx::query_as(
            "SELECT p.number, p.title, col.name AS column FROM project_cards c
             JOIN projects p ON p.id = c.project_id JOIN project_columns col ON col.id = c.column_id
             WHERE c.issue_id = ?1 AND p.closed_at IS NULL ORDER BY p.number",
        )
        .bind(issue_id)
        .fetch_all(&self.db)
        .await?)
    }

    /// After an issue was closed (or merged) or reopened: moves its cards
    /// to their board's done column, or out of it to the first column.
    pub(crate) async fn move_cards_of(&self, issue_id: i64, open: bool) -> Result<()> {
        let moves: Vec<(i64, i64, i64)> = if open {
            sqlx::query_as(
                "SELECT c.id, c.project_id, (SELECT f.id FROM project_columns f WHERE f.project_id = c.project_id
                                             ORDER BY f.position, f.id LIMIT 1)
                 FROM project_cards c JOIN project_columns col ON col.id = c.column_id
                 WHERE c.issue_id = ?1 AND col.is_done",
            )
        } else {
            sqlx::query_as(
                "SELECT c.id, c.project_id, d.id FROM project_cards c
                 JOIN project_columns d ON d.project_id = c.project_id AND d.is_done
                 WHERE c.issue_id = ?1 AND c.column_id != d.id",
            )
        }
        .bind(issue_id)
        .fetch_all(&self.db)
        .await?;
        for (card, project, column) in moves {
            let mut tx = self.db.begin().await?;
            place(&mut tx, card, column, 0).await?;
            touch(&mut tx, project).await?;
            tx.commit().await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn boards() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let repo = core.create_repo(&ada, "engine", "", false).await.unwrap();
        let other = core.create_repo(&ada, "other", "", false).await.unwrap();
        let bug = core.create_issue(repo.id, &ada, "Bug", "").await.unwrap();
        let feature = core.create_issue(repo.id, &ada, "Feature", "").await.unwrap();
        let elsewhere = core.create_issue(other.id, &ada, "Elsewhere", "").await.unwrap();

        assert!(core.create_project(repo.id, &ada, " ", "", true).await.is_err());
        let p = core.create_project(repo.id, &ada, "Roadmap", "", true).await.unwrap();
        assert_eq!(p.number, 1);
        let cols = core.project_columns(p.id).await.unwrap();
        let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["To do", "In progress", "Done"]);
        let (todo, doing, done) = (cols[0].id, cols[1].id, cols[2].id);

        let a = core.add_issue_card(&p, todo, &bug, &ada).await.unwrap();
        let b = core.add_issue_card(&p, todo, &feature, &ada).await.unwrap();
        let n = core.add_note_card(&p, todo, "Write docs", &ada).await.unwrap();
        assert!(core.add_issue_card(&p, todo, &bug, &ada).await.is_err(), "once per board");
        assert!(core.add_issue_card(&p, todo, &elsewhere, &ada).await.is_err(), "same repository only");
        let order = |cards: &[ProjectCard], col: i64| -> Vec<i64> {
            cards.iter().filter(|c| c.column_id == col).map(|c| c.id).collect()
        };
        assert_eq!(order(&core.project_cards(p.id).await.unwrap(), todo), [a, b, n]);

        core.move_card(&p, n, todo, 0).await.unwrap();
        core.move_card(&p, a, doing, 5).await.unwrap();
        let cards = core.project_cards(p.id).await.unwrap();
        assert_eq!((order(&cards, todo), order(&cards, doing)), (vec![n, b], vec![a]));
        assert!(cards.iter().all(|c| c.position < 2), "no gaps");

        // Closing moves to Done, reopening back to To do.
        core.set_issue_open(&bug, &ada, false).await.unwrap();
        let cards = core.project_cards(p.id).await.unwrap();
        assert_eq!(order(&cards, done), [a]);
        let p = core.project(repo.id, 1).await.unwrap().unwrap();
        assert_eq!((p.open_items, p.closed_items, p.notes, p.progress()), (1, 1, 1, 50));
        let bug = core.issue(repo.id, bug.number).await.unwrap().unwrap();
        core.set_issue_open(&bug, &ada, true).await.unwrap();
        assert_eq!(order(&core.project_cards(p.id).await.unwrap(), todo), [a, n, b]);
        let on = core.issue_cards(bug.id).await.unwrap();
        assert_eq!((on.len(), on[0].column.as_str()), (1, "To do"));

        // Columns.
        core.move_project_column(&p, done, 0).await.unwrap();
        let names: Vec<String> = core.project_columns(p.id).await.unwrap().into_iter().map(|c| c.name).collect();
        assert_eq!(names, ["Done", "To do", "In progress"]);
        core.edit_project_column(&p, doing, "Doing", true).await.unwrap();
        let cols = core.project_columns(p.id).await.unwrap();
        assert_eq!(cols.iter().filter(|c| c.is_done).map(|c| c.id).collect::<Vec<_>>(), [doing], "one done column");
        core.delete_project_column(&p, todo).await.unwrap();
        let cols = core.project_columns(p.id).await.unwrap();
        assert_eq!(cols.iter().map(|c| c.position).collect::<Vec<_>>(), [0, 1]);
        assert_eq!(core.project_cards(p.id).await.unwrap().len(), 0, "its cards went with it");

        core.set_project_open(&p, false).await.unwrap();
        assert_eq!(core.project_counts(repo.id).await.unwrap(), (0, 1));
        assert!(core.issue_cards(bug.id).await.unwrap().is_empty(), "closed boards aren't listed");
        core.delete_project(&p).await.unwrap();
        assert!(core.project(repo.id, 1).await.unwrap().is_none());
    }
}
