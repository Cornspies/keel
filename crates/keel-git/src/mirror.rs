//! Fetching from another server into a mirror, with git locked down: only
//! http(s), no redirects, no proxies, no credential helpers or prompts, the
//! host pinned to an address the caller checked, and limits on time and
//! size.

use std::net::IpAddr;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use base64::Engine;

use crate::{Error, Result};

/// How to reach the remote.
pub struct Remote<'a> {
    pub url: &'a str,
    /// Username and password (or token), sent as HTTP Basic auth.
    pub auth: Option<(&'a str, &'a str)>,
    /// Connect to `host:port` at this address rather than resolving it
    /// again (which could give a different, internal address).
    pub pin: Option<(&'a str, u16, IpAddr)>,
}

fn git(path: &Path, remote: &Remote<'_>) -> Command {
    let mut config: Vec<(&str, String)> = vec![
        ("http.followRedirects", "false".into()),
        ("credential.helper", String::new()),
        ("core.askPass", String::new()),
        ("protocol.allow", "never".into()),
        ("protocol.https.allow", "always".into()),
        ("protocol.http.allow", "always".into()),
        // Every fetch keeps what it gets as a pack, whose growth `run`
        // can watch (rather than as loose objects).
        ("fetch.unpackLimit", "1".into()),
    ];
    if let Some((host, port, ip)) = remote.pin {
        let addr = match ip {
            IpAddr::V6(v6) => format!("[{v6}]"),
            IpAddr::V4(v4) => v4.to_string(),
        };
        config.push(("http.curloptResolve", format!("{host}:{port}:{addr}")));
    }
    if let Some((user, pass)) = remote.auth {
        let basic = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
        config.push(("http.extraHeader", format!("Authorization: Basic {basic}")));
    }
    let mut cmd = Command::new("git");
    cmd.arg("--git-dir")
        .arg(path)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default());
    crate::standard_env(&mut cmd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_ALLOW_PROTOCOL", "http:https")
        // As environment, not arguments, so credentials don't show in `ps`.
        .env("GIT_CONFIG_COUNT", config.len().to_string());
    for (i, (key, value)) in config.iter().enumerate() {
        cmd.env(format!("GIT_CONFIG_KEY_{i}"), key).env(format!("GIT_CONFIG_VALUE_{i}"), value);
    }
    cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    cmd
}

/// Bytes of the packs git is receiving into the repository at `path`.
fn incoming_bytes(path: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(path.join("objects/pack")) else { return 0 };
    entries
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("tmp_pack_"))
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}

/// Runs `cmd`, killing it after `limit`, or, with `watch` (a repository and
/// a number of bytes), once the packs it's receiving there grow past that
/// size. Returns stdout.
fn run(mut cmd: Command, limit: Duration, watch: Option<(&Path, u64)>) -> Result<String> {
    let mut child = cmd.spawn()?;
    let deadline = Instant::now() + limit;
    // Read output on threads so a chatty git can't block on a full pipe.
    let mut stdout = child.stdout.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    let out = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::Read::read_to_string(&mut stdout, &mut s);
        s
    });
    let err = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = std::io::Read::read_to_string(&mut stderr, &mut s);
        s
    });
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::Other(format!("timed out after {} minutes", limit.as_secs() / 60)));
        }
        if let Some((path, max)) = watch
            && incoming_bytes(path) > max
        {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::Other(format!("it's larger than the {} MiB of storage left", max >> 20)));
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let (out, err) = (out.join().unwrap_or_default(), err.join().unwrap_or_default());
    if !status.success() {
        let msg = err.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("git failed").trim();
        return Err(Error::Other(msg.trim_start_matches("fatal: ").to_owned()));
    }
    Ok(out)
}

/// Makes the bare repository at `path` match the remote's branches and
/// tags (deleting those gone there; other refs are left alone), and points
/// HEAD at the remote's default branch. Stops after `limit`, or once it
/// has fetched more than `max_bytes`.
pub fn mirror_fetch(path: &Path, remote: &Remote<'_>, limit: Duration, max_bytes: Option<u64>) -> Result<()> {
    fetch_refs(path, remote, limit, max_bytes, &["+refs/heads/*:refs/heads/*", "+refs/tags/*:refs/tags/*"])
}

