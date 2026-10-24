//! Two-factor authentication with time-based one-time passwords (TOTP,
//! RFC 6238: HMAC-SHA1, 30-second steps, 6 digits, as every authenticator
//! app supports), plus single-use recovery codes.
//!
//! Signing in with a password then takes a second step: the password check
//! starts a short-lived login challenge, which a code completes.

use sha1::{Digest, Sha1};

use crate::auth::{hash_token, new_token};
use crate::{Core, Result, User, invalid, now};

const STEP: i64 = 30;
pub const RECOVERY_CODES: usize = 10;
/// How long the second sign-in step may take.
const CHALLENGE_SECS: i64 = 600;
/// Wrong codes allowed per challenge, and per account in `LOCK_SECS`.
const CHALLENGE_ATTEMPTS: i64 = 5;
const MAX_FAILURES: i64 = 10;
const LOCK_SECS: i64 = 900;

const BASE32: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";

pub fn base32_encode(data: &[u8]) -> String {
    let mut out = String::new();
    for chunk in data.chunks(5) {
        let mut buf = [0u8; 5];
        buf[..chunk.len()].copy_from_slice(chunk);
        let bits = u64::from_be_bytes([0, 0, 0, buf[0], buf[1], buf[2], buf[3], buf[4]]);
        let chars = (chunk.len() * 8).div_ceil(5);
        for i in 0..chars {
            out.push(BASE32[((bits >> (35 - i * 5)) & 31) as usize] as char);
        }
    }
    out
}

