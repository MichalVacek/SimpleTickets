//! The background process: shared state, scheduled scans, file watching and the HTTP server.

use crate::api;
use crate::token;
use crate::update::{self, Release, UpdateCheck};
use notify::{PollWatcher, RecursiveMode, Watcher};
use serde_json::{json, Value};
use simpletickets_core::audit::AuditLog;
use simpletickets_core::config::{Config, CONFIG_FILE};
use simpletickets_core::migration::{self, Journal, MigrationError};
use simpletickets_core::rules::{self, RunOptions};
use simpletickets_core::state::{audit_log_path, ensure_state_dir, token_path, STATE_DIR};
use simpletickets_core::store::Store;
use simpletickets_core::time::now_timestamp;
use simpletickets_core::VERSION;
use std::collections::VecDeque;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// The API only ever listens on the loopback interface.
pub const BIND_ADDRESS: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
pub const DEFAULT_PORT: u16 = 7878;
const WORKERS: usize = 8;
const EVENT_BACKLOG: usize = 1000;
const WATCH_QUIET: Duration = Duration::from_millis(300);
const RETRY_AFTER: Duration = Duration::from_secs(3);

#[derive(Debug, Clone)]
pub struct Options {
    pub dir: PathBuf,
    pub port: u16,
    /// Listen on every interface instead of loopback. Only for containers whose published
    /// port is itself restricted to the host's loopback.
    pub bind_all: bool,
    pub scan_interval: Duration,
    /// Token file; defaults to the state folder.
    pub token_file: Option<PathBuf>,
    pub watch: bool,
    /// Detect changes by polling at this interval instead of native file events, for
    /// folders where those are not delivered (bind mounts, network shares).
    pub poll_interval: Option<Duration>,
    /// Periodic check for a newer release; `None` disables it.
    pub update_check: Option<UpdateCheck>,
}

impl Options {
    pub fn new(dir: PathBuf) -> Options {
        Options {
            dir,
            port: DEFAULT_PORT,
            bind_all: false,
            scan_interval: Duration::from_secs(60),
            token_file: None,
            watch: true,
            poll_interval: None,
            update_check: None,
        }
    }
}

/// Sequence-numbered log of recent events, with blocking reads.
pub struct Events {
    log: Mutex<(u64, VecDeque<Value>)>,
    changed: Condvar,
}

impl Events {
    fn new() -> Events {
        Events {
            log: Mutex::new((1, VecDeque::new())),
            changed: Condvar::new(),
        }
    }

    pub fn publish(&self, kind: &str, data: Value) {
        let mut guard = self.log.lock().unwrap_or_else(|e| e.into_inner());
        let seq = guard.0;
        guard.0 += 1;
        guard
            .1
            .push_back(json!({"seq": seq, "time": now_timestamp(), "type": kind, "data": data}));
        if guard.1.len() > EVENT_BACKLOG {
            guard.1.pop_front();
        }
        self.changed.notify_all();
    }

