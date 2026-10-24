//! Webhooks: repository events POSTed as JSON to other services.
//!
//! When something happens, a delivery with the finished payload is queued
//! in the database for every interested hook (see `activity.rs`); a
//! background worker (the `keel-hooks` crate) sends due deliveries and
//! records the outcome, retrying failures with backoff. Queued deliveries
//! survive restarts.

use std::net::IpAddr;

use sha2::{Digest, Sha256};

use crate::{Core, Error, Result, invalid, now};

/// Events a hook can subscribe to, with what they cover.
pub const WEBHOOK_EVENTS: &[(&str, &str)] = &[
    ("push", "Pushes to branches and tags, including their creation and deletion"),
    ("issues", "Issues opened, closed, reopened, edited, labeled or assigned"),
    ("issue_comment", "Comments on issues and pull requests"),
    ("pull_request", "Pull requests opened, closed, merged, reopened, edited, labeled, assigned or updated"),
    ("pull_request_review", "Pull request approvals and change requests"),
    ("pull_request_review_comment", "Comments on lines of a pull request's diff"),
    ("status", "Commit statuses set by CI and other services"),
    ("release", "Releases published, edited or deleted"),
    ("gollum", "Wiki pages created or edited"),
    ("star", "Repositories starred or unstarred"),
];

/// Retry delays after each failed attempt; after the last one the delivery
/// is given up.
const RETRY_DELAYS: &[i64] = &[30, 120, 600, 3600];
/// Deliveries kept per hook for the log.
const KEEP_DELIVERIES: i64 = 100;
/// How long a claimed delivery is reserved for its attempt.
const LEASE_SECS: i64 = 120;

/// What a hook belongs to: a repository, or an organization (whose hooks
/// get the events of all its repositories).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookOwner {
    Repo(i64),
    Org(i64),
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Webhook {
    pub id: i64,
    pub repo_id: Option<i64>,
    pub org_id: Option<i64>,
    pub url: String,
    pub secret: String,
    /// Comma-separated event names, or `*`.
    pub events: String,
    pub active: bool,
    pub created_at: i64,
}

impl Webhook {
    pub fn owner(&self) -> HookOwner {
        match (self.repo_id, self.org_id) {
            (Some(repo), _) => HookOwner::Repo(repo),
            (None, org) => HookOwner::Org(org.unwrap_or_default()),
        }
    }

    pub fn wants(&self, event: &str) -> bool {
        event == "ping" || self.events == "*" || self.events.split(',').any(|e| e == event)
    }

    pub fn event_list(&self) -> Vec<String> {
        if self.events == "*" {
            return WEBHOOK_EVENTS.iter().map(|(e, _)| (*e).to_owned()).collect();
        }
        self.events.split(',').filter(|e| !e.is_empty()).map(str::to_owned).collect()
    }
}

#[derive(Clone, Debug, sqlx::FromRow)]
pub struct Delivery {
    pub id: i64,
    pub hook_id: i64,
    pub guid: String,
    pub event: String,
    pub action: String,
    pub payload: String,
    /// `pending`, `ok` or `failed`.
    pub status: String,
    pub attempts: i64,
    pub next_attempt_at: i64,
    pub response_status: Option<i64>,
    pub response_body: Option<String>,
    pub error: Option<String>,
    pub duration_ms: Option<i64>,
    pub created_at: i64,
    pub delivered_at: Option<i64>,
}

/// What happened when a delivery was attempted.
#[derive(Clone, Debug)]
pub struct Attempt {
    /// HTTP status, if a response arrived.
    pub status: Option<u16>,
    /// The start of the response body.
    pub body: String,
    /// Why there was no (successful) response.
    pub error: Option<String>,
    pub duration_ms: i64,
}

impl Attempt {
    pub fn succeeded(&self) -> bool {
        self.error.is_none() && self.status.is_some_and(|s| (200..300).contains(&s))
    }
}

/// `sha256=<hex HMAC-SHA256 of body>`, the `X-Hub-Signature-256` format.
pub fn sign(secret: &str, body: &[u8]) -> String {
    const BLOCK: usize = 64;
    let mut key = [0u8; BLOCK];
    if secret.len() > BLOCK {
        key[..32].copy_from_slice(&Sha256::digest(secret.as_bytes()));
    } else {
        key[..secret.len()].copy_from_slice(secret.as_bytes());
    }
    let pad = |byte: u8| key.map(|k| k ^ byte);
    let inner = Sha256::new().chain_update(pad(0x36)).chain_update(body).finalize();
    let outer = Sha256::new().chain_update(pad(0x5c)).chain_update(inner).finalize();
    format!("sha256={}", hex::encode(outer))
}

