//! The background process: shared state, scheduled scans, file watching and the HTTP server.

use crate::api;
use crate::events::{self, Events};
use crate::idempotency;
use crate::logger::Logger;
use crate::supervisor::{self, Policy, Runtime};
use crate::token;
use crate::update::{self, Release, UpdateCheck};
use crate::webhooks::{self, Backoff};
use notify::{PollWatcher, RecursiveMode, Watcher};
use serde_json::{json, Value};
use simpletickets_core::audit::AuditLog;
use simpletickets_core::config::{Config, CONFIG_FILE};
use simpletickets_core::diff;
use simpletickets_core::lease::{self, Lease};
use simpletickets_core::migration::{self, Journal, MigrationError};
use simpletickets_core::plugin::{self, Source};
use simpletickets_core::rules::{self, RunOptions};
use simpletickets_core::state::{
    audit_log_path, ensure_state_dir, is_within, state_dir, token_path, STATE_DIR,
};
use simpletickets_core::store::Store;
use simpletickets_core::VERSION;
use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// The API only ever listens on the loopback interface.
pub const BIND_ADDRESS: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);
pub const DEFAULT_PORT: u16 = 7878;
const WORKERS: usize = 8;
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
    /// Heartbeat age after which another daemon may take over this folder's lease.
    pub lease_ttl: Duration,
    /// How many events the stream keeps for clients that poll late.
    pub event_backlog: usize,
    /// Retry policy for webhook deliveries.
    pub webhook_backoff: Backoff,
    /// The per-user plugins folder searched after the tickets folder's own.
    pub user_plugins_dir: Option<PathBuf>,
    /// Restart policy for service plugins.
    pub plugin_policy: Policy,
    /// How long the answer to a write sent with an `Idempotency-Key` is remembered.
    pub idempotency_ttl: Duration,
    pub logger: Arc<Logger>,
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
            lease_ttl: lease::DEFAULT_TTL,
            event_backlog: events::DEFAULT_BACKLOG,
            webhook_backoff: Backoff::default(),
            user_plugins_dir: plugin::user_plugins_dir(),
            plugin_policy: Policy::default(),
            idempotency_ttl: idempotency::DEFAULT_TTL,
            logger: Arc::new(Logger::discard()),
        }
    }
}

/// Reasons for a rescan that follow a write made through the API; any other
/// rescan finds changes made outside the daemon.
const API_REASONS: [&str; 6] = [
    "create",
    "update",
    "reassign-id",
    "forget",
    "config",
    "migration",
];

/// Per ticket, the status before the first and after the last rule change of a run,
/// in order of first change; tickets that ended where they began are left out.
fn net_status_rewrites(changes: &[rules::Change]) -> Vec<(String, String, String)> {
    let mut out: Vec<(String, String, String)> = Vec::new();
    for c in changes {
        match out.iter_mut().find(|(id, _, _)| *id == c.id) {
            Some(entry) => entry.2 = c.to.clone(),
            None => out.push((c.id.clone(), c.from.clone(), c.to.clone())),
        }
    }
    out.retain(|(_, from, to)| from != to);
    out
}

pub struct Inner {
    pub config: Config,
    pub store: Store,
    /// Offline roots already announced through events.
    offline_reported: BTreeSet<PathBuf>,
}

/// State shared between the API workers and the scheduler.
pub struct Shared {
    pub dir: PathBuf,
    pub logger: Arc<Logger>,
    pub events: Events,
    pub audit: AuditLog,
    pub stop: AtomicBool,
    /// Where plugin manifests are searched, in priority order.
    pub plugin_dirs: Vec<(Source, PathBuf)>,
    /// Runtime state of the service plugins the supervisor manages.
    pub plugins: Mutex<BTreeMap<String, Runtime>>,
    /// Answers to keyed writes, for retries.
    pub idempotency: Mutex<idempotency::Store>,
    token: String,
    inner: Mutex<Inner>,
    scheduler: Mutex<Option<Sender<Message>>>,
    update: Mutex<Option<Release>>,
    /// The lease that entitles this daemon to run the rules; `None` after it was lost.
    lease: Mutex<Option<Lease>>,
}

