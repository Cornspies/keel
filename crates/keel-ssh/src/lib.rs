//! Built-in SSH server for `git clone/fetch/push` over SSH.
//!
//! No system sshd or `authorized_keys` setup is needed: users upload public
//! keys in the web UI, and this server maps a key to its owner and runs
//! `git-upload-pack` / `git-receive-pack` for permitted repositories.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use keel_core::{Access, Core, User};
use keel_git::{Service, git_command};
use russh::keys::ssh_key::LineEnding;
use russh::keys::ssh_key::private::Ed25519Keypair;
use russh::keys::{HashAlg, PrivateKey, PublicKey};
use russh::server::{Auth, ChannelOpenHandle, Handle, Msg, Server, Session};
use russh::{Channel, ChannelId, MethodKind, MethodSet, SshId};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

/// Runs the SSH server until the listener fails.
pub async fn serve(core: Core, addr: SocketAddr) -> anyhow::Result<()> {
    let key = load_or_create_host_key(&core.config.ssh_host_key_path())?;
    let config = russh::server::Config {
        server_id: SshId::Standard("SSH-2.0-keel".into()),
        methods: MethodSet::from(&[MethodKind::PublicKey][..]),
        keys: vec![key],
        auth_rejection_time: Duration::from_millis(500),
        // OpenSSH probes with "none" first; don't make every login slow.
        auth_rejection_time_initial: Some(Duration::ZERO),
        inactivity_timeout: Some(Duration::from_secs(600)),
        keepalive_interval: Some(Duration::from_secs(30)),
        nodelay: true,
        ..Default::default()
    };
    tracing::info!("SSH server listening on {addr}");
    let mut server = SshServer { core };
    server
        .run_on_address(Arc::new(config), addr)
        .await
        .with_context(|| format!("SSH server on {addr}"))
}

fn load_or_create_host_key(path: &Path) -> anyhow::Result<PrivateKey> {
    if path.exists() {
        return PrivateKey::read_openssh_file(path)
            .with_context(|| format!("reading SSH host key {}", path.display()));
    }
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|e| anyhow::anyhow!("getrandom: {e}"))?;
    let key = PrivateKey::from(Ed25519Keypair::from_seed(&seed));
    key.write_openssh_file(path, LineEnding::LF)
        .with_context(|| format!("writing SSH host key {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    tracing::info!(
        "generated SSH host key {} ({})",
        path.display(),
        key.public_key().fingerprint(HashAlg::Sha256)
    );
    Ok(key)
}

struct SshServer {
    core: Core,
}

impl Server for SshServer {
    type Handler = Connection;

    fn new_client(&mut self, peer: Option<SocketAddr>) -> Connection {
        Connection {
            core: self.core.clone(),
            peer,
            user: None,
            protocol: HashMap::new(),
            stdin: HashMap::new(),
        }
    }

    fn handle_session_error(&mut self, error: anyhow::Error) {
        tracing::debug!("ssh session error: {error:#}");
    }
}

/// One SSH connection.
struct Connection {
    core: Core,
    peer: Option<SocketAddr>,
    user: Option<User>,
    /// `GIT_PROTOCOL` sent via `SetEnv`, per channel.
    protocol: HashMap<ChannelId, String>,
    /// Feeds client data to the running git process, per channel.
    stdin: HashMap<ChannelId, mpsc::Sender<Vec<u8>>>,
}

/// A parsed `git-upload-pack 'owner/repo.git'` command.
#[derive(Debug, PartialEq, Eq)]
struct GitRequest {
    service: Service,
    owner: String,
    repo: String,
}

fn parse_command(cmd: &str) -> Option<GitRequest> {
    let cmd = cmd.trim();
    let (program, arg) = cmd
        .split_once(" '")
        .map(|(p, a)| (p, a.strip_suffix('\'')))
        .or_else(|| cmd.rsplit_once(' ').map(|(p, a)| (p, Some(a))))?;
    let service = Service::parse(program.trim())?;
    let path = arg?.trim_start_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let (owner, repo) = path.split_once('/')?;
    if owner.is_empty() || repo.is_empty() || repo.contains(['/', '\'']) {
        return None;
    }
    Some(GitRequest {
        service,
        owner: owner.to_owned(),
        repo: repo.to_owned(),
    })
}

