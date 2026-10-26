//! Starts, watches and stops the `service` plugins listed in the config.
//!
//! One thread per plugin owns its child process: it captures the output into the daemon
//! log, restarts the process after a crash with a growing delay, gives up when it crashes
//! too often within a window, and stops the process when the plugin is disabled or the
//! daemon shuts down.

use crate::api::API_VERSION;
use crate::daemon::Shared;
use crate::plugins::{bin_dir, preflight};
use serde_json::{json, Value};
use simpletickets_core::plugin::{self, Found, Kind, Manifest, Placeholders, TokenDelivery};
use simpletickets_core::time::now_timestamp;
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Longest log line kept from a plugin's output; the rest of a longer line is dropped.
const MAX_LINE: u64 = 8192;
const TICK: Duration = Duration::from_millis(100);

/// Environment variables passed on to plugins besides the ones the manifest asks for.
const BASE_ENV: [&str; 20] = [
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "LC_ALL",
    "TMPDIR",
    "TMP",
    "TEMP",
    "TZ",
    "SHELL",
    "SYSTEMROOT",
    "SYSTEMDRIVE",
    "WINDIR",
    "COMSPEC",
    "PATHEXT",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
];

/// How crashes are handled. After the `n`-th crash within `window` the restart waits
/// `backoff_initial` doubled `n - 1` times, at most `backoff_cap`; `max_crashes` crashes
/// within `window` mark the plugin failed.
#[derive(Debug, Clone, Copy)]
pub struct Policy {
    pub backoff_initial: Duration,
    pub backoff_cap: Duration,
    pub max_crashes: u32,
    pub window: Duration,
    /// How long a plugin gets to exit after the termination request before it is killed.
    pub stop_grace: Duration,
}

impl Default for Policy {
    fn default() -> Policy {
        Policy {
            backoff_initial: Duration::from_secs(1),
            backoff_cap: Duration::from_secs(30),
            max_crashes: 5,
            window: Duration::from_secs(60),
            stop_grace: Duration::from_secs(5),
        }
    }
}

impl Policy {
    /// The wait before the restart that follows the `crashes`-th crash (1-based).
    pub fn delay(&self, crashes: u32) -> Duration {
        let factor = 1u32
            .checked_shl(crashes.saturating_sub(1))
            .unwrap_or(u32::MAX);
        self.backoff_initial
            .saturating_mul(factor)
            .min(self.backoff_cap)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Starting,
    Running,
    /// Crashed; waiting to be restarted.
    Backoff,
    /// Gave up, or could not be started at all.
    Failed,
    Stopped,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Starting => "starting",
            State::Running => "running",
            State::Backoff => "backoff",
            State::Failed => "failed",
            State::Stopped => "stopped",
        }
    }
}

/// What the supervisor knows about one plugin it manages.
#[derive(Debug, Clone)]
pub struct Runtime {
    pub state: State,
    pub pid: Option<u32>,
    pub started_at: Option<String>,
    /// Restarts after crashes since the plugin was enabled.
    pub restarts: u32,
    /// Crashes since the plugin was enabled.
    pub crashes: u32,
    pub last_exit: Option<String>,
    pub error: Option<String>,
    /// Result of the last health check; `None` without a check or before the first one.
    pub healthy: Option<bool>,
}

impl Runtime {
    /// The values reported for a plugin the supervisor has not started.
    pub fn idle() -> Runtime {
        Runtime::new(State::Stopped)
    }

    fn new(state: State) -> Runtime {
        Runtime {
            state,
            pid: None,
            started_at: None,
            restarts: 0,
            crashes: 0,
            last_exit: None,
            error: None,
            healthy: None,
        }
    }

    pub fn to_json(&self) -> Value {
        json!({
            "state": self.state.as_str(),
            "pid": self.pid,
            "started_at": self.started_at,
            "restarts": self.restarts,
            "crashes": self.crashes,
            "last_exit": self.last_exit,
            "error": self.error,
            "healthy": self.healthy,
        })
    }
}

/// Why a plugin thread was asked to stop.
const RUN: u8 = 0;
const DISABLED: u8 = 1;
const SHUTDOWN: u8 = 2;

struct Handle {
    stop: Arc<AtomicU8>,
    thread: JoinHandle<()>,
}

struct Context {
    api_url: String,
    token_file: PathBuf,
    policy: Policy,
}

