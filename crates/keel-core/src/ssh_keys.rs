use base64::Engine;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use sha2::{Digest, Sha256};

use crate::{Core, Error, Result, User, invalid, now};

const ALLOWED_TYPES: &[&str] = &[
    "ssh-ed25519",
    "ssh-rsa",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384",
    "ecdsa-sha2-nistp521",
    "sk-ssh-ed25519@openssh.com",
    "sk-ecdsa-sha2-nistp256@openssh.com",
];

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct SshKey {
    pub id: i64,
    pub user_id: i64,
    pub name: String,
    pub fingerprint: String,
    pub public_key: String,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

/// A parsed `authorized_keys`-style line.
#[derive(Debug, PartialEq, Eq)]
pub struct ParsedKey {
    /// `<type> <base64>`, without the comment.
    pub normalized: String,
    pub comment: String,
    /// `SHA256:<unpadded base64>` like `ssh-keygen -l`.
    pub fingerprint: String,
}

pub fn parse_public_key(line: &str) -> Result<ParsedKey> {
    let mut parts = line.split_whitespace();
    let (Some(kind), Some(data)) = (parts.next(), parts.next()) else {
        return invalid("That doesn’t look like an SSH public key (expected “ssh-ed25519 AAAA…”).");
    };
    if !ALLOWED_TYPES.contains(&kind) {
        return invalid(format!("Unsupported key type “{kind}”."));
    }
    let Ok(blob) = STANDARD.decode(data) else {
        return invalid("The key data is not valid base64.");
    };
    // The blob starts with the key type as a length-prefixed string.
    let declared = blob
        .get(..4)
        .map(|n| u32::from_be_bytes(n.try_into().unwrap()) as usize)
        .and_then(|n| blob.get(4..4 + n));
    if declared != Some(kind.as_bytes()) {
        return invalid("The key data does not match its type.");
    }
    Ok(ParsedKey {
        normalized: format!("{kind} {data}"),
        comment: parts.collect::<Vec<_>>().join(" "),
        fingerprint: format!("SHA256:{}", STANDARD_NO_PAD.encode(Sha256::digest(&blob))),
    })
}

impl Core {
    pub async fn add_ssh_key(&self, user_id: i64, name: &str, key: &str) -> Result<SshKey> {
        let parsed = parse_public_key(key)?;
        if self.deploy_key_exists(&parsed.fingerprint).await? {
            return invalid("This key is a repository's deploy key. Use another key for your account.");
        }
        let name = match name.trim() {
            "" if parsed.comment.is_empty() => "SSH key",
            "" => parsed.comment.as_str(),
            n => n,
        };
        let result = sqlx::query_as::<_, SshKey>(
            "INSERT INTO ssh_keys (user_id, name, fingerprint, public_key, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5) RETURNING *",
        )
        .bind(user_id)
        .bind(name)
        .bind(&parsed.fingerprint)
        .bind(&parsed.normalized)
        .bind(now())
        .fetch_one(&self.db)
        .await;
        match result {
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
                invalid("This key is already in use.")
            }
            r => Ok(r?),
        }
    }

    pub async fn ssh_keys(&self, user_id: i64) -> Result<Vec<SshKey>> {
        Ok(
            sqlx::query_as("SELECT * FROM ssh_keys WHERE user_id = ?1 ORDER BY created_at")
                .bind(user_id)
                .fetch_all(&self.db)
                .await?,
        )
    }

    pub async fn delete_ssh_key(&self, user_id: i64, key_id: i64) -> Result<()> {
        let done = sqlx::query("DELETE FROM ssh_keys WHERE id = ?1 AND user_id = ?2")
            .bind(key_id)
            .bind(user_id)
            .execute(&self.db)
            .await?;
        if done.rows_affected() == 0 {
            return Err(Error::NotFound);
        }
        Ok(())
    }

    pub async fn ssh_key_exists(&self, fingerprint: &str) -> Result<bool> {
        let row: Option<(i64,)> = sqlx::query_as("SELECT id FROM ssh_keys WHERE fingerprint = ?1")
            .bind(fingerprint)
            .fetch_optional(&self.db)
            .await?;
        Ok(row.is_some())
    }

    /// Finds the owner of a key by fingerprint and records that it was used.
    pub async fn user_by_ssh_fingerprint(&self, fingerprint: &str) -> Result<Option<User>> {
        let row: Option<(i64,)> = sqlx::query_as(
            "UPDATE ssh_keys SET last_used_at = ?2 WHERE fingerprint = ?1 RETURNING user_id",
        )
        .bind(fingerprint)
        .bind(now())
        .fetch_optional(&self.db)
        .await?;
        let Some((user_id,)) = row else {
            return Ok(None);
        };
        Ok(self.user_by_id(user_id).await?.filter(|u| !u.disabled))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Generated with `ssh-keygen -t ed25519`; fingerprint from `ssh-keygen -l`.
    const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAICJPFh5LMLXpWKF2eX226t5YJwrn6uebhJG4vjuqawds ada@laptop";
    const FINGERPRINT: &str = "SHA256:ToiPQbjHyCmXO/EZaL9k5PIVsPHQExG47zuWe2Y7hxU";

    #[test]
    fn parses_keys() {
        let k = parse_public_key(KEY).unwrap();
        assert_eq!(k.comment, "ada@laptop");
        assert!(k.normalized.ends_with("qawds"));
        assert_eq!(k.fingerprint, FINGERPRINT);

        assert!(parse_public_key(&KEY.replace("ssh-ed25519", "ssh-rsa")).is_err());
        assert!(parse_public_key("ssh-dss AAAA").is_err());
        assert!(parse_public_key("garbage").is_err());
    }

    #[tokio::test]
    async fn key_lifecycle() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let bob = core.register("bob", "bob@example.com", "password1").await.unwrap();
        let key = core.add_ssh_key(ada.id, "", KEY).await.unwrap();
        assert_eq!(key.name, "ada@laptop");
        assert!(core.add_ssh_key(bob.id, "dup", KEY).await.is_err());

        let found = core.user_by_ssh_fingerprint(FINGERPRINT).await.unwrap().unwrap();
        assert_eq!(found.id, ada.id);
        assert!(core.ssh_keys(ada.id).await.unwrap()[0].last_used_at.is_some());

        assert!(core.delete_ssh_key(bob.id, key.id).await.is_err());
        core.delete_ssh_key(ada.id, key.id).await.unwrap();
        assert!(core.user_by_ssh_fingerprint(FINGERPRINT).await.unwrap().is_none());
    }
}
