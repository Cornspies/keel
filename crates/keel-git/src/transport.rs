use std::path::Path;
use std::process::{Command, Stdio};

/// The two server-side git programs behind clone/fetch and push.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Service {
    UploadPack,
    ReceivePack,
}

impl Service {
    /// Parses `git-upload-pack` / `git-receive-pack` as used in HTTP query
    /// strings and SSH commands (`git upload-pack` with a space is accepted
    /// too, as some clients send that).
    pub fn parse(s: &str) -> Option<Self> {
        match s.strip_prefix("git-").or_else(|| s.strip_prefix("git "))? {
            "upload-pack" => Some(Self::UploadPack),
            "receive-pack" => Some(Self::ReceivePack),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::UploadPack => "git-upload-pack",
            Self::ReceivePack => "git-receive-pack",
        }
    }

    fn subcommand(self) -> &'static str {
        match self {
            Self::UploadPack => "upload-pack",
            Self::ReceivePack => "receive-pack",
        }
    }

    pub fn is_write(self) -> bool {
        self == Self::ReceivePack
    }
}

/// Builds the command that serves `service` for the repository at `path`.
///
/// - `stateless`: HTTP mode (`--stateless-rpc`), one request per round.
/// - `advertise`: only print the ref advertisement (HTTP `info/refs`).
/// - `protocol`: the client's `GIT_PROTOCOL` value, enabling protocol v2.
///
/// stdin/stdout/stderr are all piped.
pub fn git_command(
    service: Service,
    path: &Path,
    stateless: bool,
    advertise: bool,
    protocol: Option<&str>,
) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg(service.subcommand());
    if stateless {
        cmd.arg("--stateless-rpc");
    }
    if advertise {
        cmd.arg("--advertise-refs");
    }
    cmd.arg(path)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        // See `crate::command`.
        .env("GIT_NO_REPLACE_OBJECTS", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Unlike `crate::command`, the service account's global config applies:
    // settings such as `safe.directory` (for a data directory owned by
    // another user) are only read from there.
    if let Some(home) = std::env::var_os("HOME") {
        cmd.env("HOME", home);
    }
    if let Some(p) = protocol.filter(|p| is_valid_protocol(p)) {
        cmd.env("GIT_PROTOCOL", p);
    }
    cmd
}

/// `GIT_PROTOCOL` is a colon-separated list of `key=value` pairs, e.g.
/// `version=2`. Anything else is dropped rather than passed to git.
fn is_valid_protocol(p: &str) -> bool {
    p.len() < 256
        && p.split(':').all(|kv| {
            !kv.is_empty()
                && kv
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"=-_.".contains(&b))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_services() {
        assert_eq!(Service::parse("git-upload-pack"), Some(Service::UploadPack));
        assert_eq!(Service::parse("git receive-pack"), Some(Service::ReceivePack));
        assert_eq!(Service::parse("git-upload-archive"), None);
        assert_eq!(Service::parse("upload-pack"), None);
    }

    #[test]
    fn protocol_validation() {
        assert!(is_valid_protocol("version=2"));
        assert!(is_valid_protocol("version=2:object-format=sha1"));
        assert!(!is_valid_protocol("version=2\nfoo"));
        assert!(!is_valid_protocol(""));
    }
}
