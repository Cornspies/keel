use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use keel_core::{Config, Core, SshConfig};
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
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info,tower_http=warn")),
        )
        .init();
    match Cli::parse().command {
        Command::Serve(args) => serve(args).await,
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
    let out = std::process::Command::new("git")
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
    let config = Config {
        data_dir: args.data_dir,
        base_url,
        ssh,
        open_registration: !args.closed_registration,
        site_name: args.site_name,
    };
    let core = Core::open(config)
        .await
        .context("opening the data directory")?;

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