/// Whether an address is one webhooks may not reach unless private targets
/// are allowed: loopback, private networks, link-local, and the like.
pub fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
                || a == 0
                || (a == 100 && (64..128).contains(&b)) // carrier-grade NAT
                || a >= 224 // multicast and reserved
        }
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => is_private_ip(IpAddr::V4(v4)),
            None => {
                let first = v6.segments()[0];
                v6.is_loopback()
                    || v6.is_unspecified()
                    || v6.is_multicast()
                    || (first & 0xfe00) == 0xfc00 // unique local
                    || (first & 0xffc0) == 0xfe80 // link-local
            }
        },
    }
}

/// Checks a hook URL as far as possible without resolving it (delivery
/// checks resolved addresses again).
fn validate_url(url: &str, allow_private: bool) -> Result<()> {
    let url = url.trim();
    let Some(rest) = url.strip_prefix("https://").or_else(|| url.strip_prefix("http://")) else {
        return invalid("Webhook URLs start with http:// or https://.");
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.is_empty() || authority.contains('@') || url.len() > 2048 || url.contains(char::is_whitespace) {
        return invalid("That doesn't look like a valid URL.");
    }
    let host = match authority.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => authority.rsplit_once(':').map_or(authority, |(h, _)| h),
    };
    if !allow_private {
        let local = host.eq_ignore_ascii_case("localhost") || host.to_ascii_lowercase().ends_with(".localhost");
        if local || host.parse::<IpAddr>().is_ok_and(is_private_ip) {
            return invalid(
                "Webhooks can't reach private or local addresses on this server \
                 (the administrator can allow it with --webhook-allow-private).",
            );
        }
    }
    Ok(())
}

fn validate_events(events: &[String]) -> Result<String> {
    if events.is_empty() {
        return invalid("Choose at least one event.");
    }
    if events.iter().any(|e| !WEBHOOK_EVENTS.iter().any(|(name, _)| name == e)) {
        return invalid("Unknown event.");
    }
    if events.len() == WEBHOOK_EVENTS.len() {
        return Ok("*".into());
    }
    Ok(events.join(","))
}

