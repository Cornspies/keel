//! Domain logic shared by the web and SSH frontends: configuration, the
//! database, users, sessions, SSH keys, repositories and permissions.

// Macros rather than consts so queries stay `&'static str` via `concat!`
// (sqlx refuses dynamically built SQL).
macro_rules! user_columns {
    () => {
        "id, username, COALESCE(email, '') AS email, password_hash, is_admin, kind, created_at"
    };
}

mod auth;
mod cache;
mod config;
mod issues;
mod names;
mod permissions;
mod pulls;
mod repos;
mod ssh_keys;
mod tokens;
mod users;

use std::sync::Arc;
use std::time::Duration;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};

pub use cache::EntryCommits;
pub use config::{Config, SshConfig};
pub use issues::{Comment, Issue, IssueCounts};
pub use names::{validate_repo_name, validate_username};
pub use permissions::{Access, OrgRole};
pub use pulls::{PullRequest, pull_ref};
pub use repos::Repo;
pub use ssh_keys::{SshKey, parse_public_key};
pub use tokens::{AccessToken, looks_like_token};
pub use users::User;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not found")]
    NotFound,
    /// Invalid user input; the message is shown to the user.
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl From<sqlx::Error> for Error {
    fn from(e: sqlx::Error) -> Self {
        Self::Internal(e.into())
    }
}

impl From<keel_git::Error> for Error {
    fn from(e: keel_git::Error) -> Self {
        Self::Internal(e.into())
    }
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

pub(crate) fn invalid<T>(msg: impl Into<String>) -> Result<T> {
    Err(Error::Invalid(msg.into()))
}

pub(crate) fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default()
}

/// Something that happened to a repository, for background work such as
/// cache warming and search indexing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RepoEvent {
    /// Refs changed (a push or a merge) and maintenance has run.
    Pushed { repo_id: i64 },
    Deleted { repo_id: i64 },
}

/// Shared handle to the database and configuration. Cheap to clone.
#[derive(Clone)]
pub struct Core {
    pub db: SqlitePool,
    pub config: Arc<Config>,
    events: tokio::sync::broadcast::Sender<RepoEvent>,
}

impl Core {
    /// Opens (creating if needed) the data directory and database and runs
    /// migrations.
    pub async fn open(config: Config) -> anyhow::Result<Self> {
        std::fs::create_dir_all(config.repos_dir())?;
        let options = SqliteConnectOptions::new()
            .filename(config.db_path())
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .foreign_keys(true)
            .busy_timeout(Duration::from_secs(5));
        let db = SqlitePoolOptions::new()
            .max_connections(8)
            .connect_with(options)
            .await?;
        sqlx::migrate!("./migrations").run(&db).await?;
        let core = Self {
            db,
            config: Arc::new(config),
            events: tokio::sync::broadcast::channel(256).0,
        };
        core.delete_expired_sessions().await?;
        Ok(core)
    }

    /// Subscribes to repository events. Slow subscribers may miss events
    /// (they get `RecvError::Lagged`) rather than holding up pushes.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<RepoEvent> {
        self.events.subscribe()
    }

    pub(crate) fn emit(&self, event: RepoEvent) {
        // No subscribers is fine.
        let _ = self.events.send(event);
    }

    #[cfg(test)]
    pub(crate) async fn for_tests() -> (Self, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let config = Config {
            data_dir: dir.path().to_owned(),
            ..Config::default()
        };
        (Self::open(config).await.unwrap(), dir)
    }
}
