//! Sends queued webhook deliveries (see `keel_core::webhooks`).
//!
//! One background task claims due deliveries, POSTs them (a few at a time)
//! and records each outcome; core schedules retries. Requests don't follow
//! redirects or use proxies, and unless the server allows it, hosts that
//! resolve to loopback or private addresses are refused, checked on the
//! addresses actually connected to so DNS tricks can't get around it.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use keel_core::{Attempt, Core, Delivery, Webhook, is_private_ip, sign};
use reqwest::dns::{Addrs, Name, Resolve, Resolving};

/// Deliveries sent at the same time.
const BATCH: i64 = 8;
const TIMEOUT: Duration = Duration::from_secs(10);
/// Response bytes kept for the delivery log.
const MAX_RESPONSE: usize = 4096;

/// Resolves names, dropping private addresses unless they are allowed.
struct PublicOnly;

impl Resolve for PublicOnly {
    fn resolve(&self, name: Name) -> Resolving {
        Box::pin(async move {
            let addrs: Vec<SocketAddr> = tokio::net::lookup_host((name.as_str(), 0)).await?.collect();
            let public: Vec<SocketAddr> = addrs.into_iter().filter(|a| !is_private_ip(a.ip())).collect();
            if public.is_empty() {
                return Err(format!("{} resolves only to private addresses, which webhooks may not reach", name.as_str()).into());
            }
            Ok(Box::new(public.into_iter()) as Addrs)
        })
    }
}

pub fn client(allow_private: bool) -> reqwest::Client {
    let mut builder = reqwest::Client::builder()
        .user_agent(concat!("keel-hooks/", env!("CARGO_PKG_VERSION")))
        .redirect(reqwest::redirect::Policy::none())
        .no_proxy()
        .connect_timeout(Duration::from_secs(5))
        .timeout(TIMEOUT);
    if !allow_private {
        builder = builder.dns_resolver(Arc::new(PublicOnly));
    }
    builder.build().expect("valid HTTP client configuration")
}

/// Starts the delivery worker (needs a Tokio runtime).
pub fn start(core: Core) {
    let client = client(core.config.webhook_allow_private);
    tokio::spawn(async move {
        let wakeup = core.hook_wakeup();
        loop {
            match core.claim_deliveries(BATCH).await {
                Ok(batch) if !batch.is_empty() => {
                    let sends = batch.into_iter().map(|(delivery, hook)| {
                        let (core, client) = (core.clone(), client.clone());
                        async move {
                            let attempt = deliver(&client, &delivery, &hook, core.config.webhook_allow_private).await;
                            if !attempt.succeeded() {
                                tracing::info!(hook = hook.id, delivery = delivery.id, "webhook delivery failed: {}",
                                    attempt.error.clone().unwrap_or_else(|| format!("HTTP {}", attempt.status.unwrap_or(0))));
                            }
                            if let Err(e) = core.finish_delivery(&delivery, &attempt).await {
                                tracing::warn!("recording webhook delivery: {e}");
                            }
                        }
                    });
                    futures_util::future::join_all(sends).await;
                    continue;
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("claiming webhook deliveries: {e}"),
            }
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs() as i64);
            let wait = match core.next_delivery_due().await {
                Ok(Some(due)) => (due - now).clamp(1, 60),
                _ => 60,
            };
            tokio::select! {
                _ = wakeup.notified() => {}
                _ = tokio::time::sleep(Duration::from_secs(wait as u64)) => {}
            }
        }
    });
}

/// The error and its causes, e.g. "error sending request: dns error: ...".
fn describe(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut source = e.source();
    while let Some(s) = source {
        let text = s.to_string();
        if !out.contains(&text) {
            out.push_str(": ");
            out.push_str(&text);
        }
        source = s.source();
    }
    out
}

/// POSTs one delivery.
pub async fn deliver(client: &reqwest::Client, d: &Delivery, hook: &Webhook, allow_private: bool) -> Attempt {
    let started = Instant::now();
    let failed = |error: String| Attempt {
        status: None,
        body: String::new(),
        error: Some(error),
        duration_ms: started.elapsed().as_millis() as i64,
    };
    let url = match reqwest::Url::parse(&hook.url) {
        Ok(u) => u,
        Err(e) => return failed(format!("invalid URL: {e}")),
    };
    // Literal addresses don't go through the resolver.
    let literal = url.host_str().map(|h| h.trim_start_matches('[').trim_end_matches(']'));
    if !allow_private && literal.and_then(|h| h.parse::<IpAddr>().ok()).is_some_and(is_private_ip) {
        return failed("webhooks may not reach private addresses".into());
    }

    let mut request = client
        .post(url)
        .header("Content-Type", "application/json")
        .header("X-Keel-Event", &d.event)
        .header("X-Keel-Delivery", &d.guid)
        .header("X-Keel-Hook-ID", hook.id.to_string())
        // The names GitHub uses, so receivers written for it work.
        .header("X-GitHub-Event", &d.event)
        .header("X-GitHub-Delivery", &d.guid)
        .header("X-GitHub-Hook-ID", hook.id.to_string());
    if !hook.secret.is_empty() {
        let signature = sign(&hook.secret, d.payload.as_bytes());
        request = request
            .header("X-Keel-Signature-256", &signature)
            .header("X-Hub-Signature-256", &signature);
    }
    let mut response = match request.body(d.payload.clone()).send().await {
        Ok(r) => r,
        Err(e) => return failed(describe(&e)),
    };
    let status = response.status().as_u16();
    let mut body = Vec::new();
    while body.len() < MAX_RESPONSE {
        match response.chunk().await {
            Ok(Some(chunk)) => body.extend_from_slice(&chunk),
            _ => break,
        }
    }
    body.truncate(MAX_RESPONSE);
    Attempt {
        status: Some(status),
        body: String::from_utf8_lossy(&body).into_owned(),
        error: None,
        duration_ms: started.elapsed().as_millis() as i64,
    }
}
