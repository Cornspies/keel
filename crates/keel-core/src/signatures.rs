//! Verified commits: SSH signatures are checked against the SSH keys people
//! added, OpenPGP signatures against their GPG keys. A commit is verified
//! when its signature is good, made with someone's key, and its committer
//! email is that person's (verified) email.

use std::collections::HashMap;

use keel_git::{Oid, SignedCommit};
use pgp::composed::{Deserializable, DetachedSignature, SignedPublicKey, SignedPublicSubKey};
use pgp::packet::SignatureType;
use pgp::types::KeyDetails;

use crate::{Core, Error, Result, invalid, now};

/// Largest GPG key accepted, armored.
const MAX_GPG_KEY: usize = 64 * 1024;

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct GpgKey {
    pub id: i64,
    pub user_id: i64,
    /// Of the primary key, upper-case hex.
    pub fingerprint: String,
    /// The key's user ID emails, one per line.
    pub emails: String,
    pub public_key: String,
    pub expires_at: Option<i64>,
    pub created_at: i64,
}

impl GpgKey {
    /// The long key ID, as `gpg` shows it.
    pub fn key_id(&self) -> &str {
        &self.fingerprint[self.fingerprint.len().saturating_sub(16)..]
    }

    pub fn emails(&self) -> impl Iterator<Item = &str> {
        self.emails.lines()
    }
}

/// What an uploaded GPG key holds.
#[derive(Debug, PartialEq, Eq)]
pub struct ParsedGpgKey {
    pub fingerprint: String,
    /// Long key IDs and fingerprints of the primary key and its subkeys.
    pub ids: Vec<String>,
    pub emails: Vec<String>,
    pub expires_at: Option<i64>,
}

/// Parses an ASCII-armored OpenPGP public key, checking its
/// self-signatures (so nobody can tack on someone else's email).
pub fn parse_gpg_key(armored: &str) -> Result<ParsedGpgKey> {
    if armored.len() > MAX_GPG_KEY {
        return invalid("That key is too large.");
    }
    let Ok((key, _)) = SignedPublicKey::from_string(armored.trim()) else {
        return invalid("That doesn’t look like a GPG public key (expected “-----BEGIN PGP PUBLIC KEY BLOCK-----”).");
    };
    if key.verify_bindings().is_err() {
        return invalid("The key’s self-signatures don’t verify.");
    }
    if !key.details.revocation_signatures.is_empty() {
        return invalid("This key has been revoked.");
    }
    let fingerprint = format!("{:X}", key.primary_key.fingerprint());
    let mut ids = vec![hex::encode_upper(key.primary_key.legacy_key_id()), fingerprint.clone()];
    // Signatures from other subkeys are then from an unknown key.
    for sub in key.public_subkeys.iter().filter(|s| subkey_state(s, now()) == SubkeyState::Usable) {
        ids.push(hex::encode_upper(sub.key.legacy_key_id()));
        ids.push(format!("{:X}", sub.key.fingerprint()));
    }
    ids.sort_unstable();
    ids.dedup();
    let mut emails: Vec<String> = key
        .details
        .users
        .iter()
        .filter_map(|u| {
            let id = String::from_utf8_lossy(u.id.id()).into_owned();
            let email = match (id.rfind('<'), id.rfind('>')) {
                (Some(a), Some(b)) if a < b => id[a + 1..b].to_owned(),
                _ if id.contains('@') && !id.contains(' ') => id,
                _ => return None,
            };
            Some(email.trim().to_lowercase())
        })
        .collect();
    emails.sort_unstable();
    emails.dedup();
    let created = i64::from(key.primary_key.created_at().as_secs());
    let expires_at = key
        .details
        .users
        .iter()
        .flat_map(|u| &u.signatures)
        .filter_map(|s| s.key_expiration_time())
        .map(|d| created + i64::from(d.as_secs()))
        .max();
    Ok(ParsedGpgKey { fingerprint, ids, emails, expires_at })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SubkeyState {
    Usable,
    Expired,
    /// Revoked, or not for signing.
    Unusable,
}

/// Whether `sub` can make signatures at time `at`, going by its latest
/// binding signature.
fn subkey_state(sub: &SignedPublicSubKey, at: i64) -> SubkeyState {
    if sub.signatures.iter().any(|s| s.typ() == Some(SignatureType::SubkeyRevocation)) {
        return SubkeyState::Unusable;
    }
    let binding = sub
        .signatures
        .iter()
        .filter(|s| s.typ() == Some(SignatureType::SubkeyBinding))
        .max_by_key(|s| s.created().map(|t| t.as_secs()));
    let Some(binding) = binding else { return SubkeyState::Unusable };
    if !binding.key_flags().sign() {
        return SubkeyState::Unusable;
    }
    let created = i64::from(sub.key.created_at().as_secs());
    match binding.key_expiration_time().map(|d| i64::from(d.as_secs())) {
        Some(d) if d > 0 && at > created + d => SubkeyState::Expired,
        _ => SubkeyState::Usable,
    }
}

/// Whether a signed commit is verified, and if not, why.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verification {
    pub verified: bool,
    /// GitHub's reason codes: `valid`, `unknown_key`, `invalid`,
    /// `bad_email`, `expired_key`, `unknown_signature_type` or
    /// `malformed_signature`.
    pub reason: &'static str,
    /// `SSH`, `GPG` or `X.509`.
    pub kind: &'static str,
    /// The SSH key's fingerprint or the GPG key's ID.
    pub key: String,
    /// Whose key it is, if keel knows the key.
    pub signer: Option<String>,
    /// Signed with the server's own key, as commits keel makes are: the
    /// site's name.
    pub made_on: Option<String>,
}

