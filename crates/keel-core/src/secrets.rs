//! Secrets keel has to read back, unlike passwords and tokens (which are
//! hashed): two-factor secrets, webhook secrets, mirror passwords and import
//! tokens. They're stored encrypted (AES-256-GCM) with a key that isn't in
//! the database, so a copy of `keel.db` alone, or a dump of it, doesn't give
//! them away. The key is `secret.key` in the data directory by default; put
//! it elsewhere with `--secret-key-file` to keep it out of the data
//! directory's backups too.
//!
//! Stored values look like `enc1:<base64url of nonce and ciphertext>`. The
//! column a value belongs to is authenticated with it, so a value can't be
//! moved to another column (say, a webhook secret shown as something
//! else). Values from before encryption are plain and are encrypted at
//! startup; empty values stay empty, so `password != ''` still means "set".

use std::path::Path;

use aes_gcm::Aes256Gcm;
use aes_gcm::aead::{Aead, KeyInit, Payload};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;

use crate::{Core, Result};

/// The key's name in the data directory, unless `--secret-key-file` says
/// otherwise.
pub const KEY_FILE: &str = "secret.key";
const PREFIX: &str = "enc1:";
const NONCE_LEN: usize = 12;

/// What a secret is for; authenticated with it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Purpose {
    Totp,
    Webhook,
    Mirror,
    PushMirror,
    Import,
}

impl Purpose {
    fn aad(self) -> &'static [u8] {
        match self {
            Purpose::Totp => b"keel totp",
            Purpose::Webhook => b"keel webhook",
            Purpose::Mirror => b"keel mirror",
            Purpose::PushMirror => b"keel push mirror",
            Purpose::Import => b"keel import",
        }
    }
}

pub(crate) struct SecretBox {
    cipher: Aes256Gcm,
}

impl SecretBox {
    /// Reads the key at `path`, or creates it there (readable only by its
    /// owner) if `create` and it doesn't exist.
    pub(crate) fn load(path: &Path, create: bool) -> anyhow::Result<Self> {
        let key = match std::fs::read_to_string(path) {
            Ok(text) => hex::decode(text.trim())
                .ok()
                .filter(|k| k.len() == 32)
                .ok_or_else(|| anyhow::anyhow!("{} isn't a key keel made (64 hex digits)", path.display()))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && create => {
                let mut key = vec![0u8; 32];
                getrandom::fill(&mut key).map_err(|e| anyhow::anyhow!("getrandom: {e}"))?;
                write_private(path, &format!("{}\n", hex::encode(&key)))?;
                key
            }
            Err(e) => return Err(anyhow::anyhow!("reading the secret key {}: {e}", path.display())),
        };
        Ok(Self { cipher: Aes256Gcm::new_from_slice(&key).expect("32-byte key") })
    }

    /// A random key that's never stored, for processes that don't touch
    /// secrets (the `pre-receive` hook).
    pub(crate) fn ephemeral() -> Self {
        let mut key = [0u8; 32];
        let _ = getrandom::fill(&mut key);
        Self { cipher: Aes256Gcm::new_from_slice(&key).expect("32-byte key") }
    }

    pub(crate) fn seal(&self, purpose: Purpose, plain: &str) -> Result<String> {
        if plain.is_empty() {
            return Ok(String::new());
        }
        let mut nonce = [0u8; NONCE_LEN];
        getrandom::fill(&mut nonce).map_err(|e| anyhow::anyhow!("getrandom: {e}"))?;
        let sealed = self
            .cipher
            .encrypt(&nonce.into(), Payload { msg: plain.as_bytes(), aad: purpose.aad() })
            .map_err(|_| anyhow::anyhow!("encrypting a secret failed"))?;
        let mut out = nonce.to_vec();
        out.extend(sealed);
        Ok(format!("{PREFIX}{}", URL_SAFE_NO_PAD.encode(out)))
    }

    /// Decrypts a stored value; plain values (from before encryption) come
    /// back as they are.
    pub(crate) fn open(&self, purpose: Purpose, stored: &str) -> Result<String> {
        let Some(encoded) = stored.strip_prefix(PREFIX) else { return Ok(stored.to_owned()) };
        let bad = || anyhow::anyhow!("a stored secret can't be decrypted: is --secret-key-file the key it was made with?");
        let data = URL_SAFE_NO_PAD.decode(encoded).map_err(|_| bad())?;
        if data.len() < NONCE_LEN {
            return Err(bad().into());
        }
        let (nonce, sealed) = data.split_at(NONCE_LEN);
        let nonce: [u8; NONCE_LEN] = nonce.try_into().expect("split at the nonce length");
        let plain =
            self.cipher.decrypt(&nonce.into(), Payload { msg: sealed, aad: purpose.aad() }).map_err(|_| bad())?;
        Ok(String::from_utf8(plain).map_err(|_| bad())?)
    }
}

fn write_private(path: &Path, content: &str) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)?.write_all(content.as_bytes())
}