/// Like [`mirror_fetch`], also fetching the heads of pull requests
/// (`refs/pull/<n>/head` on GitHub and Gitea), for importing them.
pub fn import_fetch(path: &Path, remote: &Remote<'_>, limit: Duration, max_bytes: Option<u64>) -> Result<()> {
    fetch_refs(
        path,
        remote,
        limit,
        max_bytes,
        &["+refs/heads/*:refs/heads/*", "+refs/tags/*:refs/tags/*", "+refs/pull/*/head:refs/pull/*/head"],
    )
}

fn fetch_refs(path: &Path, remote: &Remote<'_>, limit: Duration, max_bytes: Option<u64>, refspecs: &[&str]) -> Result<()> {
    let mut fetch = git(path, remote);
    fetch
        .args(["fetch", "--prune", "--quiet", "--no-write-fetch-head", "--no-auto-gc", "--"])
        .arg(remote.url)
        .args(refspecs);
    run(fetch, limit, max_bytes.map(|max| (path, max)))?;

    let mut head = git(path, remote);
    head.args(["ls-remote", "--symref", "--"]).arg(remote.url).arg("HEAD");
    let out = run(head, Duration::from_secs(60), None)?;
    let branch = out
        .lines()
        .find_map(|l| l.strip_prefix("ref: ")?.strip_suffix("\tHEAD"))
        .map(str::to_owned);
    if let Some(branch) = branch
        && branch.starts_with("refs/heads/")
    {
        let repo = git2::Repository::open_bare(path)?;
        if repo.find_reference(&branch).is_ok() {
            repo.set_head(&branch)?;
        }
    }
    Ok(())
}

/// The remote's branches and tags.
pub fn ls_remote(path: &Path, remote: &Remote<'_>) -> Result<crate::Refs> {
    let mut cmd = git(path, remote);
    cmd.args(["ls-remote", "--heads", "--tags", "--"]).arg(remote.url);
    let out = run(cmd, Duration::from_secs(120), None)?;
    let mut refs = crate::Refs::new();
    for line in out.lines() {
        let Some((id, name)) = line.split_once('\t') else { continue };
        // `^{}` lines are what annotated tags point at.
        if name.ends_with("^{}") {
            continue;
        }
        if let Ok(id) = git2::Oid::from_str(id) {
            refs.insert(name.to_owned(), id);
        }
    }
    Ok(refs)
}

/// Makes the remote's branches and tags match the bare repository at
/// `path`, deleting there what's gone here (other refs are left alone).
pub fn mirror_push(path: &Path, remote: &Remote<'_>, limit: Duration) -> Result<()> {
    let mut cmd = git(path, remote);
    cmd.args(["push", "--prune", "--quiet", "--no-verify", "--"])
        .arg(remote.url)
        .args(["+refs/heads/*:refs/heads/*", "+refs/tags/*:refs/tags/*"]);
    run(cmd, limit, None).map(|_| ())
}

/// The object a Git LFS pointer file refers to: (oid, size).
pub fn parse_lfs_pointer(data: &[u8]) -> Option<(String, u64)> {
    if data.len() > 1024 {
        return None;
    }
    let text = std::str::from_utf8(data).ok()?;
    if !text.starts_with("version https://git-lfs.github.com/spec/v1\n") {
        return None;
    }
    let (mut oid, mut size) = (None, None);
    for line in text.lines() {
        if let Some(o) = line.strip_prefix("oid sha256:") {
            oid = Some(o.trim().to_owned());
        } else if let Some(s) = line.strip_prefix("size ") {
            size = s.trim().parse().ok();
        }
    }
    let oid = oid.filter(|o| o.len() == 64 && o.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))?;
    Some((oid, size?))
}

