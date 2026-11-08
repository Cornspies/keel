use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use keel_core::{Config, Core, MailConfig, Network, SshConfig};
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(version, about = "A fast, self-hosted git server")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the web UI, git over HTTP and the SSH server.
    Serve(Box<ServeArgs>),
    /// Copy a data directory, consistently, while the server runs. The copy
    /// is a data directory itself: restore by pointing --data-dir at it.
    Backup {
        /// The data directory to back up.
        #[arg(long, env = "KEEL_DATA_DIR", default_value = "data")]
        data_dir: PathBuf,
        /// Where to put the copy (a new or empty directory).
        out: PathBuf,
    },
    /// Git hooks; run by git during pushes, not by people.
    #[command(hide = true)]
    Hook {
        /// The hook, e.g. `pre-receive`.
        name: String,
    },
}

#[derive(clap::Args)]
struct ServeArgs {
    /// Where the database, repositories and SSH host key are stored.
    #[arg(long, env = "KEEL_DATA_DIR", default_value = "data")]
    data_dir: PathBuf,

    /// Address for the web UI and git over HTTP.
    #[arg(long, env = "KEEL_HTTP_ADDR", default_value = "0.0.0.0:3000")]
    http_addr: SocketAddr,

    /// Public URL of the web UI, used in clone URLs and links
    /// [default: http://localhost:<http port>].
    #[arg(long, env = "KEEL_BASE_URL")]
    base_url: Option<String>,

    /// Address for the built-in SSH server.
    #[arg(long, env = "KEEL_SSH_ADDR", default_value = "0.0.0.0:2222")]
    ssh_addr: SocketAddr,

    /// Disable the built-in SSH server.
    #[arg(long, env = "KEEL_NO_SSH")]
    no_ssh: bool,

    /// Host name shown in SSH clone URLs [default: the host of --base-url].
    #[arg(long, env = "KEEL_SSH_HOST")]
    ssh_host: Option<String>,

    /// Port shown in SSH clone URLs, if it differs from the listen port
    /// (e.g. when forwarding port 22) [default: the port of --ssh-addr].
    #[arg(long, env = "KEEL_SSH_PORT")]
    ssh_port: Option<u16>,

    /// Only allow the first account to sign up.
    #[arg(long, env = "KEEL_CLOSED_REGISTRATION")]
    closed_registration: bool,

    /// Name shown in the page header.
    #[arg(long, env = "KEEL_SITE_NAME", default_value = "keel")]
    site_name: String,

    /// Let webhooks reach loopback and private network addresses (e.g. a
    /// CI server on the same network). Off by default, so repository
    /// admins can't use webhooks to probe the server's network.
    #[arg(long, env = "KEEL_WEBHOOK_ALLOW_PRIVATE")]
    webhook_allow_private: bool,

    /// SMTP server for email (notifications, address verification,
    /// password resets), e.g. smtps://user:password@smtp.example.com or
    /// smtp://user:password@smtp.example.com:587?tls=required. Prefer the
    /// environment variable, so the password doesn't show up in `ps`.
    #[arg(long, env = "KEEL_SMTP_URL", hide_env_values = true)]
    smtp_url: Option<String>,

    /// The largest file Git LFS accepts, in MiB.
    #[arg(long, env = "KEEL_LFS_MAX_FILE_SIZE", default_value_t = 2048)]
    lfs_max_file_size: u64,

    /// The largest push accepted, in MiB.
    #[arg(long, env = "KEEL_MAX_PUSH_SIZE", default_value_t = 4096)]
    max_push_size: u64,

    /// Storage per account, in MiB: its repositories (with wikis), Git LFS
    /// files and release files. Pushes, uploads, imports and mirrors stop
    /// once it's used up. Administrators have no limit [default: none].
    #[arg(long, env = "KEEL_QUOTA")]
    quota: Option<u64>,

    /// Repositories per account (forks, mirrors and imports included).
    /// Administrators have no limit [default: none].
    #[arg(long, env = "KEEL_MAX_REPOS")]
    max_repos: Option<u64>,

    /// Networks webhooks, mirrors and imports may not reach, besides
    /// private ones, comma-separated (e.g. the server's own public address:
    /// 203.0.113.7,2001:db8::7).
    #[arg(long, env = "KEEL_BLOCK_OUTBOUND", value_delimiter = ',', value_parser = parse_network)]
    block_outbound: Vec<Network>,

    /// The key two-factor secrets, webhook secrets, mirror passwords and
    /// import tokens are encrypted with in the database; created if
    /// missing. Keep it outside the data directory to keep it out of its
    /// backups [default: <data dir>/secret.key].
    #[arg(long, env = "KEEL_SECRET_KEY_FILE")]
    secret_key_file: Option<PathBuf>,

