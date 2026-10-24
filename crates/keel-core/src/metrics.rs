//! Metrics in Prometheus' text format: counters and histograms kept in
//! memory since the server started, and totals read from the database when
//! scraped.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::sync::Mutex;
use std::time::Instant;

use crate::{Core, Result};

/// Upper bounds (seconds) of the request duration histogram's buckets.
const BUCKETS: &[f64] = &[0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0];

#[derive(Default)]
struct Histogram {
    /// Observations at most each bucket's bound (not cumulative yet).
    buckets: [u64; 11],
    count: u64,
    sum: f64,
}

/// What happened since the server started.
pub struct Metrics {
    started: Instant,
    /// (method, route, status) → requests.
    http_requests: Mutex<BTreeMap<(String, String, u16), u64>>,
    /// route → duration histogram.
    http_durations: Mutex<BTreeMap<String, Histogram>>,
    /// (service, transport) → git operations.
    git_operations: Mutex<BTreeMap<(&'static str, &'static str), u64>>,
    /// (kind, outcome) → mirror syncs and pushes.
    mirror_runs: Mutex<BTreeMap<(&'static str, &'static str), u64>>,
    /// direction → bytes of Git LFS files transferred.
    lfs_bytes: Mutex<BTreeMap<&'static str, u64>>,
}

impl Default for Metrics {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            http_requests: Mutex::default(),
            http_durations: Mutex::default(),
            git_operations: Mutex::default(),
            mirror_runs: Mutex::default(),
            lfs_bytes: Mutex::default(),
        }
    }
}

impl Metrics {
    /// A finished HTTP request; `route` is the matched route pattern (not
    /// the path, which would make too many series).
    pub fn http_request(&self, method: &str, route: &str, status: u16, seconds: f64) {
        *self.http_requests.lock().expect("metrics").entry((method.to_owned(), route.to_owned(), status)).or_default() += 1;
        let mut durations = self.http_durations.lock().expect("metrics");
        let h = durations.entry(route.to_owned()).or_default();
        if let Some(i) = BUCKETS.iter().position(|b| seconds <= *b) {
            h.buckets[i] += 1;
        }
        h.count += 1;
        h.sum += seconds;
    }

    /// A clone, fetch or push: `service` is `upload-pack` or
    /// `receive-pack`, `transport` `http` or `ssh`.
    pub fn git_operation(&self, service: &'static str, transport: &'static str) {
        *self.git_operations.lock().expect("metrics").entry((service, transport)).or_default() += 1;
    }

    /// A mirror run: `kind` is `pull` or `push`.
    pub fn mirror_run(&self, kind: &'static str, ok: bool) {
        *self.mirror_runs.lock().expect("metrics").entry((kind, if ok { "ok" } else { "failed" })).or_default() += 1;
    }

    /// Git LFS bytes received (`upload`) or sent (`download`).
    pub fn lfs_transfer(&self, direction: &'static str, bytes: u64) {
        *self.lfs_bytes.lock().expect("metrics").entry(direction).or_default() += bytes;
    }
}

/// Escapes a label value.
fn label(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"").replace('\n', "\\n")
}

impl Core {
    pub fn metrics(&self) -> &Metrics {
        &self.metrics
    }

    /// Everything, in Prometheus' text exposition format.
    pub async fn metrics_text(&self) -> Result<String> {
        let mut out = String::new();
        let m = &self.metrics;
        let mut metric = |name: &str, kind: &str, help: &str, lines: Vec<String>| {
            let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}");
            for l in lines {
                let _ = writeln!(out, "{name}{l}");
            }
        };

        metric("keel_build_info", "gauge", "The running version.", vec![format!("{{version=\"{}\"}} 1", env!("CARGO_PKG_VERSION"))]);
        metric("keel_uptime_seconds", "gauge", "Seconds since the server started.", vec![format!(" {}", m.started.elapsed().as_secs())]);

        let s = self.site_stats().await?;
        metric("keel_accounts", "gauge", "User accounts.", vec![format!(" {}", s.users)]);
        metric("keel_organizations", "gauge", "Organizations.", vec![format!(" {}", s.orgs)]);
        metric("keel_repositories", "gauge", "Repositories, by visibility.", vec![
            format!("{{visibility=\"public\"}} {}", s.repos - s.private_repos),
            format!("{{visibility=\"private\"}} {}", s.private_repos),
        ]);
        metric("keel_issues", "gauge", "Issues and pull requests.", vec![
            format!("{{kind=\"issue\"}} {}", s.issues),
            format!("{{kind=\"pull_request\"}} {}", s.pulls),
        ]);
        metric("keel_webhook_deliveries", "gauge", "Webhook deliveries waiting to be sent, and given up.", vec![
            format!("{{status=\"pending\"}} {}", s.pending_webhooks),
            format!("{{status=\"failed\"}} {}", s.failed_webhooks),
        ]);
        metric("keel_mail", "gauge", "Emails waiting to be sent, and given up.", vec![
            format!("{{status=\"pending\"}} {}", s.pending_mail),
            format!("{{status=\"failed\"}} {}", s.failed_mail),
        ]);
        let (mirrors, failing_mirrors, push_mirrors, failing_pushes): (i64, i64, i64, i64) = sqlx::query_as(
            "SELECT (SELECT COUNT(*) FROM mirrors), (SELECT COUNT(*) FROM mirrors WHERE last_error IS NOT NULL),
                    (SELECT COUNT(*) FROM push_mirrors), (SELECT COUNT(*) FROM push_mirrors WHERE last_error IS NOT NULL)",
        )
        .fetch_one(&self.db)
        .await?;
        metric("keel_mirrors", "gauge", "Mirrors, and how many failed their last run.", vec![
            format!("{{kind=\"pull\",state=\"ok\"}} {}", mirrors - failing_mirrors),
            format!("{{kind=\"pull\",state=\"failing\"}} {failing_mirrors}"),
            format!("{{kind=\"push\",state=\"ok\"}} {}", push_mirrors - failing_pushes),
            format!("{{kind=\"push\",state=\"failing\"}} {failing_pushes}"),
        ]);
        let (lfs_files, lfs_bytes): (i64, i64) =
            sqlx::query_as("SELECT COUNT(*), COALESCE(SUM(size), 0) FROM (SELECT DISTINCT oid, size FROM lfs_objects)")
                .fetch_one(&self.db)
                .await?;
        metric("keel_lfs_files", "gauge", "Stored Git LFS files.", vec![format!(" {lfs_files}")]);
        metric("keel_lfs_stored_bytes", "gauge", "Size of the stored Git LFS files.", vec![format!(" {lfs_bytes}")]);