impl Connection {
    /// Writes a message to the client's stderr and ends the channel.
    async fn fail(handle: &Handle, channel: ChannelId, message: &str) {
        let _ = handle
            .extended_data(channel, 1, format!("{message}\n").into_bytes())
            .await;
        finish(handle, channel, 1).await;
    }

    async fn start_git(
        &mut self,
        channel: ChannelId,
        request: GitRequest,
        handle: Handle,
    ) -> anyhow::Result<()> {
        let user = self.user.clone();
        let repo = self.core.repo(&request.owner, &request.repo).await?;
        let needed = if request.service.is_write() { Access::Write } else { Access::Read };
        let access = match &repo {
            Some(r) => self.core.access(r, user.as_ref()).await?,
            None => Access::None,
        };
        let repo = match repo {
            Some(r) if access >= needed => r,
            Some(_) if access == Access::Read => {
                Self::fail(&handle, channel, "keel: you don't have permission to push to this repository").await;
                return Ok(());
            }
            _ => {
                Self::fail(&handle, channel, "keel: repository not found").await;
                return Ok(());
            }
        };

        let path = self.core.repo_path(&repo);
        let protocol = self.protocol.get(&channel).map(String::as_str);
        let mut cmd = tokio::process::Command::from(git_command(request.service, &path, false, false, protocol));
        let mut child = cmd.kill_on_drop(true).spawn().context("spawning git")?;
        let (Some(mut stdin), Some(stdout), Some(stderr)) =
            (child.stdin.take(), child.stdout.take(), child.stderr.take())
        else {
            anyhow::bail!("git stdio not captured");
        };

        let (tx, mut rx) = mpsc::channel::<Vec<u8>>(16);
        self.stdin.insert(channel, tx);
        tokio::spawn(async move {
            while let Some(chunk) = rx.recv().await {
                if stdin.write_all(&chunk).await.is_err() {
                    break;
                }
            }
            // Dropping stdin sends EOF to git.
        });

        let core = self.core.clone();
        let pusher = self.user.as_ref().map(|u| u.id);
        let service = request.service;
        tokio::spawn(async move {
            let out = pump(stdout, handle.clone(), channel, None);
            let err = pump(stderr, handle.clone(), channel, Some(1));
            let _ = tokio::join!(out, err);
            let code = match child.wait().await {
                Ok(status) => status.code().unwrap_or(1) as u32,
                Err(e) => {
                    tracing::error!("waiting for git: {e}");
                    1
                }
            };
            finish(&handle, channel, code).await;
            if code == 0
                && service == Service::ReceivePack
                && let Err(e) = core.after_push(repo.id, pusher).await
            {
                tracing::warn!("after push: {e}");
            }
        });
        Ok(())
    }
}

