//! Domain logic shared by the web and SSH frontends: configuration, the
//! database, users, sessions, SSH keys, repositories and permissions.

mod auth;
mod config;
mod names;
mod repos;
mod ssh_keys;
mod users;

use std::sync::Arc;
use std::time::Duration;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};

pub use config::{Config, SshConfig};
pub use names::{validate_repo_name, validate_username};
pub use repos::{Access, Repo};
pub use ssh_keys::{SshKey, parse_public_key};
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

/// Shared handle to the database and configuration. Cheap to clone.
#[derive(Clone)]
pub struct Core {
    pub db: SqlitePool,
    pub config: Arc<Config>,
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
        };
        core.delete_expired_sessions().await?;
        Ok(core)
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
