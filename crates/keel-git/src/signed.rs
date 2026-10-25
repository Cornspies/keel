//! Commit signatures (`gpgsig` headers), for verifying who made commits.

use git2::Oid;

use crate::{GitRepo, Result};

/// A signed commit's signature and what it signs.
#[derive(Clone, Debug)]
pub struct SignedCommit {
    pub id: Oid,
    /// ASCII-armored: an OpenPGP, SSH or X.509 signature.
    pub signature: Vec<u8>,
    /// The commit object without its signature.
    pub payload: Vec<u8>,
    pub committer_email: String,
}

impl GitRepo {
    /// The signatures of those of `ids` that are signed.
    pub fn signed_commits(&self, ids: &[Oid]) -> Result<Vec<SignedCommit>> {
        let mut out = Vec::new();
        for &id in ids {
            let (signature, payload) = match self.repo.extract_signature(&id, None) {
                Ok(s) => s,
                Err(e) if e.code() == git2::ErrorCode::NotFound => continue,
                Err(e) => return Err(e.into()),
            };
            let commit = self.repo.find_commit(id)?;
            out.push(SignedCommit {
                id,
                signature: signature.to_vec(),
                payload: payload.to_vec(),
                committer_email: String::from_utf8_lossy(commit.committer().email_bytes()).into_owned(),
            });
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use crate::read::tests::Scratch;

    #[test]
    fn signatures() {
        let s = Scratch::new();
        s.write("a", "1\n");
        let plain = s.commit("plain");
        // A commit with a (made-up) signature header.
        let raw = s.git(&["cat-file", "commit", &plain.to_string()]);
        let (headers, message) = raw.split_once("\n\n").unwrap();
        let signed = format!("{headers}\ngpgsig -----BEGIN SSH SIGNATURE-----\n U1NIU0lH\n -----END SSH SIGNATURE-----\n\n{message}\n");
        let path = s.dir.path().join("signed");
        std::fs::write(&path, &signed).unwrap();
        let id = s.git(&["hash-object", "-t", "commit", "-w", path.to_str().unwrap()]);
        let id = git2::Oid::from_str(&id).unwrap();

        let repo = s.open();
        let found = repo.signed_commits(&[plain, id]).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, id);
        assert!(found[0].signature.starts_with(b"-----BEGIN SSH SIGNATURE-----\nU1NIU0lH"));
        assert!(!String::from_utf8_lossy(&found[0].payload).contains("gpgsig"));
        assert_eq!(found[0].committer_email, "ada@example.com");
    }
}