/// Copies a git output stream to the SSH channel (stdout, or stderr when
/// `ext` is `Some(1)`).
async fn pump(mut from: impl AsyncRead + Unpin, handle: Handle, channel: ChannelId, ext: Option<u32>) {
    let mut buf = vec![0u8; 32 * 1024];
    loop {
        let n = match from.read(&mut buf).await {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        let chunk = buf[..n].to_vec();
        let sent = match ext {
            Some(code) => handle.extended_data(channel, code, chunk).await,
            None => handle.data(channel, chunk).await,
        };
        if sent.is_err() {
            return;
        }
    }
}

async fn finish(handle: &Handle, channel: ChannelId, code: u32) {
    let _ = handle.exit_status_request(channel, code).await;
    let _ = handle.eof(channel).await;
    let _ = handle.close(channel).await;
}

impl russh::server::Handler for Connection {
    type Error = anyhow::Error;

    async fn auth_publickey_offered(&mut self, _user: &str, key: &PublicKey) -> Result<Auth, Self::Error> {
        // Skip the signature round-trip for keys we don't know.
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
        Ok(if self.core.ssh_key_exists(&fingerprint).await? {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }

    async fn auth_publickey(&mut self, _user: &str, key: &PublicKey) -> Result<Auth, Self::Error> {
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
        match self.core.user_by_ssh_fingerprint(&fingerprint).await? {
            Some(user) => {
                tracing::info!(user = %user.username, peer = ?self.peer, "ssh login");
                self.user = Some(user);
                Ok(Auth::Accept)
            }
            None => Ok(Auth::reject()),
        }
    }

    async fn channel_open_session(
        &mut self,
        _channel: Channel<Msg>,
        reply: ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        reply.accept().await;
        Ok(())
    }

    async fn env_request(
        &mut self,
        channel: ChannelId,
        name: &str,
        value: &str,
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        if name == "GIT_PROTOCOL" {
            self.protocol.insert(channel, value.to_owned());
            session.channel_success(channel)?;
        } else {
            session.channel_failure(channel)?;
        }
        Ok(())
    }

    async fn shell_request(&mut self, channel: ChannelId, session: &mut Session) -> Result<(), Self::Error> {
        session.channel_success(channel)?;
        let name = self.user.as_ref().map(|u| u.username.as_str()).unwrap_or("there");
        let handle = session.handle();
        let message = format!("Hi {name}! You've successfully authenticated, but keel does not provide shell access.\r\n");
        tokio::spawn(async move {
            let _ = handle.extended_data(channel, 1, message.into_bytes()).await;
            finish(&handle, channel, 1).await;
        });
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let command = String::from_utf8_lossy(data).into_owned();
        let handle = session.handle();
        let Some(request) = parse_command(&command) else {
            session.channel_success(channel)?;
            tokio::spawn(async move {
                Self::fail(&handle, channel, "keel: only git-upload-pack and git-receive-pack are supported").await;
            });
            return Ok(());
        };
        session.channel_success(channel)?;
        if let Err(e) = self.start_git(channel, request, handle.clone()).await {
            tracing::error!("ssh exec: {e:#}");
            tokio::spawn(async move { Self::fail(&handle, channel, "keel: internal error").await });
        }
        Ok(())
    }

    async fn data(&mut self, channel: ChannelId, data: &[u8], _session: &mut Session) -> Result<(), Self::Error> {
        if let Some(tx) = self.stdin.get(&channel)
            && tx.send(data.to_vec()).await.is_err()
        {
            self.stdin.remove(&channel);
        }
        Ok(())
    }

    async fn channel_eof(&mut self, channel: ChannelId, _session: &mut Session) -> Result<(), Self::Error> {
        // Closing our sender ends the writer task, which closes git's stdin.
        self.stdin.remove(&channel);
        Ok(())
    }

    async fn channel_close(&mut self, channel: ChannelId, _session: &mut Session) -> Result<(), Self::Error> {
        self.stdin.remove(&channel);
        self.protocol.remove(&channel);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_git_commands() {
        let expect = |service| {
            Some(GitRequest {
                service,
                owner: "ada".into(),
                repo: "engine".into(),
            })
        };
        assert_eq!(parse_command("git-upload-pack 'ada/engine.git'"), expect(Service::UploadPack));
        assert_eq!(parse_command("git-upload-pack '/ada/engine.git'"), expect(Service::UploadPack));
        assert_eq!(parse_command("git-receive-pack 'ada/engine'"), expect(Service::ReceivePack));
        assert_eq!(parse_command("git upload-pack 'ada/engine.git'"), expect(Service::UploadPack));
        assert_eq!(parse_command("git-upload-pack ada/engine.git"), expect(Service::UploadPack));
        assert_eq!(parse_command("git-upload-archive 'ada/engine.git'"), None);
        assert_eq!(parse_command("git-upload-pack 'ada/x/y'"), None);
        assert_eq!(parse_command("git-upload-pack '/engine'"), None);
        assert_eq!(parse_command("rm -rf /"), None);
    }
}