impl Verification {
    /// One sentence for people.
    pub fn describe(&self) -> String {
        let signer = self.signer.as_deref().unwrap_or("someone");
        let kind = self.kind;
        let key = &self.key;
        if let Some(site) = self.made_on.as_ref().filter(|_| self.verified) {
            return format!("Made on {site} and signed with its key {key}.");
        }
        match self.reason {
            "valid" => format!("Signed with {signer}’s {kind} key {key}."),
            "unknown_key" => format!("Signed with {kind} key {key}, which nobody here has added."),
            "invalid" => format!("The {kind} signature doesn’t match the commit."),
            "bad_email" if kind == "GPG" => format!(
                "Signed with {signer}’s {kind} key {key}, but the committer email isn’t their verified email, or isn’t on the key."
            ),
            "bad_email" => format!("Signed with {signer}’s {kind} key {key}, but the committer email isn’t their verified email."),
            "expired_key" => format!("Signed with {signer}’s {kind} key {key} after it expired."),
            "unknown_signature_type" => format!("Signed with an {kind} signature, which keel doesn’t check."),
            _ => "The signature can’t be read.".to_owned(),
        }
    }
}

/// Whether signatures check out against keys, by commit and key (SSH: the
/// fingerprint; GPG: the stored key's fingerprint): which can't change,
/// and the cryptography is the slow part (RSA especially). `Some` holds
/// the key ID that made a good signature. Who owns a key, and their
/// email, are looked up each time.
#[derive(Default)]
pub(crate) struct SignatureCache(std::sync::Mutex<HashMap<(Oid, String), Option<String>>>);

/// Entries kept before the cache starts over.
const SIGNATURE_CACHE_SIZE: usize = 50_000;

impl SignatureCache {
    fn get_or(&self, commit: Oid, key: &str, check: impl FnOnce() -> Option<String>) -> Option<String> {
        let id = (commit, key.to_owned());
        if let Some(found) = self.0.lock().expect("not poisoned").get(&id) {
            return found.clone();
        }
        let result = check();
        let mut cache = self.0.lock().expect("not poisoned");
        if cache.len() >= SIGNATURE_CACHE_SIZE {
            cache.clear();
        }
        cache.insert(id, result.clone());
        result
    }
}