pub(crate) fn new_guid() -> String {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("system randomness");
    bytes[6] = (bytes[6] & 0x0f) | 0x40; // UUID version 4
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let h = hex::encode(bytes);
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

macro_rules! delivery_columns {
    () => {
        "id, hook_id, guid, event, action, payload, status, attempts, next_attempt_at, response_status,
         response_body, error, duration_ms, created_at, delivered_at"
    };
}

impl Core {
    /// The hooks of a repository or an organization.
    pub async fn webhooks(&self, owner: HookOwner) -> Result<Vec<Webhook>> {
        let (repo, org) = match owner {
            HookOwner::Repo(id) => (Some(id), None),
            HookOwner::Org(id) => (None, Some(id)),
        };
        Ok(sqlx::query_as("SELECT * FROM webhooks WHERE repo_id IS ?1 AND org_id IS ?2 ORDER BY id")
            .bind(repo)
            .bind(org)
            .fetch_all(&self.db)
            .await?)
    }

    /// Hooks that get a repository's events: its own and its
    /// organization's.
    pub(crate) async fn hooks_for_repo(&self, repo_id: i64) -> Result<Vec<Webhook>> {
        Ok(sqlx::query_as(
            "SELECT * FROM webhooks WHERE repo_id = ?1
                OR org_id = (SELECT owner_id FROM repositories WHERE id = ?1)
             ORDER BY id",
        )
        .bind(repo_id)
        .fetch_all(&self.db)
        .await?)
    }

    pub async fn webhook(&self, owner: HookOwner, id: i64) -> Result<Option<Webhook>> {
        Ok(self.webhooks(owner).await?.into_iter().find(|h| h.id == id))
    }

    /// Creates a hook and queues a `ping` delivery to it.
    pub async fn create_webhook(
        &self,
        owner: HookOwner,
        url: &str,
        secret: &str,
        events: &[String],
        active: bool,
    ) -> Result<Webhook> {
        validate_url(url, self.config.webhook_allow_private)?;
        let events = validate_events(events)?;
        let (repo, org) = match owner {
            HookOwner::Repo(id) => (Some(id), None),
            HookOwner::Org(id) => (None, Some(id)),
        };
        let (id,): (i64,) = sqlx::query_as(
            "INSERT INTO webhooks (repo_id, org_id, url, secret, events, active, created_at)
             VALUES (?1, ?7, ?2, ?3, ?4, ?5, ?6) RETURNING id",
        )
        .bind(repo)
        .bind(url.trim())
        .bind(secret.trim())
        .bind(events)
        .bind(active)
        .bind(now())
        .bind(org)
        .fetch_one(&self.db)
        .await?;
        let hook = self.webhook(owner, id).await?.ok_or(Error::NotFound)?;
        self.ping_webhook(&hook).await?;
        Ok(hook)
    }

    /// Updates a hook; `secret` = `None` keeps the current one.
    pub async fn update_webhook(
        &self,
        hook: &Webhook,
        url: &str,
        secret: Option<&str>,
        events: &[String],
        active: bool,
    ) -> Result<()> {
        validate_url(url, self.config.webhook_allow_private)?;
        let events = validate_events(events)?;
        sqlx::query(
            "UPDATE webhooks SET url = ?2, secret = COALESCE(?3, secret), events = ?4, active = ?5 WHERE id = ?1",
        )
        .bind(hook.id)
        .bind(url.trim())
        .bind(secret.map(str::trim))
        .bind(events)
        .bind(active)
        .execute(&self.db)
        .await?;
        Ok(())
    }

    pub async fn delete_webhook(&self, hook: &Webhook) -> Result<()> {
        sqlx::query("DELETE FROM webhooks WHERE id = ?1").bind(hook.id).execute(&self.db).await?;
        Ok(())
    }

    /// Queues a `ping` to check that a hook is reachable.
    pub async fn ping_webhook(&self, hook: &Webhook) -> Result<()> {
        let kind = match hook.owner() {
            HookOwner::Repo(_) => "Repository",
            HookOwner::Org(_) => "Organization",
        };
        let mut payload = serde_json::json!({
            "zen": "Keep it simple.",
            "hook_id": hook.id,
            "hook": { "id": hook.id, "type": kind, "active": hook.active, "events": hook.event_list(),
                      "config": { "url": hook.url, "content_type": "json" } },
        });
        match hook.owner() {
            HookOwner::Repo(id) => payload["repository"] = serde_json::json!(self.repo_by_id(id).await?.map(|r| self.repo_json(&r))),
            HookOwner::Org(id) => payload["organization"] = serde_json::json!(self.user_by_id(id).await?.map(|o| self.user_json(&o))),
        }
        self.insert_delivery(hook.id, "ping", "", &payload.to_string()).await
    }

    /// Queues `payload` for every active hook of the repository (or its
    /// organization) that wants `event`.
    pub(crate) async fn enqueue_webhooks(&self, repo_id: i64, event: &str, action: &str, payload: &serde_json::Value) -> Result<()> {
        let hooks = self.hooks_for_repo(repo_id).await?;
        let hooks: Vec<_> = hooks.iter().filter(|h| h.active && h.wants(event)).collect();
        if hooks.is_empty() {
            return Ok(());
        }
        let body = payload.to_string();
        for hook in hooks {
            self.insert_delivery(hook.id, event, action, &body).await?;
        }
        Ok(())
    }

    async fn insert_delivery(&self, hook_id: i64, event: &str, action: &str, payload: &str) -> Result<()> {
        let now = now();
        sqlx::query(
            "INSERT INTO webhook_deliveries (hook_id, guid, event, action, payload, next_attempt_at, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)",
        )
        .bind(hook_id)
        .bind(new_guid())
        .bind(event)
        .bind(action)
        .bind(payload)
        .bind(now)
        .execute(&self.db)
        .await?;
        // Keep the log bounded (but never drop what's still queued).
        sqlx::query(
            "DELETE FROM webhook_deliveries WHERE hook_id = ?1 AND status != 'pending' AND id <= (
                SELECT id FROM webhook_deliveries WHERE hook_id = ?1 ORDER BY id DESC LIMIT 1 OFFSET ?2)",
        )
        .bind(hook_id)
        .bind(KEEP_DELIVERIES)
        .execute(&self.db)
        .await?;
        self.hook_wakeup.notify_one();
        Ok(())
    }

    pub async fn deliveries(&self, hook_id: i64, limit: i64) -> Result<Vec<Delivery>> {
        Ok(sqlx::query_as(concat!(
            "SELECT ",
            delivery_columns!(),
            " FROM webhook_deliveries WHERE hook_id = ?1 ORDER BY id DESC LIMIT ?2"
        ))
        .bind(hook_id)
        .bind(limit)
        .fetch_all(&self.db)
        .await?)
    }

    pub async fn delivery(&self, hook_id: i64, id: i64) -> Result<Option<Delivery>> {
        Ok(sqlx::query_as(concat!(
            "SELECT ",
            delivery_columns!(),
            " FROM webhook_deliveries WHERE hook_id = ?1 AND id = ?2"
        ))
        .bind(hook_id)
        .bind(id)
        .fetch_optional(&self.db)
        .await?)
    }

    /// Queues a delivery again, as a new delivery with the same payload.
    pub async fn redeliver(&self, delivery: &Delivery) -> Result<()> {
        self.insert_delivery(delivery.hook_id, &delivery.event, &delivery.action, &delivery.payload).await
    }

    // -- For the delivery worker ------------------------------------------

    /// Wakes the delivery worker when something is queued.
    pub fn hook_wakeup(&self) -> std::sync::Arc<tokio::sync::Notify> {
        self.hook_wakeup.clone()
    }

    /// Reserves up to `limit` due deliveries (with their hooks) for sending.
    pub async fn claim_deliveries(&self, limit: i64) -> Result<Vec<(Delivery, Webhook)>> {
        let now = now();
        let due: Vec<Delivery> = sqlx::query_as(concat!(
            "UPDATE webhook_deliveries SET next_attempt_at = ?1 + ?3
             WHERE id IN (SELECT id FROM webhook_deliveries WHERE status = 'pending' AND next_attempt_at <= ?1
                          ORDER BY next_attempt_at LIMIT ?2)
             RETURNING ",
            delivery_columns!()
        ))
        .bind(now)
        .bind(limit)
        .bind(LEASE_SECS)
        .fetch_all(&self.db)
        .await?;
        let mut out = Vec::with_capacity(due.len());
        for d in due {
            let hook: Option<Webhook> = sqlx::query_as("SELECT * FROM webhooks WHERE id = ?1")
                .bind(d.hook_id)
                .fetch_optional(&self.db)
                .await?;
            if let Some(hook) = hook {
                out.push((d, hook));
            }
        }
        Ok(out)
    }

    /// When the next queued delivery is due, if any.
    pub async fn next_delivery_due(&self) -> Result<Option<i64>> {
        let (due,): (Option<i64>,) =
            sqlx::query_as("SELECT MIN(next_attempt_at) FROM webhook_deliveries WHERE status = 'pending'")
                .fetch_one(&self.db)
                .await?;
        Ok(due)
    }

    /// Records an attempt; failures are retried later until they run out.
    pub async fn finish_delivery(&self, delivery: &Delivery, attempt: &Attempt) -> Result<()> {
        let attempts = delivery.attempts + 1;
        let ok = attempt.succeeded();
        let retry = RETRY_DELAYS.get(delivery.attempts as usize).filter(|_| !ok);
        let status = match (ok, retry) {
            (true, _) => "ok",
            (false, Some(_)) => "pending",
            (false, None) => "failed",
        };
        let now = now();
        let body: String = attempt.body.chars().take(4096).collect();
        sqlx::query(
            "UPDATE webhook_deliveries SET status = ?2, attempts = ?3, next_attempt_at = ?4, response_status = ?5,
                response_body = ?6, error = ?7, duration_ms = ?8, delivered_at = ?9
             WHERE id = ?1",
        )
        .bind(delivery.id)
        .bind(status)
        .bind(attempts)
        .bind(now + retry.copied().unwrap_or(0))
        .bind(attempt.status.map(i64::from))
        .bind(body)
        .bind(&attempt.error)
        .bind(attempt.duration_ms)
        .bind(now)
        .execute(&self.db)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signatures_match_hmac_sha256() {
        // RFC 4231 test case 2.
        assert_eq!(
            sign("Jefe", b"what do ya want for nothing?"),
            "sha256=5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // Keys longer than a block are hashed first.
        let key = "k".repeat(100);
        assert_eq!(sign(&key, b"x").len(), "sha256=".len() + 64);
        assert_ne!(sign(&key, b"x"), sign("k", b"x"));
    }

    #[test]
    fn private_addresses() {
        for ip in ["127.0.0.1", "10.1.2.3", "192.168.0.1", "172.16.0.1", "169.254.169.254", "0.0.0.0", "100.64.0.1", "::1", "fd00::1", "fe80::1", "::ffff:127.0.0.1"] {
            assert!(is_private_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["1.1.1.1", "93.184.216.34", "2606:4700::1111"] {
            assert!(!is_private_ip(ip.parse().unwrap()), "{ip}");
        }
        assert!(validate_url("https://ci.example.com/hook", false).is_ok());
        assert!(validate_url("http://localhost:8080/x", false).is_err());
        assert!(validate_url("http://127.0.0.1/x", false).is_err());
        assert!(validate_url("http://[::1]:80/x", false).is_err());
        assert!(validate_url("http://127.0.0.1/x", true).is_ok());
        assert!(validate_url("ftp://example.com", true).is_err());
        assert!(validate_url("https://user:pw@example.com", true).is_err());
    }

    #[tokio::test]
    async fn queue_retry_and_give_up() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let repo = core.create_repo(&ada, "engine", "", false).await.unwrap();
        assert!(core.create_webhook(HookOwner::Repo(repo.id), "https://ci.example.com", "", &[], true).await.is_err());
        let hook = core
            .create_webhook(HookOwner::Repo(repo.id), "https://ci.example.com/hook", "s3cret", &["push".into()], true)
            .await
            .unwrap();
        assert!(hook.wants("push") && !hook.wants("issues"));

        // Creating a hook pings it.
        let claimed = core.claim_deliveries(10).await.unwrap();
        assert_eq!(claimed.len(), 1);
        assert_eq!(claimed[0].0.event, "ping");
        assert!(core.claim_deliveries(10).await.unwrap().is_empty(), "leased");

        let failed = Attempt { status: Some(500), body: "oops".into(), error: None, duration_ms: 3 };
        core.finish_delivery(&claimed[0].0, &failed).await.unwrap();
        let d = core.delivery(hook.id, claimed[0].0.id).await.unwrap().unwrap();
        assert_eq!((d.status.as_str(), d.attempts, d.response_status), ("pending", 1, Some(500)));
        assert!(d.next_attempt_at > now());

        // Out of retries: failed.
        let mut d = d;
        d.attempts = RETRY_DELAYS.len() as i64;
        core.finish_delivery(&d, &failed).await.unwrap();
        assert_eq!(core.delivery(hook.id, d.id).await.unwrap().unwrap().status, "failed");

        // Events only reach hooks that want them; redelivery copies.
        core.enqueue_webhooks(repo.id, "issues", "opened", &serde_json::json!({})).await.unwrap();
        core.enqueue_webhooks(repo.id, "push", "", &serde_json::json!({"ref": "x"})).await.unwrap();
        let all = core.deliveries(hook.id, 10).await.unwrap();
        assert_eq!(all.len(), 2);
        core.redeliver(&all[0]).await.unwrap();
        let all = core.deliveries(hook.id, 10).await.unwrap();
        assert_eq!((all[0].payload.as_str(), all.len()), (r#"{"ref":"x"}"#, 3));
        assert_ne!(all[0].guid, all[1].guid);

        let ok = Attempt { status: Some(204), body: String::new(), error: None, duration_ms: 1 };
        let claimed = core.claim_deliveries(10).await.unwrap();
        assert_eq!(claimed.len(), 2);
        for (d, _) in &claimed {
            core.finish_delivery(d, &ok).await.unwrap();
        }
        assert_eq!(core.next_delivery_due().await.unwrap(), None);
    }

    #[tokio::test]
    async fn organization_hooks() {
        let (core, _dir) = Core::for_tests().await;
        let ada = core.register("ada", "ada@example.com", "password1").await.unwrap();
        let org = core.create_org(&ada, "acme").await.unwrap();
        let tool = core.create_repo(&org, "tool", "", false).await.unwrap();
        let own = core.create_repo(&ada, "own", "", false).await.unwrap();
        let hook = core
            .create_webhook(HookOwner::Org(org.id), "https://ci.example.com/acme", "", &["issues".into()], true)
            .await
            .unwrap();
        assert_eq!(hook.owner(), HookOwner::Org(org.id));
        assert!(core.webhooks(HookOwner::Repo(tool.id)).await.unwrap().is_empty());
        assert!(core.webhook(HookOwner::Repo(tool.id), hook.id).await.unwrap().is_none());
        let ping: serde_json::Value = serde_json::from_str(&core.deliveries(hook.id, 1).await.unwrap()[0].payload).unwrap();
        assert_eq!(ping["organization"]["login"], "acme");

        // Issues in the organization's repositories reach it; others don't.
        core.create_issue(tool.id, &ada, "In acme", "").await.unwrap();
        core.create_issue(own.id, &ada, "Elsewhere", "").await.unwrap();
        let events: Vec<String> = core.deliveries(hook.id, 10).await.unwrap().into_iter().map(|d| d.payload).collect();
        assert_eq!(events.len(), 2);
        assert!(events[0].contains("In acme"));
    }
}