fn update(shared: &Shared, name: &str, change: impl FnOnce(&mut Runtime)) {
    let mut map = shared.plugins.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(rt) = map.get_mut(name) {
        change(rt);
    }
}

/// Reconciles the running plugins with the config until `stop` is set, then stops them all.
pub fn run(
    shared: &Arc<Shared>,
    api_url: String,
    token_file: PathBuf,
    policy: Policy,
    stop: &AtomicBool,
) {
    let ctx = Arc::new(Context {
        api_url,
        token_file,
        policy,
    });
    let mut handles: HashMap<String, Handle> = HashMap::new();
    let mut wanted: Option<Vec<String>> = None;
    while !stop.load(Ordering::Relaxed) {
        if let Some(now) = shared.enabled_plugins() {
            if wanted.as_ref() != Some(&now) {
                reconcile(shared, &ctx, &mut handles, &now);
                wanted = Some(now);
            }
        }
        thread::sleep(TICK);
    }
    for handle in handles.values() {
        handle.stop.store(SHUTDOWN, Ordering::Relaxed);
    }
    for (_, handle) in handles {
        let _ = handle.thread.join();
    }
}

fn reconcile(
    shared: &Arc<Shared>,
    ctx: &Arc<Context>,
    handles: &mut HashMap<String, Handle>,
    wanted: &[String],
) {
    let leaving: Vec<String> = handles
        .keys()
        .filter(|n| !wanted.contains(n))
        .cloned()
        .collect();
    for name in leaving {
        if let Some(handle) = handles.remove(&name) {
            handle.stop.store(DISABLED, Ordering::Relaxed);
            let _ = handle.thread.join();
        }
    }
    shared
        .plugins
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|n, rt| wanted.contains(n) && rt.state != State::Stopped);
    let found = plugin::discover(&shared.plugin_dirs);
    for name in wanted {
        if handles.contains_key(name) {
            continue;
        }
        let Some(entry) = found.iter().find(|f| &f.name == name) else {
            fail(shared, name, "plugin is enabled but not installed");
            continue;
        };
        let manifest = match &entry.manifest {
            Ok(m) => m.clone(),
            Err(e) => {
                fail(shared, name, &format!("invalid manifest: {e}"));
                continue;
            }
        };
        if manifest.kind != Kind::Service {
            continue;
        }
        if let Err(reason) = preflight(&manifest, &entry.dir) {
            fail(shared, name, &reason);
            continue;
        }
        shared
            .plugins
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(name.clone(), Runtime::new(State::Starting));
        let stop = Arc::new(AtomicU8::new(RUN));
        let thread = {
            let (shared, ctx, stop) = (shared.clone(), ctx.clone(), stop.clone());
            let entry = entry.clone();
            thread::spawn(move || supervise(&shared, &ctx, &entry, manifest, &stop))
        };
        handles.insert(name.clone(), Handle { stop, thread });
    }
}

/// Records that a plugin cannot be started and says so through the event stream.
fn fail(shared: &Shared, name: &str, reason: &str) {
    let mut rt = Runtime::new(State::Failed);
    rt.error = Some(reason.to_string());
    shared
        .plugins
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(name.to_string(), rt);
    shared.events.publish(
        "plugin.failed",
        json!({"name": name, "reason": reason, "crashes": 0}),
    );
}

/// The child's exit as text: the exit code, or the signal on Unix.
fn describe_exit(status: &ExitStatus) -> String {
    if let Some(code) = status.code() {
        return format!("exit code {code}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return format!("signal {signal}");
        }
    }
    "unknown exit".to_string()
}

