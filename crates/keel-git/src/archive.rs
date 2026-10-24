//! Snapshots of a commit as `.tar.gz` or `.zip`, made by `git archive`.

use std::path::Path;
use std::process::{Command, Stdio};

use git2::Oid;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArchiveFormat {
    TarGz,
    Zip,
}

impl ArchiveFormat {
    /// The format a file name asks for, and the name without its extension.
    pub fn split(name: &str) -> Option<(&str, Self)> {
        if let Some(rev) = name.strip_suffix(".tar.gz") {
            Some((rev, Self::TarGz))
        } else {
            name.strip_suffix(".zip").map(|rev| (rev, Self::Zip))
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::TarGz => "tar.gz",
            Self::Zip => "zip",
        }
    }

    pub fn content_type(self) -> &'static str {
        match self {
            Self::TarGz => "application/gzip",
            Self::Zip => "application/zip",
        }
    }
}

/// The command that writes an archive of `commit` to stdout, with every
/// path under `prefix/`. stdout and stderr are piped.
pub fn archive_command(path: &Path, commit: Oid, format: ArchiveFormat, prefix: &str) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("--git-dir")
        .arg(path)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .arg("archive")
        .arg(format!("--format={}", format.extension()))
        .arg(format!("--prefix={prefix}/"))
        .arg(commit.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::read::tests::Scratch;

    #[test]
    fn archives() {
        assert_eq!(ArchiveFormat::split("v1.0.tar.gz"), Some(("v1.0", ArchiveFormat::TarGz)));
        assert_eq!(ArchiveFormat::split("feature/x.zip"), Some(("feature/x", ArchiveFormat::Zip)));
        assert_eq!(ArchiveFormat::split("main.tar"), None);

        let s = Scratch::new();
        s.write("src/lib.rs", "fn a() {}");
        let commit = s.commit("first");
        let repo = s.dir.path().join(".git");
        for format in [ArchiveFormat::TarGz, ArchiveFormat::Zip] {
            let out = archive_command(&repo, commit, format, "engine-main").output().unwrap();
            assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
            let magic: &[u8] = if format == ArchiveFormat::Zip { b"PK" } else { &[0x1f, 0x8b] };
            assert!(out.stdout.starts_with(magic));
        }
        let listing = Command::new("sh")
            .arg("-c")
            .arg(format!("git --git-dir {} archive --format=tar --prefix=engine-main/ {commit} | tar -t", repo.display()))
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&listing.stdout).contains("engine-main/src/lib.rs"));
    }
}
