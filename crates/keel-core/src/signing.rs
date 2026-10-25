//! The server's own signing key: commits keel makes (merges, squashes,
//! rebases, edits in the browser) are signed with it, as SSH signatures,
//! so they show as verified like people's signed commits.

use std::path::Path;

use anyhow::Context;
use ssh_key::private::Ed25519Keypair;
use ssh_key::{HashAlg, LineEnding, PrivateKey, SshSig};

/// Where in the data directory the key is kept.
pub(crate) const KEY_FILE: &str = "signing_ed25519_key";

pub(crate) struct SigningKey {
    key: PrivateKey,
    /// `SHA256:…`, as signatures name their key.
    pub fingerprint: String,
    /// In OpenSSH's one-line format.
    pub public: String,
}

impl SigningKey {
    /// Reads the key at `path`; creates it first if `create` and it doesn't
    /// exist yet.
    pub fn load(path: &Path, create: bool) -> anyhow::Result<Self> {
        let key = if path.exists() || !create {
            PrivateKey::read_openssh_file(path).with_context(|| format!("reading the signing key {}", path.display()))?
        } else {
            let mut seed = [0u8; 32];
            getrandom::fill(&mut seed).map_err(|e| anyhow::anyhow!("getrandom: {e}"))?;
            let mut key = PrivateKey::from(Ed25519Keypair::from_seed(&seed));
            key.set_comment("keel commit signing");
            key.write_openssh_file(path, LineEnding::LF)
                .with_context(|| format!("writing the signing key {}", path.display()))?;
            crate::backup::private(path, 0o600)?;
            key
        };
        let public = key.public_key().to_openssh()?;
        let fingerprint = key.public_key().fingerprint(HashAlg::Sha256).to_string();
        Ok(Self { key, fingerprint, public })
    }
}

impl keel_git::Signer for SigningKey {
    fn sign(&self, commit: &[u8]) -> Result<String, String> {
        SshSig::sign(&self.key, "git", HashAlg::Sha512, commit)
            .and_then(|sig| sig.to_pem(LineEnding::LF))
            .map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use keel_git::Signer;

    #[test]
    fn signs_like_git() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(KEY_FILE);
        assert!(SigningKey::load(&path, false).is_err(), "not created unless asked");
        let key = SigningKey::load(&path, true).unwrap();
        let again = SigningKey::load(&path, false).unwrap();
        assert_eq!(key.fingerprint, again.fingerprint);
        assert!(key.public.starts_with("ssh-ed25519 "));

        let pem = key.sign(b"tree 1234\n").unwrap();
        assert!(pem.starts_with("-----BEGIN SSH SIGNATURE-----"));
        let sig: SshSig = pem.parse().unwrap();
        let public = key.key.public_key();
        assert!(public.verify("git", b"tree 1234\n", &sig).is_ok());
        assert!(public.verify("git", b"tree 5678\n", &sig).is_err());

        // git itself accepts it.
        let allowed = dir.path().join("allowed");
        std::fs::write(&allowed, format!("* namespaces=\"git\" {}\n", key.public)).unwrap();
        std::fs::write(dir.path().join("sig"), &pem).unwrap();
        std::fs::write(dir.path().join("msg"), "tree 1234\n").unwrap();
        let out = std::process::Command::new("ssh-keygen")
            .args(["-Y", "check-novalidate", "-n", "git", "-f"])
            .arg(&allowed)
            .arg("-s")
            .arg(dir.path().join("sig"))
            .stdin(std::fs::File::open(dir.path().join("msg")).unwrap())
            .output();
        if let Ok(out) = out {
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        }
    }
}