fn base32_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u64, 0u32);
    for c in s.bytes().filter(|c| !matches!(c, b'=' | b' ')) {
        let v = BASE32.iter().position(|&b| b == c.to_ascii_uppercase())? as u64;
        acc = (acc << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

fn hmac_sha1(key: &[u8], msg: &[u8]) -> [u8; 20] {
    const BLOCK: usize = 64;
    let mut k = [0u8; BLOCK];
    if key.len() > BLOCK {
        k[..20].copy_from_slice(&Sha1::digest(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let pad = |byte: u8| k.map(|b| b ^ byte);
    let inner = Sha1::new().chain_update(pad(0x36)).chain_update(msg).finalize();
    Sha1::new().chain_update(pad(0x5c)).chain_update(inner).finalize().into()
}

/// The 6-digit code for time step `step` (Unix time / 30).
pub fn totp_code(secret: &[u8], step: i64) -> String {
    let mac = hmac_sha1(secret, &(step as u64).to_be_bytes());
    let offset = (mac[19] & 0x0f) as usize;
    let n = u32::from_be_bytes([mac[offset] & 0x7f, mac[offset + 1], mac[offset + 2], mac[offset + 3]]);
    format!("{:06}", n % 1_000_000)
}

/// What authenticator apps scan, as a QR code.
pub fn totp_uri(issuer: &str, account: &str, secret: &str) -> String {
    let enc = |s: &str| form_urlencoded::byte_serialize(s.as_bytes()).collect::<String>().replace('+', "%20");
    format!(
        "otpauth://totp/{}:{}?secret={secret}&issuer={}&algorithm=SHA1&digits=6&period=30",
        enc(issuer),
        enc(account),
        enc(issuer)
    )
}

fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Recovery codes as typed: lowercase, without dashes and spaces.
fn normalize_recovery(code: &str) -> String {
    code.chars().filter(|c| c.is_ascii_alphanumeric()).collect::<String>().to_ascii_lowercase()
}

/// The outcome of the second sign-in step.
#[derive(Debug)]
pub enum SecondStep {
    SignedIn(User),
    WrongCode,
    /// The challenge is unknown, used up or too old: sign in again.
    Expired,
    /// Too many wrong codes lately.
    Locked,
}

impl Core {
    /// Starts setting up an authenticator: a new secret, which
    /// [`enable_totp`](Self::enable_totp) confirms.
    pub async fn begin_totp_setup(&self, user: &User) -> Result<String> {
        let mut secret = [0u8; 20];
        getrandom::fill(&mut secret).map_err(|e| anyhow::anyhow!("getrandom: {e}"))?;
        let secret = base32_encode(&secret);
        sqlx::query("UPDATE users SET totp_pending = ?2 WHERE id = ?1")
            .bind(user.id)
            .bind(&secret)
            .execute(&self.db)
            .await?;
        Ok(secret)
    }

    pub async fn pending_totp(&self, user_id: i64) -> Result<Option<String>> {
        let row: Option<(Option<String>,)> = sqlx::query_as("SELECT totp_pending FROM users WHERE id = ?1")
            .bind(user_id)
            .fetch_optional(&self.db)
            .await?;
        Ok(row.and_then(|(p,)| p))
    }

    /// Turns two-factor authentication on once `code` shows the
    /// authenticator has the pending secret. Returns recovery codes.
    pub async fn enable_totp(&self, user: &User, code: &str) -> Result<Vec<String>> {
        let Some(secret) = self.pending_totp(user.id).await? else {
            return invalid("Start the setup again.");
        };
        let key = base32_decode(&secret).unwrap_or_default();
        let step = now() / STEP;
        let code = code.trim().replace(' ', "");
        let Some(matched) = (step - 1..=step + 1).find(|s| same(&totp_code(&key, *s), &code)) else {
            return invalid("That code isn't right. Check the time on your phone and try the current code.");
        };
        sqlx::query(
            "UPDATE users SET totp_secret = totp_pending, totp_pending = NULL, totp_last_step = ?2 WHERE id = ?1",
        )
        .bind(user.id)
        .bind(matched)
        .execute(&self.db)
        .await?;
        self.new_recovery_codes(user.id).await
    }

    /// Turns two-factor authentication off (callers check the password).
    pub async fn disable_totp(&self, user_id: i64) -> Result<()> {
        let mut tx = self.db.begin().await?;
        sqlx::query("UPDATE users SET totp_secret = NULL, totp_pending = NULL, totp_last_step = 0 WHERE id = ?1")
            .bind(user_id)
            .execute(&mut *tx)
            .await?;
        sqlx::query("DELETE FROM recovery_codes WHERE user_id = ?1").bind(user_id).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM login_challenges WHERE user_id = ?1").bind(user_id).execute(&mut *tx).await?;
        tx.commit().await?;
        Ok(())
    }

    /// Replaces the recovery codes, returning the new ones (shown once).
    pub async fn new_recovery_codes(&self, user_id: i64) -> Result<Vec<String>> {
        let mut codes = Vec::new();
        let mut tx = self.db.begin().await?;
        sqlx::query("DELETE FROM recovery_codes WHERE user_id = ?1").bind(user_id).execute(&mut *tx).await?;
        for _ in 0..RECOVERY_CODES {
            let mut bytes = [0u8; 7];
            getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("getrandom: {e}"))?;
            let raw = base32_encode(&bytes)[..10].to_ascii_lowercase();
            sqlx::query("INSERT INTO recovery_codes (user_id, code_hash) VALUES (?1, ?2)")
                .bind(user_id)
                .bind(hash_token(&raw))
                .execute(&mut *tx)
                .await?;
            codes.push(format!("{}-{}", &raw[..5], &raw[5..]));
        }
        tx.commit().await?;
        Ok(codes)
    }

    pub async fn recovery_codes_left(&self, user_id: i64) -> Result<i64> {
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM recovery_codes WHERE user_id = ?1 AND used_at IS NULL")
            .bind(user_id)
            .fetch_one(&self.db)
            .await?;
        Ok(n)
    }

    /// Checks a code from the authenticator (each can be used once) or a
    /// recovery code (used up by this).
    pub async fn verify_second_factor(&self, user_id: i64, code: &str) -> Result<bool> {
        let code = code.trim().replace(' ', "");
        if code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit()) {
            let row: Option<(Option<String>,)> = sqlx::query_as("SELECT totp_secret FROM users WHERE id = ?1")
                .bind(user_id)
                .fetch_optional(&self.db)
                .await?;
            let Some(key) = row.and_then(|(s,)| s).and_then(|s| base32_decode(&s)) else {
                return Ok(false);
            };
            let step = now() / STEP;
            for s in step - 1..=step + 1 {
                if same(&totp_code(&key, s), &code) {
                    // Only if no later (or this) code was used already.
                    let done = sqlx::query("UPDATE users SET totp_last_step = ?2 WHERE id = ?1 AND totp_last_step < ?2")
                        .bind(user_id)
                        .bind(s)
                        .execute(&self.db)
                        .await?;
                    return Ok(done.rows_affected() == 1);
                }
            }
            return Ok(false);
        }
        let used = sqlx::query(
            "UPDATE recovery_codes SET used_at = ?3 WHERE user_id = ?1 AND code_hash = ?2 AND used_at IS NULL",
        )
        .bind(user_id)
        .bind(hash_token(&normalize_recovery(&code)))
        .bind(now())
        .execute(&self.db)
        .await?;
        Ok(used.rows_affected() == 1)
    }

    /// After the password: a token for the second step.
    pub async fn start_login_challenge(&self, user_id: i64) -> Result<String> {
        let token = new_token()?;
        let now = now();
        sqlx::query("DELETE FROM login_challenges WHERE expires_at < ?1").bind(now).execute(&self.db).await?;
        sqlx::query("INSERT INTO login_challenges (token_hash, user_id, created_at, expires_at) VALUES (?1, ?2, ?3, ?4)")
            .bind(hash_token(&token))
            .bind(user_id)
            .bind(now)
            .bind(now + CHALLENGE_SECS)
            .execute(&self.db)
            .await?;
        Ok(token)
    }

    /// Whether a challenge is still open.
    pub async fn login_challenge_valid(&self, token: &str) -> Result<bool> {
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM login_challenges WHERE token_hash = ?1 AND expires_at > ?2")
            .bind(hash_token(token))
            .bind(now())
            .fetch_one(&self.db)
            .await?;
        Ok(n > 0)
    }

    /// Completes a sign-in with the second factor.
    pub async fn complete_login_challenge(&self, token: &str, code: &str) -> Result<SecondStep> {
        let now = now();
        let row: Option<(i64, i64)> = sqlx::query_as(
            "SELECT user_id, attempts FROM login_challenges WHERE token_hash = ?1 AND expires_at > ?2",
        )
        .bind(hash_token(token))
        .bind(now)
        .fetch_optional(&self.db)
        .await?;
        let Some((user_id, attempts)) = row else { return Ok(SecondStep::Expired) };
        if attempts >= CHALLENGE_ATTEMPTS {
            return Ok(SecondStep::Expired);
        }
        let (failures,): (i64,) =
            sqlx::query_as("SELECT COALESCE(SUM(attempts), 0) FROM login_challenges WHERE user_id = ?1 AND created_at > ?2")
                .bind(user_id)
                .bind(now - LOCK_SECS)
                .fetch_one(&self.db)
                .await?;
        if failures >= MAX_FAILURES {
            return Ok(SecondStep::Locked);
        }
        if !self.verify_second_factor(user_id, code).await? {
            sqlx::query("UPDATE login_challenges SET attempts = attempts + 1 WHERE token_hash = ?1")
                .bind(hash_token(token))
                .execute(&self.db)
                .await?;
            return Ok(SecondStep::WrongCode);
        }
        sqlx::query("DELETE FROM login_challenges WHERE token_hash = ?1").bind(hash_token(token)).execute(&self.db).await?;
        Ok(match self.user_by_id(user_id).await?.filter(|u| !u.disabled) {
            Some(user) => SecondStep::SignedIn(user),
            None => SecondStep::Expired,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc_6238_vectors() {
        let key = b"12345678901234567890";
        // The RFC's 8-digit values, last six digits.
        assert_eq!(totp_code(key, 59 / 30), "287082");
        assert_eq!(totp_code(key, 1111111109 / 30), "081804");
        assert_eq!(totp_code(key, 1234567890 / 30), "005924");
        assert_eq!(base32_encode(b"foobar"), "MZXW6YTBOI");
        assert_eq!(base32_decode("MZXW6YTBOI").unwrap(), b"foobar");
        assert_eq!(base32_decode(&base32_encode(key)).unwrap(), key);
        assert_eq!(
            totp_uri("keel dev", "ada", "ABC"),
            "otpauth://totp/keel%20dev:ada?secret=ABC&issuer=keel%20dev&algorithm=SHA1&digits=6&period=30"
        );
    }

    #[tokio::test]
    async fn setup_and_sign_in() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        assert!(!ada.totp_enabled);
        let code_now = |secret: &str| totp_code(&base32_decode(secret).unwrap(), now() / STEP);

        let secret = core.begin_totp_setup(&ada).await.unwrap();
        assert_eq!(secret.len(), 32);
        assert!(core.enable_totp(&ada, "000000").await.is_err() || code_now(&secret) == "000000");
        let codes = core.enable_totp(&ada, &code_now(&secret)).await.unwrap();
        assert_eq!(codes.len(), RECOVERY_CODES);
        assert!(core.user_by_id(ada.id).await.unwrap().unwrap().totp_enabled);
        assert_eq!(core.pending_totp(ada.id).await.unwrap(), None);

        // The code used for setup can't be used again; the next one can.
        assert!(!core.verify_second_factor(ada.id, &code_now(&secret)).await.unwrap(), "replayed");
        let next = totp_code(&base32_decode(&secret).unwrap(), now() / STEP + 1);
        assert!(core.verify_second_factor(ada.id, &next).await.unwrap());

        // Recovery codes work once, typed however.
        assert!(core.verify_second_factor(ada.id, &codes[0].to_uppercase().replace('-', " ")).await.unwrap());
        assert!(!core.verify_second_factor(ada.id, &codes[0]).await.unwrap());
        assert_eq!(core.recovery_codes_left(ada.id).await.unwrap(), 9);

        // Sign-in challenges.
        let token = core.start_login_challenge(ada.id).await.unwrap();
        assert!(core.login_challenge_valid(&token).await.unwrap());
        assert!(matches!(core.complete_login_challenge(&token, "12345").await.unwrap(), SecondStep::WrongCode));
        match core.complete_login_challenge(&token, &codes[1]).await.unwrap() {
            SecondStep::SignedIn(u) => assert_eq!(u.id, ada.id),
            other => panic!("{other:?}"),
        }
        assert!(matches!(core.complete_login_challenge(&token, &codes[2]).await.unwrap(), SecondStep::Expired), "used up");

        // Five wrong codes end a challenge; ten lock the account for a while.
        let t = core.start_login_challenge(ada.id).await.unwrap();
        for _ in 0..5 {
            core.complete_login_challenge(&t, "000001").await.unwrap();
        }
        assert!(matches!(core.complete_login_challenge(&t, &codes[2]).await.unwrap(), SecondStep::Expired));
        let t = core.start_login_challenge(ada.id).await.unwrap();
        for _ in 0..5 {
            core.complete_login_challenge(&t, "000001").await.unwrap();
        }
        let t = core.start_login_challenge(ada.id).await.unwrap();
        assert!(matches!(core.complete_login_challenge(&t, &codes[2]).await.unwrap(), SecondStep::Locked));

        core.disable_totp(ada.id).await.unwrap();
        assert!(!core.user_by_id(ada.id).await.unwrap().unwrap().totp_enabled);
        assert_eq!(core.recovery_codes_left(ada.id).await.unwrap(), 0);
    }
}