    /// Sign in with an OpenID Connect provider: its issuer URL. Register
    /// keel there as a confidential client with the redirect URI
    /// <base URL>/login/oidc/callback.
    #[arg(long, env = "KEEL_OIDC_ISSUER", requires_all = ["oidc_client_id", "oidc_client_secret"])]
    oidc_issuer: Option<String>,

    /// The client ID keel has at the provider.
    #[arg(long, env = "KEEL_OIDC_CLIENT_ID", requires = "oidc_issuer")]
    oidc_client_id: Option<String>,

    /// The client secret keel has at the provider (prefer the environment
    /// variable).
    #[arg(long, env = "KEEL_OIDC_CLIENT_SECRET", hide_env_values = true, requires = "oidc_issuer")]
    oidc_client_secret: Option<String>,

    /// What the sign-in button calls the provider: "Sign in with <name>".
    #[arg(long, env = "KEEL_OIDC_NAME", default_value = "single sign-on")]
    oidc_name: String,

    /// Create accounts for whoever the provider signs in, even with
    /// --closed-registration (for a provider with only your own people).
    #[arg(long, env = "KEEL_OIDC_ALLOW_SIGNUP")]
    oidc_allow_signup: bool,

    /// Sign in to an existing account with the same address when both the
    /// provider and keel verified it, connecting the two. Only for a
    /// provider whose email_verified you trust: it vouches for every
    /// account's address. Never for site administrators or accounts with
    /// two-factor authentication. Without it, people connect existing
    /// accounts in Settings → Security.
    #[arg(long, env = "KEEL_OIDC_LINK_BY_EMAIL", requires = "oidc_issuer")]
    oidc_link_by_email: bool,

    /// Serve Prometheus metrics at /metrics to requests with this bearer
    /// token (off without one).
    #[arg(long, env = "KEEL_METRICS_TOKEN", hide_env_values = true)]
    metrics_token: Option<String>,

    /// The From address of emails [default: "<site name> <noreply@<host of
    /// --base-url>>"].
    #[arg(long, env = "KEEL_MAIL_FROM")]
    mail_from: Option<String>,
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Serve(args) => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,tower_http=warn")),
                )
                .init();
            tokio::runtime::Runtime::new()?.block_on(serve(*args))
        }
        Command::Backup { data_dir, out } => {
            let report = tokio::runtime::Runtime::new()?.block_on(keel_core::backup(&data_dir, &out))?;
            println!(
                "Backed up {} to {}: the database, {} repositories{}, {} wikis, {} large files, {} release files; {:.1} MB in {:.1}s.",
                data_dir.display(),
                out.display(),
                report.repos,
                if report.skipped > 0 { format!(" ({} deleted meanwhile, skipped)", report.skipped) } else { String::new() },
                report.wikis,
                report.lfs_objects,
                report.release_files,
                report.bytes as f64 / 1e6,
                report.took.as_secs_f64(),
            );
            if !data_dir.join("secret.key").is_file() {
                println!(
                    "The key the database's secrets are encrypted with isn't in the data directory \
                     (--secret-key-file): keep a copy of it, a restore needs it."
                );
            }
            Ok(())
        }
        Command::Hook { name } if name == "pre-receive" => std::process::exit(keel_core::pre_receive()),
        Command::Hook { name } => bail!("unknown hook {name}"),
    }
}

fn host_of(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let authority = rest.split('/').next()?;
    let host = match authority.strip_prefix('[') {
        // IPv6 literal: keep the brackets.
        Some(v6) => format!("[{}]", v6.split(']').next()?),
        None => authority.split(':').next()?.to_owned(),
    };
    (!host.is_empty()).then_some(host)
}

fn parse_network(s: &str) -> Result<Network, String> {
    Network::parse(s).ok_or_else(|| format!("{s:?} isn't an address or network like 203.0.113.0/24"))
}

/// The oldest git keel works with (`merge-tree --write-tree`).
const MIN_GIT: (u32, u32) = (2, 38);

fn check_git() -> anyhow::Result<()> {
    let out = keel_git::command()
        .arg("--version")
        .output()
        .context("`git` was not found in PATH; keel needs it to serve clones and pushes")?;
    if !out.status.success() {
        bail!("`git --version` failed");
    }
    let version = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    match git_version(&version) {
        Some(v) if v >= MIN_GIT => {}
        Some(_) => bail!("{version} is too old: keel needs git {}.{} or newer", MIN_GIT.0, MIN_GIT.1),
        None => tracing::warn!("couldn't tell the version of {version:?}"),
    }
    tracing::info!("using {version}");
    Ok(())
}