    /// Events with a sequence number above `since`, waiting up to `wait` for the first one.
    /// Returns the events and the sequence number to pass as `since` next time.
    pub fn since(&self, since: u64, wait: Duration, stop: &AtomicBool) -> (Vec<Value>, u64) {
        let deadline = Instant::now() + wait;
        let mut guard = self.log.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            let found: Vec<Value> = guard
                .1
                .iter()
                .filter(|e| e["seq"].as_u64().unwrap_or(0) > since)
                .cloned()
                .collect();
            let next = found
                .last()
                .and_then(|e| e["seq"].as_u64())
                .unwrap_or_else(|| since.max(guard.0 - 1));
            let left = deadline.saturating_duration_since(Instant::now());
            if !found.is_empty() || left.is_zero() || stop.load(Ordering::Relaxed) {
                return (found, next);
            }
            let slice = left.min(Duration::from_millis(250));
            guard = self
                .changed
                .wait_timeout(guard, slice)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

pub struct Inner {
    pub config: Config,
    pub store: Store,
}

/// State shared between the API workers and the scheduler.
pub struct Shared {
    pub dir: PathBuf,
    pub events: Events,
    pub audit: AuditLog,
    pub stop: AtomicBool,
    token: String,
    inner: Mutex<Inner>,
    scheduler: Mutex<Option<Sender<Message>>>,
    update: Mutex<Option<Release>>,
}

/// What a rescan found, for callers that schedule follow-ups.
#[derive(Debug, Clone, Copy, Default)]
pub struct ScanOutcome {
    /// Some rule changes were skipped and may succeed later.
    pub retry: bool,
    pub config_changed: bool,
}

impl Shared {
    pub fn verify_token(&self, given: &str) -> bool {
        token::matches(&self.token, given)
    }

    /// The newer release found by the update check, if any.
    pub fn available_update(&self) -> Option<Release> {
        self.update
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Reloads the config, scans all folders and runs the rules.
    pub fn rescan(&self, reason: &str) -> (Value, ScanOutcome) {
        let mut inner = self.lock();
        self.rescan_locked(&mut inner, reason)
    }

    pub fn rescan_locked(&self, inner: &mut Inner, reason: &str) -> (Value, ScanOutcome) {
        let mut outcome = ScanOutcome::default();
        match Config::load_from_dir(&self.dir) {
            Ok(config) if config != inner.config => {
                let plan = migration::plan(&inner.store, &config);
                inner.config = config;
                outcome.config_changed = true;
                self.events
                    .publish("config.changed", json!({"reason": reason}));
                if !plan.is_empty() {
                    self.events.publish(
                        "migration.suggested",
                        json!({"actions": plan.actions.len(), "conflicts": plan.conflicts.len()}),
                    );
                }
            }
            Ok(_) => {}
            Err(e) => self
                .events
                .publish("config.invalid", json!({"message": e.to_string()})),
        }
        let store = Store::scan(&inner.config);
        let (store, report) = rules::reconcile(store, Some(&self.audit), RunOptions::default());
        outcome.retry = report.skipped.iter().any(|s| s.retry);
        if outcome.retry {
            if let Some(tx) = self
                .scheduler
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_ref()
            {
                let _ = tx.send(Message::Retry);
            }
        }
        for change in &report.changes {
            self.events.publish("rule.applied", json!(change));
        }
        let problems = store.problems().len() + store.duplicates().len();
        let summary = json!({
            "reason": reason,
            "tickets": store.tickets().count(),
            "problems": problems,
            "rules": report,
        });
        self.events.publish(
            "scan",
            json!({"reason": reason, "tickets": store.tickets().count(), "problems": problems}),
        );
        inner.store = store;
        (summary, outcome)
    }
}

impl Shared {
    /// Plans, journals and applies a migration to the config in `yaml`, then rescans.
    pub fn migrate(&self, inner: &mut Inner, yaml: &str) -> Result<Journal, MigrationError> {
        let journal = migration::begin(&self.dir, &inner.store, yaml)?;
        self.events.publish(
            "migration.started",
            json!({"steps": journal.steps.len(), "conflicts": journal.conflicts.len()}),
        );
        let journal = migration::run(&self.dir, journal, migration::RunOptions::default())?;
        self.after_migration(inner, &journal);
        Ok(journal)
    }

    /// Resumes an unfinished migration; `None` when there is nothing to resume.
    pub fn resume_migration(&self, inner: &mut Inner) -> Result<Option<Journal>, MigrationError> {
        let journal = migration::resume(&self.dir, migration::RunOptions::default())?;
        if let Some(j) = &journal {
            self.after_migration(inner, j);
        }
        Ok(journal)
    }

    fn after_migration(&self, inner: &mut Inner, journal: &Journal) {
        self.events.publish("migration.finished", journal.summary());
        if journal.is_complete() {
            if let Ok(config) = Config::load_from_dir(&self.dir) {
                inner.store = Store::scan(&config);
                inner.config = config;
            }
        }
        self.rescan_locked(inner, "migration");
    }
}

enum Message {
    Changed,
    /// Some change was skipped; scan again soon.
    Retry,
    Stop,
}

/// A running daemon: the HTTP workers and the scheduler thread.
pub struct Running {
    pub addr: SocketAddr,
    pub shared: Arc<Shared>,
    token_file: PathBuf,
    tx: Sender<Message>,
    threads: Vec<JoinHandle<()>>,
}

impl Running {
    pub fn token_file(&self) -> &Path {
        &self.token_file
    }

    pub fn stop(self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        let _ = self.tx.send(Message::Stop);
        for t in self.threads {
            let _ = t.join();
        }
    }

    /// Blocks until the daemon is stopped.
    pub fn wait(self) {
        for t in self.threads {
            let _ = t.join();
        }
    }
}

pub fn start(options: Options) -> Result<Running, String> {
    let dir = options
        .dir
        .canonicalize()
        .map_err(|e| format!("{}: {e}", options.dir.display()))?;
    ensure_state_dir(&dir).map_err(|e| format!("cannot create state folder: {e}"))?;
    let resumed = migration::resume(&dir, migration::RunOptions::default());
    let config = Config::load_from_dir(&dir).map_err(|e| e.to_string())?;
    let token_file = options
        .token_file
        .clone()
        .unwrap_or_else(|| token_path(&dir));
    let token = token::load_or_create(&token_file)
        .map_err(|e| format!("token file {}: {e}", token_file.display()))?;

    let shared = Arc::new(Shared {
        dir: dir.clone(),
        events: Events::new(),
        audit: AuditLog::new(audit_log_path(&dir)),
        stop: AtomicBool::new(false),
        token,
        scheduler: Mutex::new(None),
        update: Mutex::new(None),
        inner: Mutex::new(Inner {
            store: Store::scan(&config),
            config,
        }),
    });
    match resumed {
        Ok(Some(journal)) => shared
            .events
            .publish("migration.finished", journal.summary()),
        Ok(None) => {}
        Err(e) => shared
            .events
            .publish("migration.error", json!({"message": e.to_string()})),
    }
    shared.rescan("startup");

    let bind = if options.bind_all {
        IpAddr::V4(Ipv4Addr::UNSPECIFIED)
    } else {
        BIND_ADDRESS
    };
    let server = tiny_http::Server::http(SocketAddr::new(bind, options.port))
        .map_err(|e| format!("cannot listen on {bind}:{}: {e}", options.port))?;
    let addr = match server.server_addr() {
        tiny_http::ListenAddr::IP(addr) => addr,
        #[allow(unreachable_patterns)]
        _ => return Err("server did not bind a TCP address".into()),
    };
    let server = Arc::new(server);

    let mut threads = Vec::new();
    for _ in 0..WORKERS {
        let (server, shared) = (server.clone(), shared.clone());
        threads.push(thread::spawn(move || api::serve(&server, &shared)));
    }

    let (tx, rx) = mpsc::channel();
    *shared.scheduler.lock().unwrap_or_else(|e| e.into_inner()) = Some(tx.clone());
    {
        let (shared, tx) = (shared.clone(), tx.clone());
        let (interval, watch, poll) = (options.scan_interval, options.watch, options.poll_interval);
        threads.push(thread::spawn(move || {
            schedule(&shared, &rx, &tx, interval, watch, poll)
        }));
    }
    if let Some(check) = options.update_check.clone() {
        let shared = shared.clone();
        threads.push(thread::spawn(move || run_update_checks(&shared, &check)));
    }
    Ok(Running {
        addr,
        shared,
        token_file,
        tx,
        threads,
    })
}

/// Checks at startup and then every interval until the daemon stops.
fn run_update_checks(shared: &Shared, check: &UpdateCheck) {
    while !shared.stop.load(Ordering::Relaxed) {
        if let Some(release) = update::check_once(check.fetcher.as_ref(), VERSION) {
            let mut slot = shared.update.lock().unwrap_or_else(|e| e.into_inner());
            if slot.as_ref() != Some(&release) {
                shared.events.publish(
                    "update.available",
                    json!({"current": VERSION, "latest": release.tag, "url": release.url}),
                );
                *slot = Some(release);
            }
        }
        let deadline = Instant::now() + check.interval;
        while Instant::now() < deadline && !shared.stop.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(100).min(check.interval));
        }
    }
}

fn schedule(
    shared: &Shared,
    rx: &mpsc::Receiver<Message>,
    tx: &Sender<Message>,
    interval: Duration,
    watch: bool,
    poll: Option<Duration>,
) {
    let mut watcher = watch.then(|| make_watcher(&shared.lock().config, &shared.dir, tx, poll));
    let mut deadline = Instant::now() + interval;
    loop {
        let timeout = deadline.saturating_duration_since(Instant::now());
        let reason = match rx.recv_timeout(timeout) {
            Ok(Message::Stop) | Err(RecvTimeoutError::Disconnected) => return,
            Ok(Message::Retry) => {
                deadline = deadline.min(Instant::now() + RETRY_AFTER);
                continue;
            }
            Ok(Message::Changed) => {
                // Wait for the burst of events to settle.
                loop {
                    match rx.recv_timeout(WATCH_QUIET) {
                        Ok(Message::Changed | Message::Retry) => {}
                        Ok(Message::Stop) | Err(RecvTimeoutError::Disconnected) => return,
                        Err(RecvTimeoutError::Timeout) => break,
                    }
                }
                "watch"
            }
            Err(RecvTimeoutError::Timeout) => "periodic",
        };
        if shared.stop.load(Ordering::Relaxed) {
            return;
        }
        let (_, outcome) = shared.rescan(reason);
        deadline = Instant::now() + if outcome.retry { RETRY_AFTER } else { interval };
        if outcome.config_changed && watcher.is_some() {
            watcher = Some(make_watcher(&shared.lock().config, &shared.dir, tx, poll));
        }
    }
}

/// Watches the roots, auto-scan folders and the config folder, with native file events
/// or, when `poll` is set, by polling. A failing watch is not fatal: the periodic scan
/// still finds changes.
fn make_watcher(
    config: &Config,
    dir: &Path,
    tx: &Sender<Message>,
    poll: Option<Duration>,
) -> Option<Box<dyn Watcher + Send>> {
    let sender = tx.clone();
    let handler = move |event: notify::Result<notify::Event>| {
        if let Ok(event) = event {
            if event.paths.iter().any(|p| is_relevant(p)) {
                let _ = sender.send(Message::Changed);
            }
        }
    };
    let mut watcher: Box<dyn Watcher + Send> = match poll {
        Some(interval) => Box::new(
            PollWatcher::new(
                handler,
                notify::Config::default().with_poll_interval(interval),
            )
            .ok()?,
        ),
        None => Box::new(notify::recommended_watcher(handler).ok()?),
    };
    let folders = config
        .roots
        .iter()
        .map(|r| r.path.as_path())
        .chain(config.auto_scan.iter().map(PathBuf::as_path));
    for folder in folders {
        let _ = watcher.watch(folder, RecursiveMode::Recursive);
    }
    let _ = watcher.watch(dir, RecursiveMode::NonRecursive);
    Some(watcher)
}

fn is_relevant(path: &Path) -> bool {
    let hidden_state = path.components().any(|c| c.as_os_str() == STATE_DIR);
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
    !hidden_state && (name == CONFIG_FILE || (name.ends_with(".md") && !name.starts_with('.')))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relevant_paths() {
        assert!(is_relevant(Path::new("/t/a/b.md")));
        assert!(is_relevant(Path::new("/t/simpletickets.yml")));
        assert!(!is_relevant(Path::new("/t/.b.md.1234.tmp")));
        assert!(!is_relevant(Path::new("/t/.simpletickets/audit.jsonl")));
        assert!(!is_relevant(Path::new("/t/.simpletickets/templates/x.md")));
        assert!(!is_relevant(Path::new("/t/notes.txt")));
    }

    #[test]
    fn events_are_sequenced_and_blocking() {
        let events = Events::new();
        let stop = AtomicBool::new(false);
        let (none, next) = events.since(0, Duration::ZERO, &stop);
        assert!(none.is_empty());
        assert_eq!(next, 0);
        events.publish("a", json!({}));
        events.publish("b", json!({}));
        let (all, next) = events.since(0, Duration::ZERO, &stop);
        assert_eq!(all.len(), 2);
        assert_eq!(next, 2);
        let (rest, _) = events.since(1, Duration::ZERO, &stop);
        assert_eq!(rest[0]["type"], "b");
        let (empty, same) = events.since(2, Duration::from_millis(10), &stop);
        assert!(empty.is_empty());
        assert_eq!(same, 2);
    }
}