fn spawn(
    manifest: &Manifest,
    entry: &Found,
    ctx: &Context,
    shared: &Shared,
) -> Result<Child, String> {
    let program = manifest.resolve_command(&entry.dir, bin_dir().as_deref());
    let values = Placeholders {
        tickets_dir: shared.dir.clone(),
        api_url: ctx.api_url.clone(),
        token_file: ctx.token_file.clone(),
        plugin_dir: entry.dir.clone(),
    };
    let workdir = manifest
        .working_dir
        .as_ref()
        .map_or_else(|| entry.dir.clone(), |d| entry.dir.join(d));
    let mut command = Command::new(program);
    command
        .args(manifest.expanded_args(&values))
        .current_dir(workdir)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for name in BASE_ENV
        .iter()
        .copied()
        .chain(manifest.env.iter().map(String::as_str))
    {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    command
        .env("SIMPLETICKETS_URL", &ctx.api_url)
        .env("SIMPLETICKETS_TOKEN_FILE", &ctx.token_file)
        .env("SIMPLETICKETS_DIR", &shared.dir)
        .env("SIMPLETICKETS_PLUGIN_NAME", &manifest.name)
        .env("SIMPLETICKETS_PLUGIN_DIR", &entry.dir)
        .env("SIMPLETICKETS_ACTOR", format!("plugin:{}", manifest.name))
        .env("SIMPLETICKETS_API_VERSION", API_VERSION.to_string());
    if manifest.token == TokenDelivery::Env {
        let token = std::fs::read_to_string(&ctx.token_file)
            .map_err(|e| format!("cannot read the token file: {e}"))?;
        command.env("SIMPLETICKETS_TOKEN", token.trim());
    }
    command.spawn().map_err(|e| format!("cannot start: {e}"))
}

/// Copies a stream into the daemon log, one prefixed line at a time.
fn pump(
    shared: Arc<Shared>,
    name: String,
    stream: &'static str,
    source: impl Read + Send + 'static,
) {
    let mut reader = BufReader::new(source);
    loop {
        let mut line = Vec::new();
        match (&mut reader).take(MAX_LINE).read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        if line.len() as u64 == MAX_LINE && line.last() != Some(&b'\n') {
            let mut rest = Vec::new();
            let _ = reader.read_until(b'\n', &mut rest);
        }
        let text = String::from_utf8_lossy(&line);
        let text = text.trim_end_matches(['\r', '\n']);
        if !text.is_empty() {
            shared
                .logger
                .info(&format!("[plugin:{name}] {stream}{text}"));
        }
    }
}

fn health_checks(
    shared: &Shared,
    name: &str,
    manifest: &Manifest,
    values: &Placeholders,
    done: &AtomicBool,
) {
    let Some(health) = &manifest.health else {
        return;
    };
    let url = values.expand(&health.url);
    let interval = Duration::from_secs(health.interval_seconds);
    let mut last: Option<bool> = None;
    while !done.load(Ordering::Relaxed) {
        let ok = ureq::get(&url)
            .config()
            .timeout_global(Some(Duration::from_secs(health.timeout_seconds)))
            .http_status_as_error(false)
            .max_redirects(0)
            .build()
            .call()
            .is_ok_and(|r| r.status().is_success());
        if done.load(Ordering::Relaxed) {
            return;
        }
        update(shared, name, |rt| rt.healthy = Some(ok));
        if last != Some(ok) && !(last.is_none() && ok) {
            shared.logger.warn(&format!(
                "[plugin:{name}] health check {}",
                if ok { "passes again" } else { "fails" }
            ));
        }
        last = Some(ok);
        let deadline = Instant::now() + interval;
        while Instant::now() < deadline && !done.load(Ordering::Relaxed) {
            thread::sleep(TICK.min(interval));
        }
    }
}

/// Gives the output threads a moment to log what is left. A grandchild that inherited the
/// pipes keeps them open, so the threads are left to end by themselves after that.
fn drain(pumps: Vec<JoinHandle<()>>) {
    let deadline = Instant::now() + Duration::from_millis(500);
    while Instant::now() < deadline && !pumps.iter().all(JoinHandle::is_finished) {
        thread::sleep(Duration::from_millis(10));
    }
}

/// Asks the child to exit and kills it when it does not within the grace period.
fn terminate(child: &mut Child, grace: Duration) {
    #[cfg(unix)]
    {
        let _ = Command::new("kill")
            .args(["-TERM", &child.id().to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let deadline = Instant::now() + grace;
        while Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
    #[cfg(not(unix))]
    let _ = grace;
    let _ = child.kill();
    let _ = child.wait();
}

fn supervise(
    shared: &Arc<Shared>,
    ctx: &Context,
    entry: &Found,
    manifest: Manifest,
    stop: &AtomicU8,
) {
    let name = manifest.name.clone();
    let policy = ctx.policy;
    let mut crashes: VecDeque<Instant> = VecDeque::new();
    let mut restarts = 0u32;
    loop {
        update(shared, &name, |rt| {
            rt.state = State::Starting;
            rt.healthy = None;
        });
        let outcome = match spawn(&manifest, entry, ctx, shared) {
            Err(e) => Err(e),
            Ok(mut child) => {
                let pid = child.id();
                update(shared, &name, |rt| {
                    rt.state = State::Running;
                    rt.pid = Some(pid);
                    rt.started_at = Some(now_timestamp());
                    rt.error = None;
                });
                shared.events.publish(
                    "plugin.started",
                    json!({"name": name, "pid": pid, "restarts": restarts}),
                );
                let mut pumps = Vec::new();
                if let Some(out) = child.stdout.take() {
                    let (s, n) = (shared.clone(), name.clone());
                    pumps.push(thread::spawn(move || pump(s, n, "", out)));
                }
                if let Some(err) = child.stderr.take() {
                    let (s, n) = (shared.clone(), name.clone());
                    pumps.push(thread::spawn(move || pump(s, n, "(stderr) ", err)));
                }
                let done = Arc::new(AtomicBool::new(false));
                let health = {
                    let (s, n, d) = (shared.clone(), name.clone(), done.clone());
                    let m = manifest.clone();
                    let values = Placeholders {
                        tickets_dir: shared.dir.clone(),
                        api_url: ctx.api_url.clone(),
                        token_file: ctx.token_file.clone(),
                        plugin_dir: entry.dir.clone(),
                    };
                    thread::spawn(move || health_checks(&s, &n, &m, &values, &d))
                };
                let exit = loop {
                    if let Ok(Some(status)) = child.try_wait() {
                        break Some(status);
                    }
                    if stop.load(Ordering::Relaxed) != RUN {
                        terminate(&mut child, policy.stop_grace);
                        break None;
                    }
                    thread::sleep(Duration::from_millis(25));
                };
                done.store(true, Ordering::Relaxed);
                drain(pumps);
                let _ = health.join();
                match exit {
                    Some(status) => Ok(describe_exit(&status)),
                    None => {
                        stopped(shared, &name, stop);
                        return;
                    }
                }
            }
        };
        let (exit, error) = match outcome {
            Ok(exit) => (Some(exit), None),
            Err(e) => (None, Some(e)),
        };
        let now = Instant::now();
        crashes.push_back(now);
        while crashes
            .front()
            .is_some_and(|t| now.duration_since(*t) > policy.window)
        {
            crashes.pop_front();
        }
        let recent = crashes.len() as u32;
        let gave_up = recent >= policy.max_crashes.max(1);
        let delay = policy.delay(recent);
        let reason = error.clone().or_else(|| exit.clone()).unwrap_or_default();
        update(shared, &name, |rt| {
            rt.crashes += 1;
            rt.pid = None;
            rt.last_exit = exit.clone();
            rt.error = error.clone();
            rt.healthy = None;
            rt.state = if gave_up {
                State::Failed
            } else {
                State::Backoff
            };
        });
        shared.events.publish(
            "plugin.crashed",
            json!({
                "name": name,
                "exit": exit,
                "error": error,
                "crashes": recent,
                "will_restart": !gave_up,
                "restart_in_ms": if gave_up { Value::Null } else { json!(delay.as_millis()) },
            }),
        );
        if gave_up {
            shared.events.publish(
                "plugin.failed",
                json!({
                    "name": name,
                    "reason": format!("{recent} crashes within {}s; last: {reason}", policy.window.as_secs()),
                    "crashes": recent,
                }),
            );
            return;
        }
        let deadline = Instant::now() + delay;
        while Instant::now() < deadline {
            if stop.load(Ordering::Relaxed) != RUN {
                stopped(shared, &name, stop);
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        restarts += 1;
        update(shared, &name, |rt| rt.restarts = restarts);
    }
}

fn stopped(shared: &Shared, name: &str, stop: &AtomicU8) {
    let reason = if stop.load(Ordering::Relaxed) == SHUTDOWN {
        "shutdown"
    } else {
        "disabled"
    };
    update(shared, name, |rt| {
        rt.state = State::Stopped;
        rt.pid = None;
        rt.healthy = None;
    });
    shared
        .events
        .publish("plugin.stopped", json!({"name": name, "reason": reason}));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_doubles_up_to_the_cap() {
        let p = Policy {
            backoff_initial: Duration::from_secs(1),
            backoff_cap: Duration::from_secs(10),
            ..Policy::default()
        };
        let secs: Vec<u64> = (1..=6).map(|n| p.delay(n).as_secs()).collect();
        assert_eq!(secs, [1, 2, 4, 8, 10, 10]);
        assert_eq!(p.delay(200).as_secs(), 10);
    }
}
