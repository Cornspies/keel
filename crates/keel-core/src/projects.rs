//! Project boards: issues, pull requests and notes as cards in columns.
//! A board belongs to a repository (its issues), or to an organization
//! (issues of any of its repositories). Cards follow their issues: closing
//! one moves its cards to the board's done column, reopening it back to the
//! first column. Changes show in the issues' timelines and go to webhooks.

use serde_json::{Value, json};
use sqlx::SqliteConnection;

use crate::activity::time;
use crate::issues::insert_comment;
use crate::{Core, Error, Issue, Repo, Result, User, invalid, now};

const MAX_COLUMNS: i64 = 20;
const MAX_CARDS: i64 = 2500;
const MAX_NOTE: usize = 2000;

/// Columns a new board can start with: name, and whether it's the done
/// column.
pub const KANBAN: [(&str, bool); 3] = [("To do", false), ("In progress", false), ("Done", true)];

/// Whose a board is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProjectOwner {
    Repo(i64),
    /// An organization's (its user id).
    Org(i64),
}

impl ProjectOwner {
    /// For queries matching `p.repo_id = ?1 OR p.owner_id = ?2`; ids are
    /// never negative.
    fn ids(self) -> (i64, i64) {
        match self {
            Self::Repo(id) => (id, -1),
            Self::Org(id) => (-1, id),
        }
    }
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Project {
    pub id: i64,
    pub repo_id: Option<i64>,
    /// For organization boards.
    pub owner_id: Option<i64>,
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

    pub fn owner(&self) -> ProjectOwner {
        match (self.repo_id, self.owner_id) {
            (Some(repo), _) => ProjectOwner::Repo(repo),
            (None, org) => ProjectOwner::Org(org.unwrap_or_default()),
        }
    }

    /// Done, in percent.
    pub fn progress(&self) -> i64 {
        let total = self.open_items + self.closed_items;
        if total == 0 { 0 } else { self.closed_items * 100 / total }
    }
}

macro_rules! project_select {
    () => {
        "SELECT p.id, p.repo_id, p.owner_id, p.number, p.title, p.description, p.closed_at, u.username AS creator,
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
    /// The issue's repository: on organization boards, any of its own.
    pub issue_repo_id: Option<i64>,
    pub issue_owner: Option<String>,
    pub issue_repo: Option<String>,
}

/// Where an issue is on a board, for its sidebar.
#[derive(Clone, Debug, sqlx::FromRow)]
pub struct IssueCard {
    pub number: i64,
    pub title: String,
    pub column: String,
    /// For organization boards: the organization.
    pub org_id: Option<i64>,
    pub org: Option<String>,
}

/// A card as it is (or was, before deleting it), for timelines and
/// webhooks.
#[derive(Clone, Debug, sqlx::FromRow)]
struct CardRow {
    id: i64,
    column_id: i64,
    column_name: String,
    note: Option<String>,
    issue_id: Option<i64>,
    issue_number: Option<i64>,
    issue_owner: Option<String>,
    issue_repo: Option<String>,
    creator: Option<String>,
    created_at: i64,
}

/// What changed on a board.
enum Change<'a> {
    /// `created`, `edited`, `closed`, `reopened` or `deleted`.
    Project(&'static str),
    /// `created`, `edited`, `moved` or `deleted`.
    Column(&'static str, &'a ProjectColumn),
    Card(&'a CardRow, CardChange),
}

enum CardChange {
    Created,
    /// From this column (maybe the same one).
    Moved(ProjectColumn),
    /// The note it had.
    Edited(String),
    Deleted,
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
    /// A repository's boards are read-only once it's archived, like the
    /// rest of it (organizations' boards stay open: their cards only point
    /// to archived repositories' issues).
    async fn ensure_board_writable(&self, project: &Project) -> Result<()> {
        match project.owner() {
            ProjectOwner::Repo(repo) => crate::issues::ensure_unarchived(&self.db, repo).await,
            ProjectOwner::Org(_) => Ok(()),
        }
    }

    /// What `viewer` may do with an organization's boards: they're for its
    /// members (and site admins), who can read all its repositories.
    pub async fn org_board_access(&self, org: &User, viewer: Option<&User>) -> Result<crate::Access> {
        let Some(viewer) = viewer else { return Ok(crate::Access::None) };
        let member = viewer.is_admin || self.org_role(org.id, viewer.id).await?.is_some();
        Ok(if member { crate::Access::Write } else { crate::Access::None })
    }

    /// A repository's or organization's open or closed projects, recently
    /// changed first.
    pub async fn projects(&self, owner: ProjectOwner, open: bool) -> Result<Vec<Project>> {
        let (repo, org) = owner.ids();
        Ok(sqlx::query_as(concat!(
            project_select!(),
            "WHERE (p.repo_id = ?1 OR p.owner_id = ?2) AND (p.closed_at IS NULL) = ?3
             ORDER BY p.updated_at DESC, p.id DESC"
        ))
        .bind(repo)
        .bind(org)
        .bind(open)
        .fetch_all(&self.db)
        .await?)
    }

    /// Open and closed project counts.
    pub async fn project_counts(&self, owner: ProjectOwner) -> Result<(i64, i64)> {
        let (repo, org) = owner.ids();
        Ok(sqlx::query_as(
            "SELECT COALESCE(SUM(closed_at IS NULL), 0), COALESCE(SUM(closed_at IS NOT NULL), 0) FROM projects
             WHERE repo_id = ?1 OR owner_id = ?2",
        )
        .bind(repo)
        .bind(org)
        .fetch_one(&self.db)
        .await?)
    }

    pub async fn project(&self, owner: ProjectOwner, number: i64) -> Result<Option<Project>> {
        let (repo, org) = owner.ids();
        Ok(sqlx::query_as(concat!(project_select!(), "WHERE (p.repo_id = ?1 OR p.owner_id = ?2) AND p.number = ?3"))
            .bind(repo)
            .bind(org)
            .bind(number)
            .fetch_optional(&self.db)
            .await?)
    }

    /// Creates a project, with the [`KANBAN`] columns if `kanban`.
    pub async fn create_project(&self, owner: ProjectOwner, user: &User, title: &str, description: &str, kanban: bool) -> Result<Project> {
        validate_project(title, description)?;
        let (repo, org) = owner.ids();
        let mut tx = self.write_tx().await?;
        if let ProjectOwner::Repo(repo) = owner {
            crate::issues::ensure_unarchived(&mut *tx, repo).await?;
        }
        let now = now();
        let (id, number): (i64, i64) = sqlx::query_as(
            "INSERT INTO projects (repo_id, owner_id, number, title, description, creator_id, created_at, updated_at)
             VALUES (NULLIF(?1, -1), NULLIF(?2, -1),
                     (SELECT COALESCE(MAX(number), 0) + 1 FROM projects WHERE repo_id = ?1 OR owner_id = ?2),
                     ?3, ?4, ?5, ?6, ?6)
             RETURNING id, number",
        )
        .bind(repo)
        .bind(org)
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
        let project = self.project(owner, number).await?.ok_or(Error::NotFound)?;
        self.changed(&project, Change::Project("created"), Some(user.id)).await;
        Ok(project)
    }

    pub async fn edit_project(&self, project: &Project, title: &str, description: &str, user: &User) -> Result<()> {
        self.ensure_board_writable(project).await?;
        validate_project(title, description)?;
        sqlx::query("UPDATE projects SET title = ?2, description = ?3, updated_at = ?4 WHERE id = ?1")
            .bind(project.id)
            .bind(title.trim())
            .bind(description.trim_end())
            .bind(now())
            .execute(&self.db)
            .await?;
        let edited = Project { title: title.trim().to_owned(), description: description.trim_end().to_owned(), ..project.clone() };
        self.changed(&edited, Change::Project("edited"), Some(user.id)).await;
        Ok(())
    }

    pub async fn set_project_open(&self, project: &Project, open: bool, user: &User) -> Result<()> {
        self.ensure_board_writable(project).await?;
        sqlx::query("UPDATE projects SET closed_at = CASE WHEN ?2 THEN NULL ELSE COALESCE(closed_at, ?3) END, updated_at = ?3 WHERE id = ?1")
            .bind(project.id)
            .bind(open)
            .bind(now())
            .execute(&self.db)
            .await?;
        if project.is_open() != open {
            let closed_at = (!open).then(now);
            let changed = Project { closed_at, ..project.clone() };
            self.changed(&changed, Change::Project(if open { "reopened" } else { "closed" }), Some(user.id)).await;
        }
        Ok(())
    }

    pub async fn delete_project(&self, project: &Project, user: &User) -> Result<()> {
        self.ensure_board_writable(project).await?;
        sqlx::query("DELETE FROM projects WHERE id = ?1").bind(project.id).execute(&self.db).await?;
        self.changed(project, Change::Project("deleted"), Some(user.id)).await;
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
                i.state AS issue_state, i.is_pull AS issue_is_pull, pr.is_draft AS issue_is_draft,
                i.repo_id AS issue_repo_id, o.username AS issue_owner, r.name AS issue_repo
             FROM project_cards c LEFT JOIN issues i ON i.id = c.issue_id LEFT JOIN pull_requests pr ON pr.issue_id = c.issue_id
                LEFT JOIN repositories r ON r.id = i.repo_id LEFT JOIN users o ON o.id = r.owner_id
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

    pub async fn add_project_column(&self, project: &Project, name: &str, user: &User) -> Result<i64> {
        self.ensure_board_writable(project).await?;
        validate_column(name)?;
        let mut tx = self.write_tx().await?;
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
        let column = self.column_of(project, id).await?;
        self.changed(project, Change::Column("created", &column), Some(user.id)).await;
        Ok(id)
    }

    /// Renames a column, and makes it the done column (or not). A board
    /// has at most one.
    pub async fn edit_project_column(&self, project: &Project, column_id: i64, name: &str, done: bool, user: &User) -> Result<()> {
        self.ensure_board_writable(project).await?;
        validate_column(name)?;
        self.column_of(project, column_id).await?;
        let mut tx = self.write_tx().await?;
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
        let column = self.column_of(project, column_id).await?;
        self.changed(project, Change::Column("edited", &column), Some(user.id)).await;
        Ok(())
    }

    /// Moves a column to `position` (clamped) among the board's columns.
    pub async fn move_project_column(&self, project: &Project, column_id: i64, position: i64, user: &User) -> Result<()> {
        self.ensure_board_writable(project).await?;
        let column = self.column_of(project, column_id).await?;
        let mut tx = self.write_tx().await?;
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
        let column = self.column_of(project, column_id).await?;
        self.changed(project, Change::Column("moved", &column), Some(user.id)).await;
        Ok(())
    }

    /// Deletes a column with its cards.
    pub async fn delete_project_column(&self, project: &Project, column_id: i64, user: &User) -> Result<()> {
        self.ensure_board_writable(project).await?;
        let column = self.column_of(project, column_id).await?;
        let cards = self.card_rows(project.id, Some(column.id), None).await?;
        let mut tx = self.write_tx().await?;
        sqlx::query("DELETE FROM project_columns WHERE id = ?1").bind(column.id).execute(&mut *tx).await?;
        sqlx::query("UPDATE project_columns SET position = position - 1 WHERE project_id = ?1 AND position > ?2")
            .bind(project.id)
            .bind(column.position)
            .execute(&mut *tx)
            .await?;
        touch(&mut tx, project.id).await?;
        tx.commit().await?;
        for card in &cards {
            self.changed(project, Change::Card(card, CardChange::Deleted), Some(user.id)).await;
        }
        self.changed(project, Change::Column("deleted", &column), Some(user.id)).await;
        Ok(())
    }

    async fn insert_card(&self, project: &Project, column_id: i64, issue_id: Option<i64>, note: Option<&str>, user: &User) -> Result<i64> {
        self.ensure_board_writable(project).await?;
        self.column_of(project, column_id).await?;
        let mut tx = self.write_tx().await?;
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
        let card = self.card_row(project, id).await?;
        self.changed(project, Change::Card(&card, CardChange::Created), Some(user.id)).await;
        Ok(id)
    }

    /// Adds an issue or pull request to the bottom of a column: of the
    /// board's repository, or of one of the board's organization.
    pub async fn add_issue_card(&self, project: &Project, column_id: i64, issue: &Issue, user: &User) -> Result<i64> {
        let ours = match project.owner() {
            ProjectOwner::Repo(repo) => issue.repo_id == repo,
            ProjectOwner::Org(org) => self.repo_by_id(issue.repo_id).await?.is_some_and(|r| r.owner_id == org),
        };
        if !ours {
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

    /// Cards of a project as they are: all, those of a column, or one.
    async fn card_rows(&self, project_id: i64, column_id: Option<i64>, card_id: Option<i64>) -> Result<Vec<CardRow>> {
        Ok(sqlx::query_as(
            "SELECT c.id, c.column_id, col.name AS column_name, c.note, c.issue_id, i.number AS issue_number,
                o.username AS issue_owner, r.name AS issue_repo, u.username AS creator, c.created_at
             FROM project_cards c JOIN project_columns col ON col.id = c.column_id
                LEFT JOIN issues i ON i.id = c.issue_id LEFT JOIN repositories r ON r.id = i.repo_id
                LEFT JOIN users o ON o.id = r.owner_id LEFT JOIN users u ON u.id = c.creator_id
             WHERE c.project_id = ?1 AND (?2 IS NULL OR c.column_id = ?2) AND (?3 IS NULL OR c.id = ?3)
             ORDER BY c.position, c.id",
        )
        .bind(project_id)
        .bind(column_id)
        .bind(card_id)
        .fetch_all(&self.db)
        .await?)
    }

    /// A card of `project`, or `NotFound`.
    async fn card_row(&self, project: &Project, card_id: i64) -> Result<CardRow> {
        self.card_rows(project.id, None, Some(card_id)).await?.pop().ok_or(Error::NotFound)
    }

    /// Moves a card to `position` (clamped) of a column.
    pub async fn move_card(&self, project: &Project, card_id: i64, column_id: i64, position: i64, user: &User) -> Result<()> {
        self.ensure_board_writable(project).await?;
        let before = self.card_row(project, card_id).await?;
        let from = self.column_of(project, before.column_id).await?;
        self.column_of(project, column_id).await?;
        let mut tx = self.write_tx().await?;
        place(&mut tx, card_id, column_id, position).await?;
        touch(&mut tx, project.id).await?;
        tx.commit().await?;
        let card = self.card_row(project, card_id).await?;
        self.changed(project, Change::Card(&card, CardChange::Moved(from)), Some(user.id)).await;
        Ok(())
    }

    pub async fn edit_note_card(&self, project: &Project, card_id: i64, note: &str, user: &User) -> Result<()> {
        self.ensure_board_writable(project).await?;
        let before = self.card_row(project, card_id).await?;
        let note = note.trim();
        if note.is_empty() || note.len() > MAX_NOTE {
            return invalid(format!("Notes need 1 to {MAX_NOTE} characters."));
        }
        let Some(old) = before.note else { return Ok(()) };
        sqlx::query("UPDATE project_cards SET note = ?2 WHERE id = ?1 AND note IS NOT NULL")
            .bind(card_id)
            .bind(note)
            .execute(&self.db)
            .await?;
        if old != note {
            let card = self.card_row(project, card_id).await?;
            self.changed(project, Change::Card(&card, CardChange::Edited(old)), Some(user.id)).await;
        }
        Ok(())
    }

    pub async fn delete_card(&self, project: &Project, card_id: i64, user: &User) -> Result<()> {
        self.ensure_board_writable(project).await?;
        let card = self.card_row(project, card_id).await?;
        let (column, position): (i64, i64) = sqlx::query_as("SELECT column_id, position FROM project_cards WHERE id = ?1")
            .bind(card_id)
            .fetch_one(&self.db)
            .await?;
        let mut tx = self.write_tx().await?;
        sqlx::query("DELETE FROM project_cards WHERE id = ?1").bind(card_id).execute(&mut *tx).await?;
        sqlx::query("UPDATE project_cards SET position = position - 1 WHERE column_id = ?1 AND position > ?2")
            .bind(column)
            .bind(position)
            .execute(&mut *tx)
            .await?;
        touch(&mut tx, project.id).await?;
        tx.commit().await?;
        self.changed(project, Change::Card(&card, CardChange::Deleted), Some(user.id)).await;
        Ok(())
    }

    /// The open boards an issue is on, and in which column.
    pub async fn issue_cards(&self, issue_id: i64) -> Result<Vec<IssueCard>> {
        Ok(sqlx::query_as(
            "SELECT p.number, p.title, col.name AS column, p.owner_id AS org_id, org.username AS org FROM project_cards c
             JOIN projects p ON p.id = c.project_id JOIN project_columns col ON col.id = c.column_id
             LEFT JOIN users org ON org.id = p.owner_id
             WHERE c.issue_id = ?1 AND p.closed_at IS NULL ORDER BY p.owner_id IS NOT NULL, p.number",
        )
        .bind(issue_id)
        .fetch_all(&self.db)
        .await?)
    }

    /// After an issue was closed (or merged) or reopened by `actor`: moves
    /// its cards to their board's done column, or out of it to the first
    /// column.
    pub(crate) async fn move_cards_of(&self, issue_id: i64, open: bool, actor: Option<i64>) -> Result<()> {
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
        for (card, project_id, column) in moves {
            let project: Option<Project> = sqlx::query_as(concat!(project_select!(), "WHERE p.id = ?1"))
                .bind(project_id)
                .fetch_optional(&self.db)
                .await?;
            let Some(project) = project else { continue };
            let before = self.card_row(&project, card).await?;
            let from = self.column_of(&project, before.column_id).await?;
            let mut tx = self.write_tx().await?;
            place(&mut tx, card, column, 0).await?;
            touch(&mut tx, project.id).await?;
            tx.commit().await?;
            let after = self.card_row(&project, card).await?;
            self.changed(&project, Change::Card(&after, CardChange::Moved(from)), actor).await;
        }
        Ok(())
    }

    // -- Timelines and webhooks ---------------------------------------------

    /// Records a change to a board: in the timeline of the issue on a card
    /// (added, moved to another column, removed), and as a webhook. Errors
    /// are logged: the change itself already happened.
    async fn changed(&self, project: &Project, change: Change<'_>, actor: Option<i64>) {
        if let Err(e) = self.try_changed(project, change, actor).await {
            tracing::warn!("recording a board change failed: {e}");
        }
    }

    async fn try_changed(&self, project: &Project, change: Change<'_>, actor: Option<i64>) -> Result<()> {
        // Who the board belongs to: a repository, or an organization.
        let (repo, org) = match project.owner() {
            ProjectOwner::Repo(id) => (Some(self.repo_by_id(id).await?.ok_or(Error::NotFound)?), None),
            ProjectOwner::Org(id) => (None, Some(self.user_by_id(id).await?.ok_or(Error::NotFound)?)),
        };
        if let Change::Card(card, how) = &change
            && let Some(issue_id) = card.issue_id
        {
            let event = match how {
                CardChange::Created => Some(("added_to_project", None)),
                CardChange::Moved(from) if from.id != card.column_id => Some(("moved_columns_in_project", Some(&from.name))),
                CardChange::Deleted => Some(("removed_from_project", None)),
                _ => None,
            };
            if let Some((kind, from)) = event {
                let body = json!({
                    "project": project.title,
                    "number": project.number,
                    "org": org.as_ref().map(|o| &o.username),
                    "column": card.column_name,
                    "from": from,
                });
                let mut tx = self.write_tx().await?;
                match insert_comment(&mut tx, issue_id, actor, kind, &body.to_string()).await {
                    Ok(_) => tx.commit().await?,
                    // An archived repository's issue keeps its timeline.
                    Err(Error::Invalid(_)) => {}
                    Err(e) => return Err(e),
                }
            }
        }

        let project_url = match (&repo, &org) {
            (Some(r), _) => format!("{}/{}/{}/projects/{}", self.config.base_url, r.owner, r.name, project.number),
            (_, Some(o)) => format!("{}/org/{}/projects/{}", self.config.base_url, o.username, project.number),
            _ => unreachable!("a board has an owner"),
        };
        let (event, action, key, value, changes) = match change {
            Change::Project(action) => ("project", action, "project", self.project_json(project, &project_url), Value::Null),
            Change::Column(action, column) => {
                let value = json!({
                    "id": column.id,
                    "name": column.name,
                    "position": column.position,
                    "done": column.is_done,
                    "project_url": project_url,
                });
                ("project_column", action, "project_column", value, Value::Null)
            }
            Change::Card(card, how) => {
                let (action, changes) = match how {
                    CardChange::Created => ("created", Value::Null),
                    CardChange::Moved(from) => ("moved", json!({ "column_id": { "from": from.id } })),
                    CardChange::Edited(note) => ("edited", json!({ "note": { "from": note } })),
                    CardChange::Deleted => ("deleted", Value::Null),
                };
                let content_url = match (&card.issue_owner, &card.issue_repo, card.issue_number) {
                    (Some(owner), Some(name), Some(n)) => json!(format!("{}/api/v1/repos/{owner}/{name}/issues/{n}", self.config.base_url)),
                    _ => Value::Null,
                };
                let value = json!({
                    "id": card.id,
                    "note": card.note,
                    "column_id": card.column_id,
                    "column_name": card.column_name,
                    "content_url": content_url,
                    "creator": card.creator.as_ref().map(|c| json!({ "login": c })),
                    "created_at": time(card.created_at),
                    "project_url": project_url,
                });
                ("project_card", action, "project_card", value, changes)
            }
        };
        let mut payload = json!({ "action": action, key: value, "sender": self.sender_json(actor).await? });
        if !changes.is_null() {
            payload["changes"] = changes;
        }
        match (&repo, &org) {
            (Some(r), _) => {
                payload["repository"] = self.repo_json(r);
                self.enqueue_webhooks(r.id, event, action, &payload).await
            }
            (_, Some(o)) => {
                payload["organization"] = self.user_json(o);
                self.enqueue_org_webhooks(o.id, event, action, &payload).await
            }
            _ => Ok(()),
        }
    }

    fn project_json(&self, p: &Project, url: &str) -> Value {
        json!({
            "id": p.id,
            "number": p.number,
            "name": p.title,
            "body": p.description,
            "state": if p.is_open() { "open" } else { "closed" },
            "html_url": url,
            "creator": p.creator.as_ref().map(|c| json!({ "login": c })),
            "created_at": time(p.created_at),
            "updated_at": time(p.updated_at),
        })
    }
}

/// Which repository a board's link to an issue goes to.
pub fn card_repo(card: &ProjectCard, board_repo: Option<&Repo>) -> Option<(String, String)> {
    match (&card.issue_owner, &card.issue_repo) {
        (Some(owner), Some(name)) => Some((owner.clone(), name.clone())),
        _ => board_repo.map(|r| (r.owner.clone(), r.name.clone())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{HookOwner, OrgRole};

    #[tokio::test]
    async fn boards() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let repo = core.create_repo(&ada, "engine", "", false).await.unwrap();
        let other = core.create_repo(&ada, "other", "", false).await.unwrap();
        let bug = core.create_issue(repo.id, &ada, "Bug", "").await.unwrap();
        let feature = core.create_issue(repo.id, &ada, "Feature", "").await.unwrap();
        let elsewhere = core.create_issue(other.id, &ada, "Elsewhere", "").await.unwrap();
        let here = ProjectOwner::Repo(repo.id);

        assert!(core.create_project(here, &ada, " ", "", true).await.is_err());
        let p = core.create_project(here, &ada, "Roadmap", "", true).await.unwrap();
        assert_eq!((p.number, p.owner()), (1, here));
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

        core.move_card(&p, n, todo, 0, &ada).await.unwrap();
        core.move_card(&p, a, doing, 5, &ada).await.unwrap();
        let cards = core.project_cards(p.id).await.unwrap();
        assert_eq!((order(&cards, todo), order(&cards, doing)), (vec![n, b], vec![a]));
        assert!(cards.iter().all(|c| c.position < 2), "no gaps");

        // Closing moves to Done, reopening back to To do.
        core.set_issue_open(&bug, &ada, false).await.unwrap();
        let cards = core.project_cards(p.id).await.unwrap();
        assert_eq!(order(&cards, done), [a]);
        let p = core.project(here, 1).await.unwrap().unwrap();
        assert_eq!((p.open_items, p.closed_items, p.notes, p.progress()), (1, 1, 1, 50));
        let bug = core.issue(repo.id, bug.number).await.unwrap().unwrap();
        core.set_issue_open(&bug, &ada, true).await.unwrap();
        assert_eq!(order(&core.project_cards(p.id).await.unwrap(), todo), [a, n, b]);
        let on = core.issue_cards(bug.id).await.unwrap();
        assert_eq!((on.len(), on[0].column.as_str(), on[0].org.as_deref()), (1, "To do", None));

        // Columns.
        core.move_project_column(&p, done, 0, &ada).await.unwrap();
        let names: Vec<String> = core.project_columns(p.id).await.unwrap().into_iter().map(|c| c.name).collect();
        assert_eq!(names, ["Done", "To do", "In progress"]);
        core.edit_project_column(&p, doing, "Doing", true, &ada).await.unwrap();
        let cols = core.project_columns(p.id).await.unwrap();
        assert_eq!(cols.iter().filter(|c| c.is_done).map(|c| c.id).collect::<Vec<_>>(), [doing], "one done column");
        core.delete_project_column(&p, todo, &ada).await.unwrap();
        let cols = core.project_columns(p.id).await.unwrap();
        assert_eq!(cols.iter().map(|c| c.position).collect::<Vec<_>>(), [0, 1]);
        assert_eq!(core.project_cards(p.id).await.unwrap().len(), 0, "its cards went with it");

        core.set_project_open(&p, false, &ada).await.unwrap();
        assert_eq!(core.project_counts(here).await.unwrap(), (0, 1));
        assert!(core.issue_cards(bug.id).await.unwrap().is_empty(), "closed boards aren't listed");
        core.delete_project(&p, &ada).await.unwrap();
        assert!(core.project(here, 1).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn timelines_and_webhooks() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let repo = core.create_repo(&ada, "engine", "", false).await.unwrap();
        let hook = core
            .create_webhook(HookOwner::Repo(repo.id), "https://ci.example.com/hook", "", &["project".into(), "project_card".into(), "repository".into()], true)
            .await
            .unwrap();
        let bug = core.create_issue(repo.id, &ada, "Bug", "").await.unwrap();
        let p = core.create_project(ProjectOwner::Repo(repo.id), &ada, "Roadmap", "", true).await.unwrap();
        let cols = core.project_columns(p.id).await.unwrap();
        let card = core.add_issue_card(&p, cols[0].id, &bug, &ada).await.unwrap();
        core.move_card(&p, card, cols[0].id, 0, &ada).await.unwrap(); // same column: no timeline entry
        core.move_card(&p, card, cols[1].id, 0, &ada).await.unwrap();
        core.set_issue_open(&bug, &ada, false).await.unwrap(); // to Done, automatically
        core.delete_card(&p, card, &ada).await.unwrap();

        let events: Vec<(String, Value)> = core
            .comments(bug.id)
            .await
            .unwrap()
            .into_iter()
            .filter(|c| c.kind.contains("project"))
            .map(|c| (c.kind, serde_json::from_str(&c.body).unwrap()))
            .collect();
        let kinds: Vec<&str> = events.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(
            kinds,
            ["added_to_project", "moved_columns_in_project", "moved_columns_in_project", "removed_from_project"]
        );
        assert_eq!(events[0].1, json!({ "project": "Roadmap", "number": 1, "org": null, "column": "To do", "from": null }));
        assert_eq!((&events[2].1["from"], &events[2].1["column"]), (&json!("In progress"), &json!("Done")));

        let mut deliveries = core.deliveries(hook.id, 50).await.unwrap();
        deliveries.reverse();
        let sent: Vec<String> = deliveries
            .iter()
            .filter(|d| d.event.starts_with("project"))
            .map(|d| format!("{} {}", d.event, d.action))
            .collect();
        assert_eq!(
            sent,
            [
                "project created",
                "project_card created",
                "project_card moved",
                "project_card moved",
                "project_card moved",
                "project_card deleted"
            ]
        );
        let moved: Value = serde_json::from_str(&deliveries.iter().find(|d| d.action == "moved").unwrap().payload).unwrap();
        assert_eq!(moved["changes"]["column_id"]["from"], cols[0].id);
        assert_eq!(moved["project_card"]["content_url"], "http://localhost:3000/api/v1/repos/ada/engine/issues/1");
        assert_eq!(moved["repository"]["full_name"], "ada/engine");
        assert_eq!(moved["sender"]["login"], "ada");

        // Archiving.
        core.set_archived(repo.id, true, Some(ada.id)).await.unwrap();
        core.set_archived(repo.id, true, Some(ada.id)).await.unwrap(); // no change, no event
        core.set_archived(repo.id, false, Some(ada.id)).await.unwrap();
        let archiving: Vec<String> = core
            .deliveries(hook.id, 50)
            .await
            .unwrap()
            .into_iter()
            .filter(|d| d.event == "repository")
            .map(|d| d.action)
            .collect();
        assert_eq!(archiving, ["unarchived", "archived"]);
    }

    #[tokio::test]
    async fn organization_boards() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let bob = core.register("bob", "bob@example.com", "password1").await.unwrap();
        let eve = core.register("eve", "eve@example.com", "password1").await.unwrap();
        let org = core.create_org(&ada, "acme").await.unwrap();
        core.set_org_member(&org, "bob", OrgRole::Member).await.unwrap();
        let tool = core.create_repo(&org, "tool", "", false).await.unwrap();
        let site = core.create_repo(&org, "site", "", true).await.unwrap();
        let own = core.create_repo(&eve, "own", "", false).await.unwrap();
        let hook = core
            .create_webhook(HookOwner::Org(org.id), "https://ci.example.com/acme", "", &["project_card".into()], true)
            .await
            .unwrap();
        let a = core.create_issue(tool.id, &ada, "In tool", "").await.unwrap();
        let b = core.create_issue(site.id, &bob, "In site", "").await.unwrap();
        let c = core.create_issue(own.id, &eve, "Not acme's", "").await.unwrap();

        assert_eq!(core.org_board_access(&org, Some(&bob)).await.unwrap(), crate::Access::Write);
        assert_eq!(core.org_board_access(&org, Some(&eve)).await.unwrap(), crate::Access::None);
        assert_eq!(core.org_board_access(&org, None).await.unwrap(), crate::Access::None);

        let acme = ProjectOwner::Org(org.id);
        let p = core.create_project(acme, &bob, "Launch", "", true).await.unwrap();
        assert_eq!((p.number, p.owner(), p.repo_id), (1, acme, None));
        // Numbers are per owner.
        assert_eq!(core.create_project(ProjectOwner::Repo(tool.id), &ada, "Tool", "", true).await.unwrap().number, 1);
        assert_eq!(core.projects(acme, true).await.unwrap().len(), 1);
        assert_eq!(core.project_counts(acme).await.unwrap(), (1, 0));

        let todo = core.project_columns(p.id).await.unwrap()[0].id;
        core.add_issue_card(&p, todo, &a, &bob).await.unwrap();
        core.add_issue_card(&p, todo, &b, &bob).await.unwrap();
        assert!(matches!(core.add_issue_card(&p, todo, &c, &bob).await, Err(Error::NotFound)), "only the organization's");
        let cards = core.project_cards(p.id).await.unwrap();
        let repos: Vec<(Option<&str>, Option<&str>)> =
            cards.iter().map(|c| (c.issue_owner.as_deref(), c.issue_repo.as_deref())).collect();
        assert_eq!(repos, [(Some("acme"), Some("tool")), (Some("acme"), Some("site"))]);
        let on = core.issue_cards(a.id).await.unwrap();
        assert_eq!((on[0].org.as_deref(), on[0].org_id), (Some("acme"), Some(org.id)));

        // In timelines, and to the organization's hooks.
        let entry = core.comments(b.id).await.unwrap().into_iter().find(|c| c.kind == "added_to_project").unwrap();
        assert!(entry.body.contains(r#""org":"acme""#), "{}", entry.body);
        let d: Vec<_> = core.deliveries(hook.id, 10).await.unwrap().into_iter().filter(|d| d.event == "project_card").collect();
        assert_eq!(d.len(), 2);
        let payload: Value = serde_json::from_str(&d[0].payload).unwrap();
        assert_eq!((&payload["organization"]["login"], &payload["repository"]), (&json!("acme"), &Value::Null));

        // `project:` in issue search; the board is for members.
        let search = |viewer: &User, q: &str| {
            let (core, viewer, q) = (core.clone(), viewer.clone(), crate::IssueQuery::parse(q, None));
            async move { core.search_issues(Some(&viewer), None, &q, 0, 10).await.unwrap().1 }
        };
        assert_eq!(search(&bob, "project:acme/1").await, 2);
        assert_eq!(search(&bob, "project:acme/1 is:open in tool").await, 1);
        assert_eq!(search(&eve, "project:acme/1").await, 0, "not a member");
        assert_eq!(search(&ada, "project:acme/tool/1").await, 0, "the tool board is empty");
        assert_eq!(search(&ada, "project:acme/9").await, 0);
    }
}
