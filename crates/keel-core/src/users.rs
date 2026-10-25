use std::sync::OnceLock;

use crate::auth::{hash_password, hash_token, new_token, verify_password};
use crate::{Core, Result, invalid, now, validate_username};

pub const SESSION_TTL_SECS: i64 = 30 * 24 * 3600;

/// A user account or an organization (`kind == "org"`), which share one
/// namespace.
#[derive(Clone, sqlx::FromRow)]
pub struct User {
    pub id: i64,
    pub username: String,
    /// Empty for organizations.
    pub email: String,
    password_hash: String,
    pub is_admin: bool,
    pub kind: String,
    pub created_at: i64,
    /// Whether the user proved they receive mail at `email`.
    pub email_verified: bool,
    /// Which notifications are emailed: `all`, `participating` or `off`.
    pub email_notifications: String,
    /// Disabled by an administrator: can't sign in or use git.
    pub disabled: bool,
    /// Signing in needs a code from an authenticator app too.
    pub totp_enabled: bool,
    /// The uploaded avatar's content type; `None` for a generated one.
    pub avatar_type: Option<String>,
    /// Changes with every new avatar, to bust caches.
    pub avatar_version: i64,
    /// `auto` (follow the system), `light` or `dark`.
    #[sqlx(default)]
    pub theme: String,
}

impl User {
    pub fn is_org(&self) -> bool {
        self.kind == "org"
    }
}

/// The theme choices: follow the system, or always light or dark.
pub const THEMES: [&str; 3] = ["auto", "light", "dark"];

// Hand-written so the password hash never ends up in logs.
impl std::fmt::Debug for User {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("User")
            .field("id", &self.id)
            .field("username", &self.username)
            .field("is_admin", &self.is_admin)
            .finish_non_exhaustive()
    }
}


impl Core {
    pub async fn set_theme(&self, user_id: i64, theme: &str) -> Result<()> {
        if !THEMES.contains(&theme) {
            return invalid("Unknown theme.");
        }
        sqlx::query("UPDATE users SET theme = ?2 WHERE id = ?1").bind(user_id).bind(theme).execute(&self.db).await?;
        Ok(())
    }

    /// Creates an account. The very first account becomes the administrator
    /// and is allowed even when registration is closed.
    pub async fn register(&self, username: &str, email: &str, password: &str) -> Result<User> {
        self.insert_user(username, email, password, false).await
    }