        let requests = m.http_requests.lock().expect("metrics");
        metric(
            "keel_http_requests_total",
            "counter",
            "HTTP requests, by method, route and status.",
            requests
                .iter()
                .map(|((method, route, status), n)| format!("{{method=\"{}\",route=\"{}\",status=\"{status}\"}} {n}", label(method), label(route)))
                .collect(),
        );
        drop(requests);
        let durations = m.http_durations.lock().expect("metrics");
        let _ = writeln!(out, "# HELP keel_http_request_duration_seconds HTTP request durations, by route.");
        let _ = writeln!(out, "# TYPE keel_http_request_duration_seconds histogram");
        for (route, h) in durations.iter() {
            let route = label(route);
            let mut cumulative = 0;
            for (bound, n) in BUCKETS.iter().zip(h.buckets) {
                cumulative += n;
                let _ = writeln!(out, "keel_http_request_duration_seconds_bucket{{route=\"{route}\",le=\"{bound}\"}} {cumulative}");
            }
            let _ = writeln!(out, "keel_http_request_duration_seconds_bucket{{route=\"{route}\",le=\"+Inf\"}} {}", h.count);
            let _ = writeln!(out, "keel_http_request_duration_seconds_sum{{route=\"{route}\"}} {}", h.sum);
            let _ = writeln!(out, "keel_http_request_duration_seconds_count{{route=\"{route}\"}} {}", h.count);
        }
        drop(durations);
        let mut metric = |name: &str, help: &str, lines: Vec<String>| {
            let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} counter");
            for l in lines {
                let _ = writeln!(out, "{name}{l}");
            }
        };
        metric(
            "keel_git_operations_total",
            "Clones and fetches (upload-pack) and pushes (receive-pack), by transport.",
            m.git_operations
                .lock()
                .expect("metrics")
                .iter()
                .map(|((service, transport), n)| format!("{{service=\"{service}\",transport=\"{transport}\"}} {n}"))
                .collect(),
        );
        metric(
            "keel_mirror_runs_total",
            "Mirror syncs (pull) and pushes, by outcome.",
            m.mirror_runs
                .lock()
                .expect("metrics")
                .iter()
                .map(|((kind, outcome), n)| format!("{{kind=\"{kind}\",outcome=\"{outcome}\"}} {n}"))
                .collect(),
        );
        metric(
            "keel_lfs_transfer_bytes_total",
            "Git LFS bytes received from clients (upload) and sent to them (download).",
            m.lfs_bytes.lock().expect("metrics").iter().map(|(d, n)| format!("{{direction=\"{d}\"}} {n}")).collect(),
        );
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn exposition() {
        let (core, _dir) = Core::for_tests().await;
        core.register("ada", "ada@example.com", "password1").await.unwrap();
        let m = core.metrics();
        m.http_request("GET", "/{owner}/{repo}", 200, 0.003);
        m.http_request("GET", "/{owner}/{repo}", 200, 0.2);
        m.http_request("POST", "/login", 303, 20.0);
        m.git_operation("upload-pack", "ssh");
        m.mirror_run("pull", false);
        m.lfs_transfer("upload", 42);
        let text = core.metrics_text().await.unwrap();
        for expected in [
            "keel_accounts 1",
            "keel_http_requests_total{method=\"GET\",route=\"/{owner}/{repo}\",status=\"200\"} 2",
            "keel_http_request_duration_seconds_bucket{route=\"/{owner}/{repo}\",le=\"0.005\"} 1",
            "keel_http_request_duration_seconds_bucket{route=\"/{owner}/{repo}\",le=\"0.25\"} 2",
            "keel_http_request_duration_seconds_bucket{route=\"/login\",le=\"10\"} 0",
            "keel_http_request_duration_seconds_bucket{route=\"/login\",le=\"+Inf\"} 1",
            "keel_git_operations_total{service=\"upload-pack\",transport=\"ssh\"} 1",
            "keel_mirror_runs_total{kind=\"pull\",outcome=\"failed\"} 1",
            "keel_lfs_transfer_bytes_total{direction=\"upload\"} 42",
            "keel_repositories{visibility=\"private\"} 0",
        ] {
            assert!(text.contains(expected), "missing {expected} in\n{text}");
        }
        assert_eq!(label("a\"b\\c"), "a\\\"b\\\\c");
    }
}
