//! Domain logic shared by the web and SSH frontends: configuration, the
//! database, users, sessions, SSH keys, repositories and permissions.

// Macros rather than consts so queries stay `&'static str` via `concat!`
// (sqlx refuses dynamically built SQL).
macro_rules! user_columns {
    () => {
        "id, username, COALESCE(email, '') AS email, password_hash, is_admin, kind, created_at,
         email_verified_at IS NOT NULL AS email_verified, email_notifications, disabled_at IS NOT NULL AS disabled,
         totp_secret IS NOT NULL AS totp_enabled, avatar_type, avatar_version, theme"
    };
}

mod activity;
mod admin;
mod auth;
mod auto_merge;
mod avatars;
mod backup;
mod cache;
mod config;
mod deploy_keys;
mod events;
mod file_edits;
mod imports;
mod insights;
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
mod projects;
mod protection;
mod pulls;
mod quotas;
mod reactions;
mod secrets;
mod releases;
mod repos;
mod reviews;
mod signatures;
mod signing;
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

pub use auto_merge::{parse_style, style_name};
pub use backup::{BackupReport, backup};
pub use activity::{Activity, IssueAction, label_json, time as json_time};
pub use admin::{AdminUser, SiteSettings, SiteStats};
pub use cache::EntryCommits;
pub use deploy_keys::{DeployKey, MAX_DEPLOY_KEYS};
pub use config::{Config, MailConfig, Network, SshConfig};
pub use insights::{Contributor, DAY, RepoActivity};
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
pub use projects::{IssueCard, KANBAN, Project, ProjectCard, ProjectColumn, ProjectOwner, card_repo};
pub use signatures::{GpgKey, Verification, parse_gpg_key};
pub use ssh_keys::{SshKey, parse_public_key};
pub use statuses::{CommitStatus, STATUS_STATES, combined_state};
pub use tokens::{AccessToken, looks_like_token};
pub use totp::{RECOVERY_CODES, SecondStep, totp_uri};
pub use users::{THEMES, User};
pub use avatars::{MAX_AVATAR, identicon, image_type};
pub use file_edits::{FileEdit, FileWrite, clean_path};
pub use imports::{
    Import, ImportOptions, ImportService, ImportSource, ImportedComment, ImportedIssue, ImportedLabel, ImportedMilestone,
    ImportedPull, ImportedRelease, import_source, label_color,
};
pub use issue_templates::{IssueTemplate, IssueTemplates, parse_issue_template};
pub use reactions::{REACTIONS, Reaction, ReactionGroup, group_reactions, reaction_kind};
pub use wiki::{GitSession, GitTarget, SPECIAL as WIKI_SPECIAL, WikiAction, WikiContent, WikiEdit, WikiPage, page_name, page_title};
pub use webhooks::{Attempt, Delivery, HookOwner, Outbound, WEBHOOK_EVENTS, Webhook, is_private_ip, sign};

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
    /// Held while merging pull requests automatically, one at a time.
    auto_merge_lock: Arc<tokio::sync::Mutex<()>>,
    /// Commit counts for graphs, by repository id.
    activity_cache: Arc<std::sync::Mutex<std::collections::HashMap<i64, Arc<insights::RepoActivity>>>>,
    /// See [`signatures::SignatureCache`].
    signature_cache: Arc<signatures::SignatureCache>,
    /// Instance settings from the admin page, read on every page view.
    settings: Arc<std::sync::RwLock<SiteSettings>>,
    /// Signs short-lived tokens (Git LFS).
    token_secret: Arc<String>,
    metrics: Arc<Metrics>,
    /// See [`users::LoginFailures`].
    login_failures: Arc<std::sync::Mutex<users::LoginFailures>>,
    /// Signs the commits keel makes.
    signing_key: Arc<signing::SigningKey>,
    /// See [`Core::setup_code`].
    setup_code: Arc<std::sync::Mutex<Option<String>>>,
    /// Encrypts secrets kept in the database.
    secrets: Arc<secrets::SecretBox>,
}

/// Makes a data directory from before 0.15 (or one someone loosened)
/// private again: the directory `0700`, the database files `0600`. SQLite
/// gives its `-wal` and `-shm` files the database's permissions.
fn tighten_permissions(data_dir: &std::path::Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let db = data_dir.join("keel.db");
        let wal = data_dir.join("keel.db-wal");
        let shm = data_dir.join("keel.db-shm");
        for (path, mode) in [(data_dir, 0o700), (db.as_path(), 0o600), (wal.as_path(), 0o600), (shm.as_path(), 0o600)] {
            let Ok(meta) = std::fs::metadata(path) else { continue };
            if meta.permissions().mode() & 0o077 != 0 {
                match std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)) {
                    Ok(()) => tracing::info!("made {} private ({mode:o})", path.display()),
                    Err(e) => tracing::warn!("{} is readable by others and couldn't be changed: {e}", path.display()),
                }
            }
        }
    }
}

