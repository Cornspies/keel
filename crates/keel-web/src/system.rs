//! The administrators' System page: what the keel process uses right now
//! (memory, threads, tasks, files, git processes), its caches, its
//! database connections and the data directory's size.

use std::time::{Duration, Instant};

use askama::Template;
use axum::extract::State;
use keel_core::{StorageUsage, User};

use crate::error::{AppResult, html};
use crate::render::human_size;
use crate::session::SignedIn;
use crate::{AppError, AppState, Page};

/// The data directory is measured at most this often (it walks every file).
const STORAGE_TTL: Duration = Duration::from_secs(60);

/// What the operating system says about this process. Linux only (from
/// `/proc`); elsewhere every field is `None`.
#[derive(Debug, Default)]
pub(crate) struct ProcessStats {
    pub memory: Option<u64>,
    pub peak_memory: Option<u64>,
    pub threads: Option<u64>,
    pub open_files: Option<u64>,
    /// CPU time used since starting.
    pub cpu: Option<Duration>,
    /// git processes keel started that are still running (clones, pushes,
    /// archives, blames…).
    pub git_processes: Option<u64>,
    /// The machine's memory: (available, total).
    pub system_memory: Option<(u64, u64)>,
    /// Load average over 1, 5 and 15 minutes.
    pub load: Option<String>,
}

