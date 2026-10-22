//! Personal access tokens, used instead of passwords for git over HTTP.
//!
//! Tokens look like `keel_<43 base64url chars>`. The prefix lets the server
//! tell a token from a password without a database lookup, and secret
//! scanners recognise leaked tokens.

use crate::auth::{hash_token, new_token};
use crate::{Core, Error, Result, User, invalid, now};

pub const TOKEN_PREFIX: &str = "keel_";

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct AccessToken {
    pub id: i64,
    pub user_id: i64,
    pub name: String,
    pub hint: String,
    pub can_write: bool,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
    pub expires_at: Option<i64>,
}

impl AccessToken {
    pub fn is_expired(&self) -> bool {
        self.expires_at.is_some_and(|t| t <= now())
    }
}

pub fn looks_like_token(s: &str) -> bool {
    s.starts_with(TOKEN_PREFIX)
}

impl Core {
    /// Creates a token and returns it together with the secret, which is
    /// shown to the user once and cannot be retrieved later.
    pub async fn create_token(
        &self,
        user_id: i64,
        name: &str,
        can_write: bool,
        expires_in_days: Option<i64>,
    ) -> Result<(AccessToken, String)> {
        let name = name.trim();
        if name.is_empty() || name.len() > 100 {
            return invalid("Give the token a name (up to 100 characters).");
        }
        let secret = format!("{TOKEN_PREFIX}{}", new_token()?);
        let hint = secret[secret.len() - 4..].to_owned();
        let now = now();
        let token = sqlx::query_as::<_, AccessToken>(
            "INSERT INTO access_tokens (user_id, name, token_hash, hint, can_write, created_at, expires_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             RETURNING id, user_id, name, hint, can_write, created_at, last_used_at, expires_at",
        )
        .bind(user_id)
        .bind(name)
        .bind(hash_token(&secret))
        .bind(hint)
        .bind(can_write)
        .bind(now)
        .bind(expires_in_days.map(|d| now + d * 86400))
        .fetch_one(&self.db)
        .await?;
        Ok((token, secret))
    }

    pub async fn tokens(&self, user_id: i64) -> Result<Vec<AccessToken>> {
        Ok(sqlx::query_as(
            "SELECT id, user_id, name, hint, can_write, created_at, last_used_at, expires_at
             FROM access_tokens WHERE user_id = ?1 ORDER BY created_at DESC",
        )
        .bind(user_id)
        .fetch_all(&self.db)
        .await?)
    }

    pub async fn delete_token(&self, user_id: i64, token_id: i64) -> Result<()> {
        let done = sqlx::query("DELETE FROM access_tokens WHERE id = ?1 AND user_id = ?2")
            .bind(token_id)
            .bind(user_id)
            .execute(&self.db)
            .await?;
        if done.rows_affected() == 0 {
            return Err(Error::NotFound);
        }
        Ok(())
    }

    /// Resolves a token to its owner and whether it may push. Unknown and
    /// expired tokens give `None`. This is a single indexed lookup, unlike a
    /// password check, which deliberately takes tens of milliseconds.
    pub async fn authenticate_token(&self, secret: &str) -> Result<Option<(User, bool)>> {
        if !looks_like_token(secret) {
            return Ok(None);
        }
        let now = now();
        let row: Option<(i64, bool)> = sqlx::query_as(
            "UPDATE access_tokens SET last_used_at = ?2
             WHERE token_hash = ?1 AND (expires_at IS NULL OR expires_at > ?2)
             RETURNING user_id, can_write",
        )
        .bind(hash_token(secret))
        .bind(now)
        .fetch_optional(&self.db)
        .await?;
        let Some((user_id, can_write)) = row else {
            return Ok(None);
        };
        Ok(self.user_by_id(user_id).await?.map(|u| (u, can_write)))
    }
}

#[cfg(test)]
mod tests {
    use crate::Core;

    #[tokio::test]
    async fn token_lifecycle() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let bob = core.register("bob", "bob@example.com", "password1").await.unwrap();

        assert!(core.create_token(ada.id, " ", false, None).await.is_err());
        let (token, secret) = core.create_token(ada.id, "laptop", true, Some(30)).await.unwrap();
        assert!(secret.starts_with("keel_") && secret.len() > 40);
        assert!(secret.ends_with(&token.hint));

        let (user, can_write) = core.authenticate_token(&secret).await.unwrap().unwrap();
        assert_eq!((user.id, can_write), (ada.id, true));
        assert!(core.tokens(ada.id).await.unwrap()[0].last_used_at.is_some());
        assert!(core.authenticate_token("keel_wrong").await.unwrap().is_none());
        assert!(core.authenticate_token("not-a-token").await.unwrap().is_none());

        // Expired tokens stop working.
        let (_, old) = core.create_token(ada.id, "old", false, Some(-1)).await.unwrap();
        assert!(core.authenticate_token(&old).await.unwrap().is_none());

        assert!(core.delete_token(bob.id, token.id).await.is_err());
        core.delete_token(ada.id, token.id).await.unwrap();
        assert!(core.authenticate_token(&secret).await.unwrap().is_none());
    }
}