// Static SQL per column (see the README: no SQL is built at run time).
macro_rules! encrypt_column {
    ($core:expr, $purpose:expr, $table:literal, $key:literal, $column:literal) => {{
        let rows: Vec<(i64, String)> = sqlx::query_as(concat!(
            "SELECT ", $key, ", ", $column, " FROM ", $table,
            " WHERE ", $column, " != '' AND ", $column, " NOT LIKE 'enc1:%'"
        ))
        .fetch_all(&$core.db)
        .await?;
        for (id, plain) in &rows {
            sqlx::query(concat!("UPDATE ", $table, " SET ", $column, " = ?2 WHERE ", $key, " = ?1"))
                .bind(id)
                .bind($core.secrets.seal($purpose, plain)?)
                .execute(&$core.db)
                .await?;
        }
        rows.len()
    }};
}

impl Core {
    pub(crate) fn seal(&self, purpose: Purpose, plain: &str) -> Result<String> {
        self.secrets.seal(purpose, plain)
    }

    pub(crate) fn unseal(&self, purpose: Purpose, stored: &str) -> Result<String> {
        self.secrets.open(purpose, stored)
    }

    /// Encrypts secrets stored before encryption existed. Returns how many.
    pub(crate) async fn encrypt_plain_secrets(&self) -> Result<usize> {
        Ok(encrypt_column!(self, Purpose::Totp, "users", "id", "totp_secret")
            + encrypt_column!(self, Purpose::Totp, "users", "id", "totp_pending")
            + encrypt_column!(self, Purpose::Webhook, "webhooks", "id", "secret")
            + encrypt_column!(self, Purpose::Mirror, "mirrors", "repo_id", "password")
            + encrypt_column!(self, Purpose::PushMirror, "push_mirrors", "id", "password")
            + encrypt_column!(self, Purpose::Import, "imports", "repo_id", "token"))
    }
}

/// Whether the database holds encrypted secrets (so a missing key must not
/// be replaced by a new one).
pub(crate) async fn any_encrypted(db: &sqlx::SqlitePool) -> Result<bool> {
    let (found,): (bool,) = sqlx::query_as(
        "SELECT EXISTS (SELECT 1 FROM users WHERE totp_secret LIKE 'enc1:%' OR totp_pending LIKE 'enc1:%')
             OR EXISTS (SELECT 1 FROM webhooks WHERE secret LIKE 'enc1:%')
             OR EXISTS (SELECT 1 FROM mirrors WHERE password LIKE 'enc1:%')
             OR EXISTS (SELECT 1 FROM push_mirrors WHERE password LIKE 'enc1:%')
             OR EXISTS (SELECT 1 FROM imports WHERE token LIKE 'enc1:%')",
    )
    .fetch_one(db)
    .await?;
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seals_and_opens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.key");
        assert!(SecretBox::load(&path, false).is_err(), "not created unless asked");
        let b = SecretBox::load(&path, true).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        }
        let sealed = b.seal(Purpose::Webhook, "s3cret").unwrap();
        assert!(sealed.starts_with(PREFIX) && !sealed.contains("s3cret"));
        assert_ne!(sealed, b.seal(Purpose::Webhook, "s3cret").unwrap(), "fresh nonce each time");
        assert_eq!(b.open(Purpose::Webhook, &sealed).unwrap(), "s3cret");
        assert!(b.open(Purpose::Mirror, &sealed).is_err(), "bound to its column");
        assert_eq!(b.open(Purpose::Mirror, "plain").unwrap(), "plain", "from before encryption");
        assert_eq!(b.seal(Purpose::Mirror, "").unwrap(), "", "empty stays empty");
        // The same key loads again; another key can't open it.
        assert_eq!(SecretBox::load(&path, false).unwrap().open(Purpose::Webhook, &sealed).unwrap(), "s3cret");
        let other = SecretBox::load(&dir.path().join("other.key"), true).unwrap();
        assert!(other.open(Purpose::Webhook, &sealed).is_err());
        let mut tampered = sealed.clone();
        tampered.pop();
        assert!(b.open(Purpose::Webhook, &tampered).is_err());
    }

    #[tokio::test]
    async fn upgrades_and_guards_the_key() {
        let (core, dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let repo = core.create_repo(&ada, "engine", "", false).await.unwrap();
        let owner = crate::HookOwner::Repo(repo.id);
        core.create_webhook(owner, "https://ci.example.com/hook", "s3cret", &["push".into()], true).await.unwrap();
        let stored = || async {
            let (s,): (String,) = sqlx::query_as("SELECT secret FROM webhooks").fetch_one(&core.db).await.unwrap();
            s
        };
        assert!(stored().await.starts_with(PREFIX), "stored encrypted");
        // As an earlier version stored it.
        sqlx::query("UPDATE webhooks SET secret = 'plain'").execute(&core.db).await.unwrap();
        assert_eq!(core.webhooks(owner).await.unwrap()[0].secret, "plain", "readable meanwhile");
        assert_eq!(core.encrypt_plain_secrets().await.unwrap(), 1);
        assert!(stored().await.starts_with(PREFIX));
        assert_eq!(core.webhooks(owner).await.unwrap()[0].secret, "plain");
        assert_eq!(core.encrypt_plain_secrets().await.unwrap(), 0);

        // Without its key, the data directory doesn't open (rather than
        // getting a new key that can't read the secrets).
        std::fs::remove_file(dir.path().join(KEY_FILE)).unwrap();
        let config = crate::Config { data_dir: dir.path().to_owned(), ..crate::Config::default() };
        let e = Core::open(config).await.err().unwrap().to_string();
        assert!(e.contains("is missing"), "{e}");
    }
}
