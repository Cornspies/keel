use std::sync::OnceLock;

use crate::auth::{hash_password, hash_token, new_token, verify_password};
use crate::{Core, Result, invalid, now, validate_username};

pub const SESSION_TTL_SECS: i64 = 30 * 24 * 3600;

#[derive(Clone, sqlx::FromRow)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub email: String,
    password_hash: String,
    pub is_admin: bool,
    pub created_at: i64,
}

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

// A macro rather than a const so queries stay `&'static str` via `concat!`.
macro_rules! user_columns {
    () => {
        "id, username, email, password_hash, is_admin, created_at"
    };
}

impl Core {
    /// Creates an account. The very first account becomes the administrator
    /// and is allowed even when registration is closed.
    pub async fn register(&self, username: &str, email: &str, password: &str) -> Result<User> {
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
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM users")
            .fetch_one(&mut *tx)
            .await?;
        if count > 0 && !self.config.open_registration {
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
            " FROM users WHERE username = ?1 OR email = ?1"
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

    pub async fn has_users(&self) -> Result<bool> {
        let row: Option<(i64,)> = sqlx::query_as("SELECT id FROM users LIMIT 1")
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
            "SELECT u.id, u.username, u.email, u.password_hash, u.is_admin, u.created_at
             FROM sessions s JOIN users u ON u.id = s.user_id
             WHERE s.token_hash = ?1 AND s.expires_at > ?2",
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
    }
}