impl crate::GitRepo {
    /// The Git LFS pointers in files that ref updates `(old, new)` added
    /// or changed (everything at `new` without `old`, or if `old` is
    /// unknown here): (oid, size), without duplicates.
    pub fn lfs_pointers(&self, changes: &[(Option<git2::Oid>, git2::Oid)]) -> Result<Vec<(String, u64)>> {
        let mut seen_blobs = std::collections::HashSet::new();
        // Branches and tags share most of their trees, so each tree is read
        // once, however many new refs have it (as on a mirror's first sync).
        let mut seen_trees = std::collections::HashSet::new();
        let mut out = std::collections::BTreeMap::new();
        for (old, new) in changes {
            let known_old = old.filter(|o| self.repo.find_commit(*o).is_ok());
            let blobs: Vec<git2::Oid> = match known_old {
                Some(o) => self.changed_files(o, *new)?.changed.into_iter().map(|f| f.id).collect(),
                None => self.new_blobs(self.repo.find_commit(*new)?.tree_id(), &mut seen_trees)?,
            };
            for id in blobs {
                if !seen_blobs.insert(id) || self.blob_size(id)? > 1024 {
                    continue;
                }
                if let Some((data, _)) = self.blob_by_id(id)?
                    && let Some((oid, size)) = parse_lfs_pointer(&data)
                {
                    out.insert(oid, size);
                }
            }
        }
        Ok(out.into_iter().collect())
    }

    /// The files in `tree` and its subtrees, skipping trees already in
    /// `seen` (and adding the ones it reads).
    fn new_blobs(&self, tree: git2::Oid, seen: &mut std::collections::HashSet<git2::Oid>) -> Result<Vec<git2::Oid>> {
        let (mut blobs, mut stack) = (Vec::new(), vec![tree]);
        while let Some(id) = stack.pop() {
            if !seen.insert(id) {
                continue;
            }
            for entry in self.repo.find_tree(id)?.iter() {
                match entry.kind() {
                    Some(git2::ObjectType::Tree) => stack.push(entry.id()),
                    Some(git2::ObjectType::Blob) => blobs.push(entry.id()),
                    _ => {} // submodules
                }
            }
        }
        Ok(blobs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn incoming_packs() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(incoming_bytes(dir.path()), 0, "no objects directory yet");
        let pack = dir.path().join("objects/pack");
        std::fs::create_dir_all(&pack).unwrap();
        std::fs::write(pack.join("pack-1234.pack"), vec![0; 100]).unwrap();
        std::fs::write(pack.join("tmp_pack_AbC123"), vec![0; 300]).unwrap();
        std::fs::write(pack.join("tmp_pack_XyZ789"), vec![0; 20]).unwrap();
        assert_eq!(incoming_bytes(dir.path()), 320, "only packs still arriving");
    }

    #[test]
    fn pointers() {
        let oid = "a".repeat(64);
        let p = format!("version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize 12345\n");
        assert_eq!(parse_lfs_pointer(p.as_bytes()), Some((oid.clone(), 12345)));
        assert_eq!(parse_lfs_pointer(b"just a file"), None);
        assert_eq!(parse_lfs_pointer(p.replace("sha256:a", "sha256:Z").as_bytes()), None);

        let s = crate::read::tests::Scratch::new();
        s.write("a.txt", "text");
        s.write("big.bin", &p);
        let first = s.commit("first");
        let other = format!("version https://git-lfs.github.com/spec/v1\noid sha256:{}\nsize 1\n", "b".repeat(64));
        s.write("more.bin", &other);
        let second = s.commit("second");
        let repo = s.open();
        assert_eq!(repo.lfs_pointers(&[(None, second)]).unwrap().len(), 2);
        assert_eq!(repo.lfs_pointers(&[(Some(first), second)]).unwrap(), [("b".repeat(64), 1)]);
        let unknown = git2::Oid::from_str(&"c".repeat(40)).unwrap();
        assert_eq!(repo.lfs_pointers(&[(Some(unknown), second)]).unwrap().len(), 2, "unknown old: everything");
        // Several new refs sharing trees, as on a first sync.
        let all = repo.lfs_pointers(&[(None, first), (None, second), (None, second)]).unwrap();
        assert_eq!(all, [("a".repeat(64), 12345), ("b".repeat(64), 1)]);
    }

    #[test]
    fn refuses_other_protocols() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.git");
        crate::create_bare(&path, "main").unwrap();
        // A local path would read any repository on the server.
        let source = dir.path().join("src.git");
        crate::create_bare(&source, "main").unwrap();
        for url in [source.to_str().unwrap().to_owned(), format!("file://{}", source.display()), "ext::sh -c id".into()] {
            let remote = Remote { url: &url, auth: None, pin: None };
            assert!(mirror_fetch(&path, &remote, Duration::from_secs(10), None).is_err(), "{url}");
        }
    }
}