impl Core {
    /// Opens (creating if needed) the data directory and database and runs
    /// migrations.
    pub async fn open(mut config: Config) -> anyhow::Result<Self> {
        if !config.data_dir.exists() {
            // Holds secrets; see `backup`.
            std::fs::create_dir_all(&config.data_dir)?;
            backup::private(&config.data_dir, 0o700)?;
        }
        std::fs::create_dir_all(config.repos_dir())?;
        // Every path is built from `data_dir`; keep them all absolute. Git
        // resolves a relative `core.hooksPath` from the repository it runs
        // in, finds no hook there and silently skips branch protection.
        config.data_dir = std::fs::canonicalize(&config.data_dir)?;
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
        tighten_permissions(&config.data_dir);
        let token_secret = Arc::new(Self::load_token_secret(&db).await?);
        let signing_key = signing::SigningKey::load(&config.data_dir.join(signing::KEY_FILE), true)?;
        // A missing key must not be replaced while secrets need the old one.
        let key_path = config.secret_key_path();
        let create = !key_path.exists();
        if create && secrets::any_encrypted(&db).await? {
            anyhow::bail!(
                "the database has encrypted secrets, but their key {} is missing: restore it, \
                 or point --secret-key-file at it",
                key_path.display()
            );
        }
        let secret_box = secrets::SecretBox::load(&key_path, create)?;
        let core = Self::assemble(db, config, token_secret, signing_key, secret_box);
        let encrypted = core.encrypt_plain_secrets().await?;
        if encrypted > 0 {
            tracing::info!("encrypted {encrypted} secrets stored by an earlier version");
        }
        core.load_settings().await?;
        core.announce_setup_code().await?;
        core.delete_expired_sessions().await?;
        core.prune_notifications(180).await?;
        core.prune_mail().await?;
        core.prune_lfs().await?;
        core.prune_release_assets().await?;
        core.prune_events().await?;
        Ok(core)
    }

    /// Opens an existing data directory read-only, without migrations or
    /// cleaning up: for the `pre-receive` hook, which runs in its own
    /// process while the server does.
    pub(crate) async fn open_read_only(config: Config) -> anyhow::Result<Self> {
        let options = SqliteConnectOptions::new()
            .filename(config.db_path())
            .read_only(true)
            .busy_timeout(Duration::from_secs(5));
        let db = SqlitePoolOptions::new().max_connections(2).connect_with(options).await?;
        let signing_key = signing::SigningKey::load(&config.data_dir.join(signing::KEY_FILE), false)?;
        Ok(Self::assemble(db, config, Arc::default(), signing_key, secrets::SecretBox::ephemeral()))
    }

    fn assemble(
        db: SqlitePool,
        config: Config,
        token_secret: Arc<String>,
        signing_key: signing::SigningKey,
        secret_box: secrets::SecretBox,
    ) -> Self {
        Self {
            db,
            config: Arc::new(config),
            events: tokio::sync::broadcast::channel(256).0,
            hook_wakeup: Arc::default(),
            mail_wakeup: Arc::default(),
            mirror_wakeup: Arc::default(),
            import_wakeup: Arc::default(),
            auto_merge_lock: Arc::default(),
            activity_cache: Arc::default(),
            signature_cache: Arc::default(),
            settings: Arc::default(),
            token_secret,
            metrics: Arc::default(),
            login_failures: Arc::default(),
            signing_key: Arc::new(signing_key),
            setup_code: Arc::default(),
            secrets: Arc::new(secret_box),
        }
    }

    /// The public key commits keel makes are signed with, in OpenSSH's
    /// format, for `allowed_signers` files.
    pub fn signing_public_key(&self) -> &str {
        &self.signing_key.public
    }

    /// What signs the commits keel makes, to move into blocking git work.
    pub(crate) fn signer(&self) -> Arc<signing::SigningKey> {
        self.signing_key.clone()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn relative_data_dir_becomes_absolute() {
        let dir = tempfile::tempdir().unwrap();
        // The same directory as a path relative to the current directory,
        // without changing it (tests run in parallel).
        let cwd = std::env::current_dir().unwrap();
        let mut relative = std::path::PathBuf::new();
        for _ in 1..cwd.components().count() {
            relative.push("..");
        }
        relative.push(dir.path().strip_prefix("/").unwrap());
        assert!(relative.is_relative());
        let config = Config { data_dir: relative, ..Config::default() };
        let core = Core::open(config).await.unwrap();
        assert!(core.config.data_dir.is_absolute());
        assert_eq!(core.config.data_dir, std::fs::canonicalize(dir.path()).unwrap());
        assert!(core.config.hooks_dir().is_absolute());
    }
}
