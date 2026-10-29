use argon2::Argon2;
use argon2::password_hash::phc::PasswordHash;
use argon2::password_hash::{PasswordHasher, PasswordVerifier};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

/// Password hashes running at once: one per CPU. Each takes about 19 MiB
/// and tens of milliseconds, and without a limit a flood of sign-in attempts
/// (which needs no account) would run hundreds at once on the blocking pool
/// and exhaust memory. Excess ones wait their turn instead.
fn hashing() -> &'static tokio::sync::Semaphore {
    static PERMITS: std::sync::OnceLock<tokio::sync::Semaphore> = std::sync::OnceLock::new();
    PERMITS.get_or_init(|| tokio::sync::Semaphore::new(hashing_permits()))
}

fn hashing_permits() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get())
}

/// Argon2id is deliberately slow, so it runs on the blocking pool.
pub(crate) async fn hash_password(password: String) -> anyhow::Result<String> {
    let _permit = hashing().acquire().await?;
    tokio::task::spawn_blocking(move || {
        Argon2::default()
            .hash_password(password.as_bytes())
            .map(|h| h.to_string())
            .map_err(|e| anyhow::anyhow!("hashing password: {e}"))
    })
    .await?
}

pub(crate) async fn verify_password(password: String, hash: String) -> anyhow::Result<bool> {
    let _permit = hashing().acquire().await?;
    tokio::task::spawn_blocking(move || {
        let parsed = PasswordHash::new(&hash).map_err(|e| anyhow::anyhow!("bad hash: {e}"))?;
        Ok(Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok())
    })
    .await?
}

/// A random 256-bit token, URL-safe base64 encoded.
pub(crate) fn new_token() -> anyhow::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("getrandom: {e}"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

pub(crate) fn hash_token(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn hashing_waits_for_a_turn() {
        let hash = hash_password("correct horse".into()).await.unwrap();
        assert!(verify_password("correct horse".into(), hash.clone()).await.unwrap());
        // With every permit taken, a check waits rather than running.
        let all = hashing().acquire_many(hashing_permits() as u32).await.unwrap();
        let waiting = tokio::spawn(verify_password("wrong".into(), hash));
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(!waiting.is_finished());
        drop(all);
        assert!(!waiting.await.unwrap().unwrap());
    }
}
