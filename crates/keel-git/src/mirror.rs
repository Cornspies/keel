//! Fetching from another server into a mirror, with git locked down: only
//! http(s), no redirects, no proxies, no credential helpers or prompts, the
//! host pinned to an address the caller checked, and a time limit.

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
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
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

/// Runs `cmd`, killing it after `limit`. Returns stdout.
fn run(mut cmd: Command, limit: Duration) -> Result<String> {
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
/// HEAD at the remote's default branch.
pub fn mirror_fetch(path: &Path, remote: &Remote<'_>, limit: Duration) -> Result<()> {
    let mut fetch = git(path, remote);
    fetch
        .args(["fetch", "--prune", "--quiet", "--no-write-fetch-head", "--no-auto-gc", "--"])
        .arg(remote.url)
        .args(["+refs/heads/*:refs/heads/*", "+refs/tags/*:refs/tags/*"]);
    run(fetch, limit)?;

    let mut head = git(path, remote);
    head.args(["ls-remote", "--symref", "--"]).arg(remote.url).arg("HEAD");
    let out = run(head, Duration::from_secs(60))?;
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

#[cfg(test)]
mod tests {
    use super::*;

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
            assert!(mirror_fetch(&path, &remote, Duration::from_secs(10)).is_err(), "{url}");
        }
    }
}
