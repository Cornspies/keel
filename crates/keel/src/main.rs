use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use keel_core::{Config, Core, MailConfig, SshConfig};
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
    Serve(ServeArgs),
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
            tokio::runtime::Runtime::new()?.block_on(serve(args))
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

fn check_git() -> anyhow::Result<()> {
    let out = keel_git::command()
        .arg("--version")
        .output()
        .context("`git` was not found in PATH; keel needs it to serve clones and pushes")?;
    if !out.status.success() {
        bail!("`git --version` failed");
    }
    tracing::info!("using {}", String::from_utf8_lossy(&out.stdout).trim());
    Ok(())
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
    };
    let core = Core::open(config)
        .await
        .context("opening the data directory")?;

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
}