    /// Creates an account; `by_admin` even when registration is closed.
    pub(crate) async fn insert_user(&self, username: &str, email: &str, password: &str, by_admin: bool) -> Result<User> {
        validate_username(username)?;
        let email = email.trim();
        if !email.contains('@') || email.len() > 254 {
            return invalid("Please enter a valid email address.");
        }
        if password.chars().count() < 8 {
            return invalid("Passwords must be at least 8 characters long.");
        }

        let hash = hash_password(password.to_owned()).await?;
        let mut tx = self.db.begin().await?;
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users WHERE kind = 'user'")
            .fetch_one(&mut *tx)
            .await?;
        if count > 0 && !by_admin && !self.registration_open() {
            return invalid("Registration is closed.");
        }
        let taken: Option<(String,)> =
            sqlx::query_as("SELECT username FROM users WHERE username = ?1 OR email = ?2")
                .bind(username)
                .bind(email)
                .fetch_optional(&mut *tx)
                .await?;
        if let Some((existing,)) = taken {
            return invalid(if existing.eq_ignore_ascii_case(username) {
                "That username is taken."
            } else {
                "An account with that email already exists."
            });
        }
        let user = sqlx::query_as::<_, User>(concat!(
            "INSERT INTO users (username, email, password_hash, is_admin, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5) RETURNING ",
            user_columns!()
        ))
        .bind(username)
        .bind(email)
        .bind(hash)
        .bind(count == 0)
        .bind(now())
        .fetch_one(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(user)
    }

    /// Checks a username (or email) and password.
    pub async fn authenticate(&self, login: &str, password: &str) -> Result<Option<User>> {
        let user = sqlx::query_as::<_, User>(concat!(
            "SELECT ",
            user_columns!(),
            " FROM users WHERE kind = 'user' AND disabled_at IS NULL AND (username = ?1 OR email = ?1)"
        ))
        .bind(login.trim())
        .fetch_optional(&self.db)
        .await?;
        // Verify against a dummy hash for unknown users so response times
        // don't reveal which accounts exist.
        static DUMMY: OnceLock<String> = OnceLock::new();
        let hash = match &user {
            Some(u) => u.password_hash.clone(),
            None => match DUMMY.get() {
                Some(h) => h.clone(),
                None => {
                    let h = hash_password("dummy password".into()).await?;
                    DUMMY.get_or_init(|| h).clone()
                }
            },
        };
        let ok = verify_password(password.to_owned(), hash).await?;
        Ok(user.filter(|_| ok))
    }

    pub async fn user_by_name(&self, username: &str) -> Result<Option<User>> {
        Ok(sqlx::query_as::<_, User>(concat!(
            "SELECT ",
            user_columns!(),
            " FROM users WHERE username = ?1"
        ))
        .bind(username)
        .fetch_optional(&self.db)
        .await?)
    }

    /// Changes a password after checking the current one, and signs out all
    /// sessions; the caller should start a new one for the current browser.
    pub async fn change_password(&self, user: &User, current: &str, new: &str) -> Result<()> {
        if !verify_password(current.to_owned(), user.password_hash.clone()).await? {
            return invalid("Your current password is incorrect.");
        }
        self.set_password(user.id, new).await
    }

    /// Sets a password and signs out all sessions.
    pub(crate) async fn set_password(&self, user_id: i64, new: &str) -> Result<()> {
        if new.chars().count() < 8 {
            return invalid("Passwords must be at least 8 characters long.");
        }
        let hash = hash_password(new.to_owned()).await?;
        let mut tx = self.db.begin().await?;
        sqlx::query("UPDATE users SET password_hash = ?2 WHERE id = ?1")
            .bind(user_id)
            .bind(hash)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM sessions WHERE user_id = ?1")
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Changes the email address, which then needs verifying again.
    pub async fn change_email(&self, user_id: i64, email: &str) -> Result<()> {
        let email = email.trim();
        if !email.contains('@') || email.len() > 254 {
            return invalid("Please enter a valid email address.");
        }
        let result = sqlx::query(
            "UPDATE users SET email = ?2,
                email_verified_at = CASE WHEN email = ?2 THEN email_verified_at END
             WHERE id = ?1",
        )
            .bind(user_id)
            .bind(email)
            .execute(&self.db)
            .await;
        match result {
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
                invalid("An account with that email already exists.")
            }
            r => r.map(|_| ()).map_err(Into::into),
        }
    }

    pub async fn has_users(&self) -> Result<bool> {
        let row: Option<(i64,)> = sqlx::query_as("SELECT id FROM users WHERE kind = 'user' LIMIT 1")
            .fetch_optional(&self.db)
            .await?;
        Ok(row.is_some())
    }

    pub async fn user_by_id(&self, id: i64) -> Result<Option<User>> {
        Ok(sqlx::query_as::<_, User>(concat!(
            "SELECT ",
            user_columns!(),
            " FROM users WHERE id = ?1"
        ))
        .bind(id)
        .fetch_optional(&self.db)
        .await?)
    }

    /// Starts a session and returns the token to put in the cookie.
    pub async fn create_session(&self, user_id: i64) -> Result<String> {
        let token = new_token()?;
        let now = now();
        sqlx::query(
            "INSERT INTO sessions (token_hash, user_id, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4)",
        )
        .bind(hash_token(&token))
        .bind(user_id)
        .bind(now)
        .bind(now + SESSION_TTL_SECS)
        .execute(&self.db)
        .await?;
        Ok(token)
    }

    pub async fn session_user(&self, token: &str) -> Result<Option<User>> {
        Ok(sqlx::query_as::<_, User>(
            "SELECT u.id, u.username, COALESCE(u.email, '') AS email, u.password_hash, u.is_admin,
                u.kind, u.created_at, u.email_verified_at IS NOT NULL AS email_verified, u.email_notifications,
                u.disabled_at IS NOT NULL AS disabled, u.totp_secret IS NOT NULL AS totp_enabled,
                u.avatar_type, u.avatar_version, u.theme
             FROM sessions s JOIN users u ON u.id = s.user_id
             WHERE s.token_hash = ?1 AND s.expires_at > ?2 AND u.disabled_at IS NULL",
        )
        .bind(hash_token(token))
        .bind(now())
        .fetch_optional(&self.db)
        .await?)
    }

    pub async fn delete_session(&self, token: &str) -> Result<()> {
        sqlx::query("DELETE FROM sessions WHERE token_hash = ?1")
            .bind(hash_token(token))
            .execute(&self.db)
            .await?;
        Ok(())
    }

    pub(crate) async fn delete_expired_sessions(&self) -> Result<()> {
        sqlx::query("DELETE FROM sessions WHERE expires_at <= ?1")
            .bind(now())
            .execute(&self.db)
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::{Core, Error};

    #[tokio::test]
    async fn register_login_session() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "correct horse").await.unwrap();
        assert!(ada.is_admin, "first user is admin");
        let bob = core.register("bob", "bob@example.com", "battery staple").await.unwrap();
        assert!(!bob.is_admin);

        for (name, email) in [("ADA", "x@example.com"), ("carol", "Ada@Example.com")] {
            let err = core.register(name, email, "long enough").await.unwrap_err();
            assert!(matches!(err, Error::Invalid(_)));
        }
        assert!(core.register("carol", "c@example.com", "short").await.is_err());

        assert!(core.authenticate("ada", "wrong password").await.unwrap().is_none());
        assert!(core.authenticate("nobody", "correct horse").await.unwrap().is_none());
        let user = core.authenticate("ada@example.com", "correct horse").await.unwrap().unwrap();
        assert_eq!(user.id, ada.id);

        let token = core.create_session(ada.id).await.unwrap();
        assert_eq!(core.session_user(&token).await.unwrap().unwrap().id, ada.id);
        core.delete_session(&token).await.unwrap();
        assert!(core.session_user(&token).await.unwrap().is_none());

        let token = core.create_session(ada.id).await.unwrap();
        assert!(core.change_password(&ada, "wrong", "new password").await.is_err());
        assert!(core.change_password(&ada, "correct horse", "short").await.is_err());
        core.change_password(&ada, "correct horse", "new password").await.unwrap();
        assert!(core.session_user(&token).await.unwrap().is_none(), "sessions revoked");
        assert!(core.authenticate("ada", "new password").await.unwrap().is_some());

        assert!(core.change_email(ada.id, "bob@example.com").await.is_err());
        core.change_email(ada.id, "lovelace@example.com").await.unwrap();
        assert!(core.authenticate("lovelace@example.com", "new password").await.unwrap().is_some());
    }
}