/// A signature, parsed enough to know which key to look for.
enum Parsed {
    Ssh { sig: ssh_key::SshSig, fingerprint: String },
    Gpg { sig: DetachedSignature, ids: Vec<String> },
    X509,
    Malformed(&'static str),
}

fn parse_signature(signature: &[u8]) -> Parsed {
    if signature.starts_with(b"-----BEGIN SSH SIGNATURE-----") {
        let Ok(sig) = ssh_key::SshSig::from_pem(signature) else { return Parsed::Malformed("SSH") };
        let fingerprint = sig.public_key().fingerprint(ssh_key::HashAlg::Sha256).to_string();
        Parsed::Ssh { sig, fingerprint }
    } else if signature.starts_with(b"-----BEGIN PGP SIGNATURE-----") {
        let Ok((sig, _)) = DetachedSignature::from_armor_single(signature) else { return Parsed::Malformed("GPG") };
        let mut ids: Vec<String> = sig.signature.issuer_fingerprint().iter().map(|f| format!("{f:X}")).collect();
        ids.extend(sig.signature.issuer_key_id().iter().map(hex::encode_upper));
        Parsed::Gpg { sig, ids }
    } else if signature.starts_with(b"-----BEGIN SIGNED MESSAGE-----") {
        Parsed::X509
    } else {
        Parsed::Malformed("GPG")
    }
}

/// Checks a GPG signature against a stored key; the long key ID of the
/// (sub)key that made it if it's good.
fn check_gpg(sig: &DetachedSignature, ids: &[String], key: &SignedPublicKey, payload: &[u8]) -> Option<String> {
    let matches = |id: pgp::types::KeyId, fp: pgp::types::Fingerprint| {
        ids.iter().any(|i| *i == hex::encode_upper(id) || *i == format!("{fp:X}"))
    };
    if matches(key.primary_key.legacy_key_id(), key.primary_key.fingerprint()) {
        return sig.verify(&key.primary_key, payload).ok().map(|()| hex::encode_upper(key.primary_key.legacy_key_id()));
    }
    let sub = key.public_subkeys.iter().find(|s| matches(s.key.legacy_key_id(), s.key.fingerprint()))?;
    sig.verify(&sub.key, payload).ok().map(|()| hex::encode_upper(sub.key.legacy_key_id()))
}

impl Core {
    pub async fn add_gpg_key(&self, user_id: i64, armored: &str) -> Result<GpgKey> {
        let parsed = parse_gpg_key(armored)?;
        let mut tx = self.db.begin().await?;
        let key = sqlx::query_as::<_, GpgKey>(
            "INSERT INTO gpg_keys (user_id, fingerprint, emails, public_key, expires_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) RETURNING *",
        )
        .bind(user_id)
        .bind(&parsed.fingerprint)
        .bind(parsed.emails.join("\n"))
        .bind(armored.trim())
        .bind(parsed.expires_at)
        .bind(now())
        .fetch_one(&mut *tx)
        .await;
        let key = match key {
            Err(sqlx::Error::Database(e)) if e.is_unique_violation() => return invalid("This key is already in use."),
            k => k?,
        };
        for id in &parsed.ids {
            let added = sqlx::query("INSERT INTO gpg_key_ids (key_id, gpg_key_id) VALUES (?1, ?2)")
                .bind(id)
                .bind(key.id)
                .execute(&mut *tx)
                .await;
            match added {
                Err(sqlx::Error::Database(e)) if e.is_unique_violation() => {
                    return invalid("A subkey of this key is already in use.");
                }
                r => r?,
            };
        }
        tx.commit().await?;
        Ok(key)
    }

    pub async fn gpg_keys(&self, user_id: i64) -> Result<Vec<GpgKey>> {
        Ok(sqlx::query_as("SELECT * FROM gpg_keys WHERE user_id = ?1 ORDER BY created_at, id")
            .bind(user_id)
            .fetch_all(&self.db)
            .await?)
    }

    pub async fn delete_gpg_key(&self, user_id: i64, key_id: i64) -> Result<()> {
        let done = sqlx::query("DELETE FROM gpg_keys WHERE id = ?1 AND user_id = ?2")
            .bind(key_id)
            .bind(user_id)
            .execute(&self.db)
            .await?;
        if done.rows_affected() == 0 {
            return Err(Error::NotFound);
        }
        Ok(())
    }

    /// Checks the signatures of those of `ids` (commits of `repo_id`) that
    /// are signed.
    pub async fn verify_commits(&self, repo_id: i64, ids: Vec<Oid>) -> Result<HashMap<Oid, Verification>> {
        let signed = self.git(repo_id, move |g| g.signed_commits(&ids)).await?;
        self.verify_signed(signed).await
    }