impl ProcessStats {
    fn read() -> Self {
        #[cfg(target_os = "linux")]
        {
            linux::read()
        }
        #[cfg(not(target_os = "linux"))]
        {
            Self::default()
        }
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::time::Duration;

    use super::ProcessStats;

    /// A `Key:   123 kB` line of a `/proc` status-style file, in bytes (or
    /// the bare number when there's no unit).
    fn field(text: &str, key: &str) -> Option<u64> {
        let line = text.lines().find(|l| l.starts_with(key) && l[key.len()..].starts_with(':'))?;
        let mut parts = line[key.len() + 1..].split_whitespace();
        let n: u64 = parts.next()?.parse().ok()?;
        Some(if parts.next() == Some("kB") { n * 1024 } else { n })
    }

    /// Fields of a `/proc/<pid>/stat` line after the command name, which may
    /// itself contain spaces and parentheses.
    fn stat_fields(stat: &str) -> Option<(&str, Vec<&str>)> {
        let open = stat.find('(')?;
        let close = stat.rfind(')')?;
        Some((&stat[open + 1..close], stat[close + 1..].split_whitespace().collect()))
    }

    pub(super) fn read() -> ProcessStats {
        let status = std::fs::read_to_string("/proc/self/status").unwrap_or_default();
        let meminfo = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
        let me = std::process::id().to_string();
        let cpu = std::fs::read_to_string("/proc/self/stat").ok().and_then(|s| {
            let (_, f) = stat_fields(&s)?;
            // utime and stime are fields 14 and 15; `f` starts at field 3.
            let ticks: u64 = f.get(11)?.parse::<u64>().ok()? + f.get(12)?.parse::<u64>().ok()?;
            // SAFETY: sysconf has no preconditions.
            let per_sec = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
            (per_sec > 0).then(|| Duration::from_millis(ticks * 1000 / per_sec as u64))
        });
        let git_processes = std::fs::read_dir("/proc").ok().map(|dir| {
            dir.flatten()
                .filter(|e| e.file_name().to_string_lossy().bytes().all(|b| b.is_ascii_digit()))
                .filter_map(|e| std::fs::read_to_string(e.path().join("stat")).ok())
                .filter(|s| stat_fields(s).is_some_and(|(name, f)| f.get(1) == Some(&me.as_str()) && name.starts_with("git")))
                .count() as u64
        });
        ProcessStats {
            memory: field(&status, "VmRSS"),
            peak_memory: field(&status, "VmHWM"),
            threads: field(&status, "Threads"),
            open_files: std::fs::read_dir("/proc/self/fd").ok().map(|d| d.count() as u64),
            cpu,
            git_processes,
            system_memory: field(&meminfo, "MemAvailable").zip(field(&meminfo, "MemTotal")),
            load: std::fs::read_to_string("/proc/loadavg")
                .ok()
                .map(|s| s.split_whitespace().take(3).collect::<Vec<_>>().join(" · ")),
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parses_proc_files() {
            let status = "Name:\tkeel\nVmHWM:\t  20480 kB\nVmRSS:\t   10240 kB\nThreads:\t12\n";
            assert_eq!(field(status, "VmRSS"), Some(10 << 20));
            assert_eq!(field(status, "Threads"), Some(12));
            assert_eq!(field(status, "Vm"), None);
            let (name, f) = stat_fields("123 (git (odd) name) S 99 1 1 0").unwrap();
            assert_eq!((name, f[1]), ("git (odd) name", "99"));
            let me = read();
            assert!(me.memory.unwrap() > 0 && me.threads.unwrap() >= 1 && me.open_files.unwrap() > 0);
            assert!(me.cpu.is_some() && me.system_memory.is_some());
        }
    }
}

pub(crate) struct Row {
    pub label: &'static str,
    pub value: String,
}

fn row(label: &'static str, value: impl Into<String>) -> Row {
    Row { label, value: value.into() }
}

/// One kind of stored data, with its share of the total.
pub(crate) struct StorageRow {
    pub label: &'static str,
    pub size: String,
    pub bytes: u64,
    pub percent: String,
}

pub(crate) struct CacheRow {
    pub label: &'static str,
    pub entries: u64,
    /// Memory used of the most it may use, for caches limited by size.
    pub size: String,
}

#[derive(Template)]
#[template(path = "admin/system.html")]
struct SystemPage {
    page: Page,
    nav: &'static str,
    memory: String,
    threads: String,
    tasks: usize,
    storage_total: String,
    uptime: String,
    process: Vec<Row>,
    storage: Vec<StorageRow>,
    storage_bytes: u64,
    /// "23.4 GiB free of 23.5 GiB on /tmp (tmpfs)".
    disk: String,
    in_memory: bool,
    caches: Vec<CacheRow>,
}

fn size(bytes: u64) -> String {
    human_size(bytes as usize)
}

fn dash() -> String {
    "–".into()
}

/// "3 days, 4 hours", the two largest units.
fn duration(d: Duration) -> String {
    let s = d.as_secs();
    let parts = [(s / 86_400, "day"), (s / 3600 % 24, "hour"), (s / 60 % 60, "minute"), (s % 60, "second")];
    let shown: Vec<String> = parts
        .iter()
        .skip_while(|(n, _)| *n == 0)
        .take(2)
        .filter(|(n, _)| *n > 0)
        .map(|(n, unit)| format!("{n} {unit}{}", if *n == 1 { "" } else { "s" }))
        .collect();
    if shown.is_empty() { "0 seconds".into() } else { shown.join(", ") }
}

/// The data directory's usage, measured at most every [`STORAGE_TTL`].
async fn storage(state: &AppState) -> AppResult<StorageUsage> {
    let mut cached = state.storage.lock().await;
    if let Some((usage, at)) = cached.as_ref()
        && at.elapsed() < STORAGE_TTL
    {
        return Ok(usage.clone());
    }
    let usage = state.core.storage_usage().await?;
    *cached = Some((usage.clone(), Instant::now()));
    Ok(usage)
}

/// `GET /admin/system`.
pub async fn system(State(state): State<AppState>, SignedIn(user): SignedIn) -> AppResult {
    require_admin(&user)?;
    let p = tokio::task::spawn_blocking(ProcessStats::read).await.map_err(|e| AppError::Internal(e.into()))?;
    let metrics = tokio::runtime::Handle::current().metrics();
    let workers = metrics.num_workers();
    let tasks = metrics.num_alive_tasks();
    let uptime = state.started.elapsed();
    let busy: Duration = (0..workers).map(|w| metrics.worker_total_busy_duration(w)).sum();
    let busy_share = busy.as_secs_f64() / (uptime.as_secs_f64() * workers as f64).max(f64::EPSILON) * 100.0;
    let usage = storage(&state).await?;
    let total = usage.total();
    let pool = &state.core.db;

    let process = vec![
        row("Peak memory", p.peak_memory.map_or_else(dash, size)),
        row("Machine memory", p.system_memory.map_or_else(dash, |(free, all)| format!("{} free of {}", size(free), size(all)))),
        row("Async tasks", format!("{tasks} on {workers} workers, {} queued, {busy_share:.1}% busy", metrics.global_queue_depth())),
        row("git processes", p.git_processes.map_or_else(dash, |n| n.to_string())),
        row("Open files", p.open_files.map_or_else(dash, |n| n.to_string())),
        row("Database connections", format!("{} of 8 ({} idle)", pool.size(), pool.num_idle())),
        row("CPU time", p.cpu.map_or_else(dash, duration)),
        row("Load average", p.load.clone().unwrap_or_else(dash)),
        row("Process ID", std::process::id().to_string()),
    ];
    let kinds = [
        ("Repositories", usage.repositories),
        ("Wikis", usage.wikis),
        ("Git LFS files", usage.lfs),
        ("Release files", usage.releases),
        ("Database", usage.database),
        ("Search index", usage.search),
        ("Avatars", usage.avatars),
        ("Other", usage.other),
    ];
    let storage = kinds
        .into_iter()
        .map(|(label, bytes)| StorageRow {
            label,
            size: size(bytes),
            bytes,
            percent: if total > 0 { format!("{:.1}%", bytes as f64 / total as f64 * 100.0) } else { dash() },
        })
        .collect();
    let mut disk = usage.disk.map_or_else(String::new, |(free, all)| format!("{} free of {}", size(free), size(all)));
    if let Some((point, kind)) = &usage.mount {
        disk.push_str(&format!(" on {point} ({kind})"));
    }
    let cache = |label, entries: u64, used: Option<(u64, u64)>| CacheRow {
        label,
        entries,
        size: used.map_or_else(String::new, |(used, max)| format!("{} of {}", size(used), size(max))),
    };
    let caches = vec![
        cache("Highlighted files", state.highlighted.entry_count(), Some((state.highlighted.weighted_size(), 128 << 20))),
        cache("Rendered Markdown", state.markdown.entry_count(), Some((state.markdown.weighted_size(), 32 << 20))),
        cache("Blame", state.blames.entry_count(), Some((state.blames.weighted_size(), 64 << 20))),
        cache("Last commits", state.last_commits.entry_count(), None),
        cache("Languages", state.languages.entry_count(), None),
        cache("Merge checks", state.mergeability.entry_count(), None),
    ];

    html(SystemPage {
        page: Page::new(&state, Some(user), "System · Site administration"),
        nav: "system",
        memory: p.memory.map_or_else(dash, size),
        threads: p.threads.map_or_else(dash, |n| n.to_string()),
        tasks,
        storage_total: size(total),
        uptime: duration(uptime),
        process,
        storage,
        storage_bytes: total.max(1),
        disk,
        in_memory: usage.in_memory(),
        caches,
    })
}

fn require_admin(user: &User) -> AppResult<()> {
    if user.is_admin { Ok(()) } else { Err(AppError::NotFound) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(duration(Duration::from_secs(0)), "0 seconds");
        assert_eq!(duration(Duration::from_secs(61)), "1 minute, 1 second");
        assert_eq!(duration(Duration::from_secs(3 * 86_400 + 4 * 3600 + 5)), "3 days, 4 hours");
        assert_eq!(duration(Duration::from_secs(7200)), "2 hours");
    }
}