/// What a rescan found, for callers that schedule follow-ups.
#[derive(Debug, Clone, Copy, Default)]
pub struct ScanOutcome {
    /// Some rule changes were skipped and may succeed later.
    pub retry: bool,
    pub config_changed: bool,
    /// A root went offline or came back.
    pub roots_changed: bool,
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

    /// The enabled plugin names, or `None` while a scan holds the state.
    pub fn enabled_plugins(&self) -> Option<Vec<String>> {
        self.inner
            .try_lock()
            .ok()
            .map(|inner| inner.config.plugins.clone())
    }

    /// True while this daemon holds the lease and so may run the rules.
    pub fn rules_enabled(&self) -> bool {
        self.lease
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    fn release_lease(&self) {
        *self.lease.lock().unwrap_or_else(|e| e.into_inner()) = None;
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
        self.rescan_locked_as(inner, reason, "api")
    }

    /// Like [`Shared::rescan_locked`]; ticket events that report an API write carry `actor`.
    pub fn rescan_locked_as(
        &self,
        inner: &mut Inner,
        reason: &str,
        actor: &str,
    ) -> (Value, ScanOutcome) {
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
        let store = Store::scan_after(&inner.config, Some(&inner.store));
        self.announce_roots(inner, &store, &mut outcome);
        let source = if API_REASONS.contains(&reason) {
            "api"
        } else {
            "scan"
        };
        for change in diff::diff(&inner.store, &store) {
            let mut data = change.data(source);
            if source == "api" {
                if let Value::Object(map) = &mut data {
                    map.insert("actor".into(), json!(actor));
                }
            }
            self.events.publish(change.kind(), data);
        }
        let (store, report) = if self.rules_enabled() {
            rules::reconcile(store, Some(&self.audit), RunOptions::default())
        } else {
            (store, rules::RunReport::default())
        };
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
        for (id, from, to) in net_status_rewrites(&report.changes) {
            if let Some(entry) = store.get(&id) {
                for change in diff::status_rewrite(store.config(), entry, &from, &to) {
                    self.events.publish(change.kind(), change.data("rule"));
                }
            }
        }
        for warning in &report.raised {
            self.events.publish("warning.raised", json!(warning));
        }
        if let Some(message) = &report.state_error {
            self.events
                .publish("state.error", json!({"message": message}));
        }
        let problems =
            store.problems().len() + store.conflicts().len() + store.offline_roots().len();
        let summary = json!({
            "reason": reason,
            "tickets": store.tickets().count(),
            "problems": problems,
            "rules": report,
            "rules_enabled": self.rules_enabled(),
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
    /// Publishes `root.offline` and `root.online` events for roots whose state changed.
    fn announce_roots(&self, inner: &mut Inner, store: &Store, outcome: &mut ScanOutcome) {
        let now: BTreeSet<PathBuf> = store.offline_roots().keys().cloned().collect();
        for root in now.difference(&inner.offline_reported) {
            let frozen = store
                .tickets()
                .filter(|e| store.is_frozen(&e.ticket.id) && is_within(&e.path, root))
                .count();
            self.events.publish(
                "root.offline",
                json!({
                    "root": root.display().to_string(),
                    "message": store.offline_roots()[root],
                    "frozen": frozen,
                }),
            );
        }
        for root in inner.offline_reported.difference(&now) {
            self.events
                .publish("root.online", json!({"root": root.display().to_string()}));
        }
        outcome.roots_changed = now != inner.offline_reported;
        inner.offline_reported = now;
    }

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
    plugins_stop: Arc<AtomicBool>,
    supervisor: Option<JoinHandle<()>>,
}

impl Running {
    pub fn token_file(&self) -> &Path {
        &self.token_file
    }

    /// Stops the scheduler and workers, waits for them, releases the lease and flushes the log.
    pub fn stop(self) {
        let logger = self.shared.logger.clone();
        logger.info("stopping: stopping plugins");
        self.plugins_stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.supervisor {
            let _ = t.join();
        }
        logger.info("stopping: draining workers");
        self.shared.stop.store(true, Ordering::Relaxed);
        let _ = self.tx.send(Message::Stop);
        for t in self.threads {
            let _ = t.join();
        }
        self.shared.release_lease();
        logger.info("stopped: lease released");
        logger.flush();
    }
}

pub fn start(options: Options) -> Result<Running, String> {
    let dir = options
        .dir
        .canonicalize()
        .map_err(|e| format!("{}: {e}", options.dir.display()))?;
    let logger = options.logger.clone();
    ensure_state_dir(&dir).map_err(|e| format!("cannot create state folder: {e}"))?;
    let lease = Lease::acquire(&dir, options.lease_ttl).map_err(|e| {
        logger.error(&format!("cannot take the folder lease: {e}"));
        e.to_string()
    })?;
    logger.info(&format!(
        "starting version={VERSION} dir={} lease=acquired",
        dir.display()
    ));
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
        logger: logger.clone(),
        events: Events::new(logger.clone(), options.event_backlog),
        audit: AuditLog::new(audit_log_path(&dir)),
        stop: AtomicBool::new(false),
        plugin_dirs: plugin::search_dirs(&dir, options.user_plugins_dir.clone()),
        plugins: Mutex::new(BTreeMap::new()),
        idempotency: Mutex::new(idempotency::Store::load(
            state_dir(&dir).join("idempotency.jsonl"),
            options.idempotency_ttl,
        )),
        token,
        scheduler: Mutex::new(None),
        update: Mutex::new(None),
        lease: Mutex::new(Some(lease)),
        inner: Mutex::new(Inner {
            store: Store::scan(&config),
            config,
            offline_reported: BTreeSet::new(),
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
    {
        let (shared, backoff) = (shared.clone(), options.webhook_backoff);
        threads.push(thread::spawn(move || webhooks::run(&shared, backoff)));
    }
    {
        let (shared, ttl) = (shared.clone(), options.lease_ttl);
        threads.push(thread::spawn(move || keep_lease(&shared, ttl)));
    }
    if let Some(check) = options.update_check.clone() {
        let shared = shared.clone();
        threads.push(thread::spawn(move || run_update_checks(&shared, &check)));
    }
    logger.info(&format!("listening addr={addr}"));
    let plugins_stop = Arc::new(AtomicBool::new(false));
    let supervisor = {
        let (shared, stop) = (shared.clone(), plugins_stop.clone());
        let url = format!("http://127.0.0.1:{}", addr.port());
        let (token_file, policy) = (token_file.clone(), options.plugin_policy);
        thread::spawn(move || supervisor::run(&shared, url, token_file, policy, &stop))
    };
    Ok(Running {
        addr,
        shared,
        token_file,
        tx,
        threads,
        plugins_stop,
        supervisor: Some(supervisor),
    })
}

/// Refreshes the lease heartbeat until the daemon stops. A lease taken over by
/// another daemon switches the rules off; it is taken back once that daemon is gone.
fn keep_lease(shared: &Shared, ttl: Duration) {
    let interval = (ttl / 3).max(Duration::from_millis(50));
    while !shared.stop.load(Ordering::Relaxed) {
        let deadline = Instant::now() + interval;
        while Instant::now() < deadline && !shared.stop.load(Ordering::Relaxed) {
            thread::sleep(Duration::from_millis(25).min(interval));
        }
        if shared.stop.load(Ordering::Relaxed) {
            return;
        }
        let mut slot = shared.lease.lock().unwrap_or_else(|e| e.into_inner());
        match slot.as_ref().map(Lease::renew) {
            Some(Ok(())) => {}
            Some(Err(e)) => {
                *slot = None;
                shared
                    .events
                    .publish("lease.lost", json!({"message": e.to_string()}));
            }
            None => {
                if let Ok(lease) = Lease::acquire(&shared.dir, ttl) {
                    *slot = Some(lease);
                    shared.events.publish("lease.acquired", json!({}));
                }
            }
        }
    }
}

/// Checks at startup and then every interval until the daemon stops.
fn run_update_checks(shared: &Shared, check: &UpdateCheck) {
    while !shared.stop.load(Ordering::Relaxed) {
        let found = update::check_once(check.fetcher.as_ref(), VERSION);
        shared
            .logger
            .debug(&format!("update check newer_release={}", found.is_some()));
        if let Some(release) = found {
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
        if (outcome.config_changed || outcome.roots_changed) && watcher.is_some() {
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
}