    /// Checks signatures read from a repository.
    pub(crate) async fn verify_signed(&self, signed: Vec<SignedCommit>) -> Result<HashMap<Oid, Verification>> {
        if signed.is_empty() {
            return Ok(HashMap::new());
        }
        // Which keys to fetch.
        let parsed: Vec<(SignedCommit, Parsed)> = signed
            .into_iter()
            .map(|s| {
                let p = parse_signature(&s.signature);
                (s, p)
            })
            .collect();
        let mut ssh_owners: HashMap<String, String> = HashMap::new();
        let mut gpg_keys: HashMap<String, GpgKeyRow> = HashMap::new();
        for (_, p) in &parsed {
            match p {
                Parsed::Ssh { fingerprint, .. } if !ssh_owners.contains_key(fingerprint) => {
                    let owner: Option<(String,)> = sqlx::query_as(
                        "SELECT u.username FROM ssh_keys k JOIN users u ON u.id = k.user_id
                         WHERE k.fingerprint = ?1 AND u.disabled_at IS NULL",
                    )
                    .bind(fingerprint)
                    .fetch_optional(&self.db)
                    .await?;
                    if let Some((owner,)) = owner {
                        ssh_owners.insert(fingerprint.clone(), owner);
                    }
                }
                Parsed::Gpg { ids, .. } => {
                    for id in ids {
                        if gpg_keys.contains_key(id) {
                            continue;
                        }
                        let key: Option<GpgKeyRow> = sqlx::query_as(
                            "SELECT u.username, k.fingerprint, k.public_key, k.expires_at, k.emails
                             FROM gpg_key_ids i JOIN gpg_keys k ON k.id = i.gpg_key_id JOIN users u ON u.id = k.user_id
                             WHERE i.key_id = ?1 AND u.disabled_at IS NULL",
                        )
                        .bind(id)
                        .fetch_optional(&self.db)
                        .await?;
                        if let Some(key) = key {
                            gpg_keys.insert(id.clone(), key);
                        }
                    }
                }
                _ => {}
            }
        }
        // The cryptography, off the async threads.
        let cache = self.signature_cache.clone();
        let server = (self.signing_key.fingerprint.clone(), self.config.site_name.clone());
        let checked: Vec<(Oid, String, Verification)> = tokio::task::spawn_blocking(move || {
            // Each GPG key is parsed once, however many commits it signed.
            let mut keys = HashMap::new();
            parsed
                .into_iter()
                .map(|(s, p)| {
                    let v = check(&s, p, &server, &ssh_owners, &gpg_keys, &mut keys, &cache);
                    (s.id, s.committer_email.to_lowercase(), v)
                })
                .collect()
        })
        .await
        .map_err(anyhow::Error::from)?;
        // Good signatures also need the committer to be the key's owner
        // (keel vouches for the commits it made itself).
        let emails: Vec<&str> =
            checked.iter().filter(|(_, _, v)| v.verified && v.made_on.is_none()).map(|(_, e, _)| e.as_str()).collect();
        let owners = self.commit_authors(&emails).await?;
        Ok(checked
            .into_iter()
            .map(|(id, email, mut v)| {
                if v.verified && v.made_on.is_none() && owners.get(&email) != v.signer.as_ref() {
                    v.verified = false;
                    v.reason = "bad_email";
                }
                (id, v)
            })
            .collect())
    }
}

/// A stored GPG key: owner, fingerprint, the key, when it expires, and its
/// emails (one per line).
type GpgKeyRow = (String, String, String, Option<i64>, String);

fn check(
    s: &SignedCommit,
    parsed: Parsed,
    (server_key, site): &(String, String),
    ssh_owners: &HashMap<String, String>,
    gpg_keys: &HashMap<String, GpgKeyRow>,
    parsed_keys: &mut HashMap<String, Option<SignedPublicKey>>,
    cache: &SignatureCache,
) -> Verification {
    let unverified = |reason, kind, key: String, signer: Option<&String>| Verification {
        verified: false,
        reason,
        kind,
        key,
        signer: signer.cloned(),
        made_on: None,
    };
    match parsed {
        Parsed::Malformed(kind) => unverified("malformed_signature", kind, String::new(), None),
        Parsed::X509 => unverified("unknown_signature_type", "X.509", String::new(), None),
        Parsed::Ssh { sig, fingerprint } if fingerprint == *server_key => {
            let good = cache.get_or(s.id, &fingerprint, || {
                let key = ssh_key::PublicKey::from(sig.public_key().clone());
                key.verify("git", &s.payload, &sig).ok().map(|()| String::new())
            });
            match good {
                Some(_) => Verification {
                    verified: true,
                    reason: "valid",
                    kind: "SSH",
                    key: fingerprint,
                    signer: None,
                    made_on: Some(site.clone()),
                },
                None => unverified("invalid", "SSH", fingerprint, None),
            }
        }
        Parsed::Ssh { sig, fingerprint } => {
            let Some(owner) = ssh_owners.get(&fingerprint) else {
                return unverified("unknown_key", "SSH", fingerprint, None);
            };
            let good = cache.get_or(s.id, &fingerprint, || {
                let key = ssh_key::PublicKey::from(sig.public_key().clone());
                key.verify("git", &s.payload, &sig).ok().map(|()| String::new())
            });
            match good {
                Some(_) => Verification {
                    verified: true,
                    reason: "valid",
                    kind: "SSH",
                    key: fingerprint,
                    signer: Some(owner.clone()),
                    made_on: None,
                },
                None => unverified("invalid", "SSH", fingerprint, Some(owner)),
            }
        }
        Parsed::Gpg { sig, ids } => {
            let shown = ids.iter().find(|i| i.len() == 16).or(ids.first()).cloned().unwrap_or_default();
            let Some((owner, fingerprint, armored, expires_at, emails)) = ids.iter().find_map(|i| gpg_keys.get(i)) else {
                return unverified("unknown_key", "GPG", shown, None);
            };
            let key = parsed_keys
                .entry(fingerprint.clone())
                .or_insert_with(|| SignedPublicKey::from_string(armored).ok().map(|(k, _)| k))
                .as_ref();
            let checked = cache.get_or(s.id, fingerprint, || key.and_then(|k| check_gpg(&sig, &ids, k, &s.payload)));
            let Some(key_id) = checked else {
                return unverified("invalid", "GPG", shown, Some(owner));
            };
            let signed_at = sig.signature.created().map(|t| i64::from(t.as_secs()));
            if let (Some(expires), Some(at)) = (expires_at, signed_at)
                && at > *expires
            {
                return unverified("expired_key", "GPG", key_id, Some(owner));
            }
            // Keys stored before keel checked subkeys, or subkeys that
            // expired since.
            let subkey = key
                .into_iter()
                .flat_map(|k| &k.public_subkeys)
                .find(|sub| hex::encode_upper(sub.key.legacy_key_id()) == key_id);
            match subkey.map(|sub| subkey_state(sub, signed_at.unwrap_or_else(now))) {
                Some(SubkeyState::Unusable) => return unverified("invalid", "GPG", key_id, Some(owner)),
                Some(SubkeyState::Expired) => return unverified("expired_key", "GPG", key_id, Some(owner)),
                _ => {}
            }
            // As GitHub: the key must be for the committer's email.
            if !emails.lines().any(|e| e == s.committer_email.to_lowercase()) {
                return unverified("bad_email", "GPG", key_id, Some(owner));
            }
            Verification { verified: true, reason: "valid", kind: "GPG", key: key_id, signer: Some(owner.clone()), made_on: None }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::process::Command;

    fn run(dir: &Path, env: &[(&str, &Path)], args: &[&str]) -> String {
        let out = Command::new(args[0])
            .args(&args[1..])
            .current_dir(dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_COMMITTER_NAME", "Ada")
            .env("GIT_AUTHOR_NAME", "Ada")
            .envs(env.iter().map(|(k, v)| (*k, *v)))
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    #[tokio::test]
    async fn verifying() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let bob = core.register("bob", "bob@example.com", "password1").await.unwrap();
        let repo = core.create_repo(&ada, "engine", "", false).await.unwrap();
        let work = tempfile::tempdir().unwrap();
        let w = work.path();
        let as_ada = [("GIT_COMMITTER_EMAIL", Path::new("ada@example.com"))];
        let as_bob = [("GIT_COMMITTER_EMAIL", Path::new("bob@example.com"))];
        run(w, &[], &["git", "init", "-q", "-b", "main"]);
        run(w, &[], &["git", "config", "user.email", "ada@example.com"]);
        run(w, &[], &["ssh-keygen", "-q", "-t", "ed25519", "-N", "", "-C", "ada", "-f", "key"]);
        run(w, &[], &["git", "config", "gpg.format", "ssh"]);
        run(w, &[], &["git", "config", "user.signingkey", w.join("key.pub").to_str().unwrap()]);
        // `extra`: `git -c` options, and whether to sign.
        let commit = |env: &[(&str, &Path)], extra: &[&str], msg: &str| {
            let (config, sign) = extra.split_at(extra.len().saturating_sub(1));
            let mut args = vec!["git"];
            args.extend(config);
            args.extend(["commit", "-q", "--allow-empty", "-m", msg]);
            args.extend(sign);
            run(w, env, &args);
            Oid::from_str(&run(w, &[], &["git", "rev-parse", "HEAD"])).unwrap()
        };
        let unsigned = commit(&as_ada, &[], "unsigned");
        let ssh = commit(&as_ada, &["-S"], "ssh");
        let ssh_bob = commit(&as_bob, &["-S"], "ssh, committed as bob");
        // Same signature, different message.
        let raw = run(w, &[], &["git", "cat-file", "commit", &ssh.to_string()]);
        std::fs::write(w.join("forged"), raw.replace("\n\nssh", "\n\nforged") + "\n").unwrap();
        let forged = Oid::from_str(&run(w, &[], &["git", "hash-object", "-t", "commit", "-w", "forged"])).unwrap();
        run(w, &[], &["git", "branch", "forged", &forged.to_string()]);

        // GPG, if it's installed.
        let gnupg = tempfile::tempdir_in("/tmp").unwrap();
        let home = [("GNUPGHOME", gnupg.path())];
        let gpg = Command::new("gpg").arg("--version").output().is_ok();
        let mut gpg_commit = None;
        let mut gpg_key = String::new();
        // Signed with a subkey revoked since, and with a key for another
        // email.
        let mut more = Vec::new();
        let (mut sub_key, mut sub_id, mut work_key) = (String::new(), String::new(), String::new());
        if gpg {
            let gen_key = |uid: &str, usage: &str| {
                run(w, &home, &["gpg", "--batch", "--quiet", "--passphrase", "", "--quick-gen-key", uid, "ed25519", usage, "never"]);
            };
            gen_key("Ada <ada@example.com>", "sign");
            gpg_key = run(w, &home, &["gpg", "--armor", "--export", "ada@example.com"]);
            let env = [as_ada[0], home[0]];
            let sign_with = |key: &str, msg: &str| {
                let key = format!("user.signingkey={key}");
                commit(&env, &["-c", "gpg.format=openpgp", "-c", &key, "-S"], msg)
            };
            gpg_commit = Some(sign_with("ada@example.com", "gpg"));

            gen_key("Sub <sub@example.com>", "cert");
            let colons = run(w, &home, &["gpg", "--list-keys", "--with-colons", "sub@example.com"]);
            let field = |kind: &str, n: usize| {
                colons.lines().find(|l| l.starts_with(kind)).unwrap().split(':').nth(n).unwrap().to_owned()
            };
            let fpr = field("fpr", 9);
            run(w, &home, &["gpg", "--batch", "--quiet", "--passphrase", "", "--quick-add-key", &fpr, "ed25519", "sign", "never"]);
            let colons = run(w, &home, &["gpg", "--list-keys", "--with-colons", "sub@example.com"]);
            sub_id = colons.lines().find(|l| l.starts_with("sub")).unwrap().split(':').nth(4).unwrap().to_owned();
            more.push(sign_with(&format!("{sub_id}!"), "revoked subkey"));
            let mut revoke = Command::new("gpg")
                .args(["--batch", "--yes", "--command-fd", "0", "--pinentry-mode", "loopback", "--passphrase", ""])
                .args(["--edit-key", &fpr])
                .env("GNUPGHOME", gnupg.path())
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .unwrap();
            std::io::Write::write_all(&mut revoke.stdin.take().unwrap(), b"key 1\nrevkey\ny\n0\n\ny\nsave\n").unwrap();
            assert!(revoke.wait().unwrap().success());
            sub_key = run(w, &home, &["gpg", "--armor", "--export", "sub@example.com"]);

            gen_key("Ada <ada@work.example>", "sign");
            work_key = run(w, &home, &["gpg", "--armor", "--export", "ada@work.example"]);
            more.push(sign_with("ada@work.example", "work key"));
        }
        let remote = core.repo_path(&repo);
        run(w, &[], &["git", "push", "-q", remote.to_str().unwrap(), "main", "forged"]);

        let mut ids = vec![unsigned, ssh, ssh_bob, forged];
        ids.extend(gpg_commit);
        ids.extend(&more);
        let reasons = |found: &HashMap<Oid, Verification>| -> Vec<&'static str> {
            ids.iter().map(|id| found.get(id).map_or("unsigned", |v| v.reason)).collect()
        };
        let found = core.verify_commits(repo.id, ids.clone()).await.unwrap();
        let mut want = vec!["unsigned", "unknown_key", "unknown_key", "unknown_key"];
        if gpg {
            want.extend(["unknown_key"; 3]);
        }
        assert_eq!(reasons(&found), want);

        core.add_ssh_key(ada.id, "laptop", &std::fs::read_to_string(w.join("key.pub")).unwrap()).await.unwrap();
        if gpg {
            let key = core.add_gpg_key(ada.id, &gpg_key).await.unwrap();
            assert_eq!(key.emails().collect::<Vec<_>>(), ["ada@example.com"]);
            assert_eq!(key.expires_at, None);
            assert!(core.add_gpg_key(bob.id, &gpg_key).await.is_err(), "taken");
            // The revoked subkey isn't recorded.
            assert!(!parse_gpg_key(&sub_key).unwrap().ids.contains(&sub_id));
            core.add_gpg_key(ada.id, &sub_key).await.unwrap();
            core.add_gpg_key(ada.id, &work_key).await.unwrap();
        }
        let found = core.verify_commits(repo.id, ids.clone()).await.unwrap();
        let mut want = vec!["unsigned", "valid", "bad_email", "invalid"];
        if gpg {
            want.extend(["valid", "unknown_key", "bad_email"]);
        }
        assert_eq!(reasons(&found), want);
        if gpg {
            // As keys stored by older versions have it.
            let (key,): (i64,) = sqlx::query_as("SELECT id FROM gpg_keys WHERE emails = 'sub@example.com'")
                .fetch_one(&core.db)
                .await
                .unwrap();
            sqlx::query("INSERT INTO gpg_key_ids (key_id, gpg_key_id) VALUES (?1, ?2)")
                .bind(&sub_id)
                .bind(key)
                .execute(&core.db)
                .await
                .unwrap();
            let found = core.verify_commits(repo.id, more.clone()).await.unwrap();
            assert_eq!(found[&more[0]].reason, "invalid");
        }
        let v = &found[&ssh];
        assert!(v.verified);
        assert_eq!((v.kind, v.signer.as_deref()), ("SSH", Some("ada")));
        assert!(v.describe().starts_with("Signed with ada’s SSH key SHA256:"), "{}", v.describe());
        if let Some(id) = gpg_commit {
            assert_eq!((found[&id].kind, found[&id].key.len()), ("GPG", 16));
            let _ = Command::new("gpgconf").args(["--kill", "gpg-agent"]).env("GNUPGHOME", gnupg.path()).status();
        }

        // Disabled accounts' keys don't verify.
        sqlx::query("UPDATE users SET disabled_at = 1 WHERE id = ?1").bind(ada.id).execute(&core.db).await.unwrap();
        let found = core.verify_commits(repo.id, vec![ssh]).await.unwrap();
        assert_eq!(found[&ssh].reason, "unknown_key");
        sqlx::query("UPDATE users SET disabled_at = NULL WHERE id = ?1").bind(ada.id).execute(&core.db).await.unwrap();

        // Removing the key unverifies.
        let gpg_keys = core.gpg_keys(ada.id).await.unwrap();
        for k in gpg_keys {
            core.delete_gpg_key(ada.id, k.id).await.unwrap();
        }
        assert!(parse_gpg_key("nonsense").is_err());
    }
}
