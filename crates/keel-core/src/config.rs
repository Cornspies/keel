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
        }
    }
}

impl Config {
    pub fn db_path(&self) -> PathBuf {
        self.data_dir.join("keel.db")
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
