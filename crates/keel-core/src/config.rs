use std::net::IpAddr;
use std::path::PathBuf;

#[derive(Clone, Debug)]
pub struct Config {
    /// Holds the database, repositories and SSH host key.
    pub data_dir: PathBuf,
    /// Public URL of the web UI without a trailing slash, used for clone URLs.
    pub base_url: String,
    /// `None` when the built-in SSH server is disabled.
    pub ssh: Option<SshConfig>,
    /// Whether anyone may sign up. The first account can always be created
    /// and becomes the administrator.
    pub open_registration: bool,
    pub site_name: String,
    /// Whether webhooks may target loopback and private network addresses
    /// (off by default, so repository admins can't probe the server's
    /// network).
    pub webhook_allow_private: bool,
    /// `None` when no mail server is configured: no email is sent and
    /// password resets are unavailable.
    pub mail: Option<MailConfig>,
    /// The largest file Git LFS accepts, in bytes.
    pub lfs_max_bytes: u64,
    /// Bearer token for `/metrics`; without one, metrics are off.
    pub metrics_token: Option<String>,
    /// The first account (the administrator) needs the one-time setup code
    /// keel logs when it starts without accounts. `keel serve` turns this
    /// on; without it, whoever signs up first owns the server.
    pub require_setup_code: bool,
    /// The largest push accepted, in bytes (git's `receive.maxInputSize`).
    pub max_push_bytes: u64,
    /// Storage per account (its repositories with their wikis, Git LFS
    /// files and release files), in bytes; administrators have none.
    pub quota_bytes: Option<u64>,
    /// Repositories per account; administrators have no limit.
    pub max_repos: Option<u64>,
    /// Networks webhooks, mirrors and imports may not reach, besides the
    /// private ones (e.g. the server's own public addresses).
    pub blocked_networks: Vec<Network>,
}

/// An IP network, like `203.0.113.7/32` or `2001:db8::/48`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Network {
    pub addr: IpAddr,
    pub prefix: u8,
}

impl Network {
    /// Parses `addr/prefix`, or a lone address (the whole address).
    pub fn parse(s: &str) -> Option<Self> {
        let (addr, prefix) = match s.trim().split_once('/') {
            Some((a, p)) => (a.parse::<IpAddr>().ok()?, Some(p.parse::<u8>().ok()?)),
            None => (s.trim().parse::<IpAddr>().ok()?, None),
        };
        let max = if addr.is_ipv4() { 32 } else { 128 };
        let prefix = prefix.unwrap_or(max);
        (prefix <= max).then_some(Self { addr, prefix })
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        // IPv4 addresses inside IPv6 (`::ffff:a.b.c.d`) count as IPv4.
        let ip = match ip {
            IpAddr::V6(v6) => v6.to_ipv4_mapped().map_or(ip, IpAddr::V4),
            v4 => v4,
        };
        let bits = |a: IpAddr| -> u128 {
            match a {
                IpAddr::V4(v4) => u128::from(u32::from(v4)) << 96,
                IpAddr::V6(v6) => u128::from(v6),
            }
        };
        if ip.is_ipv4() != self.addr.is_ipv4() {
            return false;
        }
        // IPv4 bits sit at the top, so prefixes count from there either way.
        let mask = if self.prefix == 0 { 0 } else { u128::MAX << (128 - u32::from(self.prefix)) };
        bits(ip) & mask == bits(self.addr) & mask
    }
}

#[derive(Clone, Debug)]
pub struct MailConfig {
    /// e.g. `smtps://user:password@smtp.example.com` (TLS),
    /// `smtp://user:password@smtp.example.com:587?tls=required` (STARTTLS)
    /// or `smtp://localhost:25` (a local relay).
    pub smtp_url: String,
    /// The From address, e.g. `keel <git@example.com>`.
    pub from: String,
}

#[derive(Clone, Debug)]
pub struct SshConfig {
    /// Host and port users connect to (may differ from the listen address,
    /// e.g. behind NAT).
    pub public_host: String,
    pub public_port: u16,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data_dir: "data".into(),
            base_url: "http://localhost:3000".into(),
            ssh: None,
            open_registration: true,
            site_name: "keel".into(),
            webhook_allow_private: false,
            lfs_max_bytes: 2 << 30,
            metrics_token: None,
            mail: None,
            require_setup_code: false,
            max_push_bytes: 4 << 30,
            quota_bytes: None,
            max_repos: None,
            blocked_networks: Vec::new(),
        }
    }
}

impl Config {
    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("keel.db")
    }

    /// Git hooks keel runs (see `protection.rs`).
    pub fn hooks_dir(&self) -> PathBuf {
        self.data_dir.join("hooks")
    }

    pub fn repos_dir(&self) -> PathBuf {
        self.data_dir.join("repos")
    }

    pub fn ssh_host_key_path(&self) -> PathBuf {
        self.data_dir.join("ssh_host_ed25519_key")
    }

    /// Repositories live on disk by id, so renames never touch the
    /// filesystem and names never end up in paths.
    pub fn repo_path(&self, repo_id: i64) -> PathBuf {
        self.repos_dir().join(format!("{repo_id}.git"))
    }

    pub fn http_clone_url(&self, owner: &str, repo: &str) -> String {
        format!("{}/{owner}/{repo}.git", self.base_url)
    }

    pub fn ssh_clone_url(&self, owner: &str, repo: &str) -> Option<String> {
        let ssh = self.ssh.as_ref()?;
        Some(if ssh.public_port == 22 {
            format!("git@{}:{owner}/{repo}.git", ssh.public_host)
        } else {
            format!("ssh://git@{}:{}/{owner}/{repo}.git", ssh.public_host, ssh.public_port)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn networks() {
        let net = Network::parse("203.0.113.0/24").unwrap();
        assert!(net.contains("203.0.113.7".parse().unwrap()));
        assert!(net.contains("::ffff:203.0.113.7".parse().unwrap()));
        assert!(!net.contains("203.0.114.7".parse().unwrap()));
        assert!(!net.contains("2001:db8::1".parse().unwrap()));
        let one = Network::parse("198.51.100.9").unwrap();
        assert!(one.contains("198.51.100.9".parse().unwrap()) && !one.contains("198.51.100.10".parse().unwrap()));
        let v6 = Network::parse("2001:db8:1::/48").unwrap();
        assert!(v6.contains("2001:db8:1:ffff::1".parse().unwrap()));
        assert!(!v6.contains("2001:db8:2::1".parse().unwrap()));
        assert!(Network::parse("0.0.0.0/0").unwrap().contains("8.8.8.8".parse().unwrap()));
        for bad in ["", "nope", "1.2.3.4/33", "::1/129", "1.2.3.4/x"] {
            assert!(Network::parse(bad).is_none(), "{bad}");
        }
    }
}
