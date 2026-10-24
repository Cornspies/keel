//! Domain logic shared by the web and SSH frontends: configuration, the
//! database, users, sessions, SSH keys, repositories and permissions.

// Macros rather than consts so queries stay `&'static str` via `concat!`
// (sqlx refuses dynamically built SQL).
macro_rules! user_columns {
    () => {
        "id, username, COALESCE(email, '') AS email, password_hash, is_admin, kind, created_at,
         email_verified_at IS NOT NULL AS email_verified, email_notifications, disabled_at IS NOT NULL AS disabled,
         totp_secret IS NOT NULL AS totp_enabled, avatar_type, avatar_version"
    };
}

mod activity;
mod admin;
mod auth;
mod avatars;
mod backup;
mod cache;
mod config;
mod events;
mod file_edits;
mod imports;
mod issue_templates;
mod issues;
mod labels;
mod lfs;
mod mail;
mod metrics;
mod names;
mod notifications;
mod milestones;
mod mirrors;
mod permissions;
mod protection;
mod pulls;
mod reactions;
mod releases;
mod repos;
mod reviews;
mod ssh_keys;
mod statuses;
mod tokens;
mod totp;
mod users;
mod webhooks;
mod wiki;

use std::sync::Arc;
use std::time::Duration;

use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};

pub use backup::{BackupReport, backup};
pub use activity::{Activity, IssueAction, label_json, time as json_time};
pub use admin::{AdminUser, SiteSettings, SiteStats};
pub use cache::EntryCommits;
pub use config::{Config, MailConfig, SshConfig};
pub use issues::{Comment, Issue, IssueCounts, IssueFilter};
pub use labels::{LABEL_COLORS, Label};
pub use lfs::{LFS_TOKEN_SECS, LfsLock, parse_pointer, valid_oid};
pub use metrics::Metrics;
pub use mail::Mail;
pub use names::{validate_repo_name, validate_username};
pub use notifications::{Notification, Subscription, mentions};
pub use milestones::{Milestone, parse_due_date};
pub use mirrors::{MIRROR_INTERVALS, Mirror, MirrorRemote, PushMirror};
pub use permissions::{Access, OrgRole};
pub use protection::{BranchRule, MergeCheck, check_update, pre_receive, rule_for};
pub use events::Event;
pub use releases::{Release, ReleaseAction, ReleaseAsset};
pub use pulls::{PullRequest, pull_ref, related};
pub use repos::{ExploreQuery, ExploreSort, IssueHit, IssueQuery, IssueSort, MAX_PINS, MAX_TOPICS, Repo, Stargazer, StateFilter, parse_topics};
pub use reviews::{InlineAnchor, Verdict, suggestion};
pub use ssh_keys::{SshKey, parse_public_key};
pub use statuses::{CommitStatus, STATUS_STATES, combined_state};
pub use tokens::{AccessToken, looks_like_token};
pub use totp::{RECOVERY_CODES, SecondStep, totp_uri};
pub use users::User;
pub use avatars::{MAX_AVATAR, identicon, image_type};
pub use file_edits::{FileEdit, FileWrite, clean_path};
pub use imports::{
    Import, ImportOptions, ImportService, ImportSource, ImportedComment, ImportedIssue, ImportedLabel, ImportedMilestone,
    ImportedPull, ImportedRelease, import_source, label_color,
};
pub use issue_templates::{IssueTemplate, IssueTemplates, parse_issue_template};
pub use reactions::{REACTIONS, Reaction, ReactionGroup, group_reactions, reaction_kind};
pub use wiki::{GitSession, GitTarget, SPECIAL as WIKI_SPECIAL, WikiAction, WikiContent, WikiEdit, WikiPage, page_name, page_title};
pub use webhooks::{Attempt, Delivery, HookOwner, WEBHOOK_EVENTS, Webhook, is_private_ip, sign};

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
    /// Signalled when a webhook delivery is queued.
    hook_wakeup: Arc<tokio::sync::Notify>,
    /// Signalled when mail is queued.
    mail_wakeup: Arc<tokio::sync::Notify>,
    /// Signalled when a mirror should sync.
    mirror_wakeup: Arc<tokio::sync::Notify>,
    /// Signalled when an import is queued.
    import_wakeup: Arc<tokio::sync::Notify>,
    /// Instance settings from the admin page, read on every page view.
    settings: Arc<std::sync::RwLock<SiteSettings>>,
    /// Signs short-lived tokens (Git LFS).
    token_secret: Arc<String>,
    metrics: Arc<Metrics>,
}

impl Core {
    /// Opens (creating if needed) the data directory and database and runs
    /// migrations.
    pub async fn open(config: Config) -> anyhow::Result<Self> {
        std::fs::create_dir_all(config.repos_dir())?;
        protection::install_hooks(&config.hooks_dir())?;
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
        let token_secret = Arc::new(Self::load_token_secret(&db).await?);
        let core = Self {
            db,
            config: Arc::new(config),
            events: tokio::sync::broadcast::channel(256).0,
            hook_wakeup: Arc::default(),
            mail_wakeup: Arc::default(),
            mirror_wakeup: Arc::default(),
            import_wakeup: Arc::default(),
            settings: Arc::default(),
            token_secret,
            metrics: Arc::default(),
        };
        core.load_settings().await?;
        core.delete_expired_sessions().await?;
        core.prune_notifications(180).await?;
        core.prune_mail().await?;
        core.prune_lfs().await?;
        core.prune_release_assets().await?;
        core.prune_events().await?;
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