/// Major and minor version of `git version 2.43.0` (or `2.39.5 (Apple Git-154)`).
fn git_version(output: &str) -> Option<(u32, u32)> {
    let mut parts = output.strip_prefix("git version ")?.split(['.', ' ']);
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

async fn serve(args: ServeArgs) -> anyhow::Result<()> {
    check_git()?;
    let base_url = args
        .base_url
        .unwrap_or_else(|| format!("http://localhost:{}", args.http_addr.port()))
        .trim_end_matches('/')
        .to_owned();
    let ssh = (!args.no_ssh).then(|| SshConfig {
        public_host: args
            .ssh_host
            .or_else(|| host_of(&base_url))
            .unwrap_or_else(|| "localhost".into()),
        public_port: args.ssh_port.unwrap_or(args.ssh_addr.port()),
    });
    let mail = args.smtp_url.map(|smtp_url| MailConfig {
        from: args.mail_from.unwrap_or_else(|| {
            let host = host_of(&base_url).unwrap_or_else(|| "localhost".into());
            format!("{} <noreply@{}>", args.site_name, host.trim_matches(['[', ']']))
        }),
        smtp_url,
    });
    let config = Config {
        data_dir: args.data_dir,
        base_url,
        ssh,
        open_registration: !args.closed_registration,
        site_name: args.site_name,
        webhook_allow_private: args.webhook_allow_private,
        lfs_max_bytes: args.lfs_max_file_size.saturating_mul(1 << 20),
        metrics_token: args.metrics_token.filter(|t| !t.is_empty()),
        mail,
        require_setup_code: true,
        max_push_bytes: args.max_push_size.saturating_mul(1 << 20),
        quota_bytes: args.quota.map(|q| q.saturating_mul(1 << 20)),
        max_repos: args.max_repos,
        blocked_networks: args.block_outbound,
        secret_key_file: args.secret_key_file,
        oidc: match (args.oidc_issuer, args.oidc_client_id, args.oidc_client_secret) {
            (Some(issuer), Some(client_id), Some(client_secret)) => {
                let issuer = issuer.trim_end_matches('/').to_owned();
                if !keel_web::secure_url(&issuer) {
                    bail!("--oidc-issuer must be an https:// URL (http:// only for localhost)");
                }
                Some(keel_core::OidcConfig {
                    issuer,
                    client_id,
                    client_secret,
                    name: args.oidc_name,
                    allow_signup: args.oidc_allow_signup,
                    link_by_email: args.oidc_link_by_email,
                })
            }
            _ => None,
        },
    };
    // Easy mistakes when the server goes on the internet.
    if config.base_url.starts_with("https://") && args.http_addr.ip().is_unspecified() {
        tracing::warn!(
            "listening on {} with an https:// public URL: if a reverse proxy serves it, listen on 127.0.0.1 \
             (--http-addr 127.0.0.1:{}) so nobody can reach keel past it, unencrypted",
            args.http_addr,
            args.http_addr.port()
        );
    }
    if config.open_registration && (config.quota_bytes.is_none() || config.max_repos.is_none()) {
        tracing::warn!(
            "anyone can sign up, and accounts have no --quota or --max-repos limit: \
             consider --closed-registration, or set both"
        );
    }
    let core = Core::open(config)
        .await
        .context("opening the data directory")?;

    if core.config.quota_bytes.is_some() {
        // Repositories from before quotas existed haven't been measured.
        let core = core.clone();
        tokio::spawn(async move {
            if let Err(e) = core.measure_repos().await {
                tracing::warn!("measuring disk usage failed: {e}");
            }
        });
    }
    keel_hooks::start(core.clone());
    keel_mail::start(core.clone()).context("setting up email")?;
    keel_mirror::start(core.clone());
    let app = keel_web::router(keel_web::AppState::new(core.clone()));
    let listener = tokio::net::TcpListener::bind(args.http_addr)
        .await
        .with_context(|| format!("binding {}", args.http_addr))?;
    tracing::info!(
        "web UI listening on {} (public URL {})",
        args.http_addr,
        core.config.base_url
    );
    let web = axum::serve(listener, app).with_graceful_shutdown(shutdown_signal());

    if args.no_ssh {
        web.await?;
    } else {
        tokio::select! {
            r = web => r?,
            r = keel_ssh::serve(core, args.ssh_addr) => r?,
        }
    }
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = tokio::signal::ctrl_c();
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
    tracing::info!("shutting down");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts() {
        assert_eq!(host_of("https://git.example.com").as_deref(), Some("git.example.com"));
        assert_eq!(host_of("http://localhost:3000/x").as_deref(), Some("localhost"));
        assert_eq!(host_of("http://[::1]:3000").as_deref(), Some("[::1]"));
        assert_eq!(host_of("nonsense"), None);
    }

    #[test]
    fn git_versions() {
        assert_eq!(git_version("git version 2.43.0"), Some((2, 43)));
        assert_eq!(git_version("git version 2.39.5 (Apple Git-154)"), Some((2, 39)));
        assert_eq!(git_version("git version 2.45.windows.1"), Some((2, 45)));
        assert_eq!(git_version("nonsense"), None);
        assert!(git_version("git version 2.37.1").unwrap() < MIN_GIT);
    }
}
