//! Outgoing webhooks: configured events are POSTed as JSON, signed with HMAC-SHA256.
//!
//! Delivery runs on its own threads, fed from the event stream, so a slow or failing
//! receiver never delays the API, the scans or the rules.

use crate::api::{API_VERSION, API_VERSION_HEADER};
use crate::daemon::Shared;
use crate::events::Gap;
use serde_json::Value;
use simpletickets_core::config::Webhook;
use simpletickets_core::hash::hmac_sha256_hex;
use simpletickets_core::VERSION;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Events queued per webhook; further events for a webhook that is this far behind
/// are dropped and logged.
const QUEUE: usize = 1000;

pub const SIGNATURE_HEADER: &str = "X-SimpleTickets-Signature";
pub const TIMESTAMP_HEADER: &str = "X-SimpleTickets-Timestamp";
pub const EVENT_HEADER: &str = "X-SimpleTickets-Event";
pub const DELIVERY_HEADER: &str = "X-SimpleTickets-Delivery";

/// How a failed delivery is retried: `attempts` tries in all, waiting `base`, then
/// twice as long each time up to `cap`.
#[derive(Debug, Clone, Copy)]
pub struct Backoff {
    pub attempts: u32,
    pub base: Duration,
    pub cap: Duration,
    /// Limit for one request, connecting included.
    pub timeout: Duration,
}

impl Default for Backoff {
    fn default() -> Backoff {
        Backoff {
            attempts: 6,
            base: Duration::from_secs(1),
            cap: Duration::from_secs(30),
            timeout: Duration::from_secs(10),
        }
    }
}

impl Backoff {
    /// The wait after the `failures`-th failed attempt (1-based).
    pub fn delay(&self, failures: u32) -> Duration {
        let factor = 1u32
            .checked_shl(failures.saturating_sub(1))
            .unwrap_or(u32::MAX);
        self.base.saturating_mul(factor).min(self.cap)
    }
}

/// True when `kind` matches a glob where `*` stands for any run of characters and `?`
/// for exactly one.
pub fn glob_matches(glob: &str, kind: &str) -> bool {
    let (g, k): (Vec<char>, Vec<char>) = (glob.chars().collect(), kind.chars().collect());
    let (mut gi, mut ki) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while ki < k.len() {
        if gi < g.len() && (g[gi] == '?' || g[gi] == k[ki]) {
            gi += 1;
            ki += 1;
        } else if gi < g.len() && g[gi] == '*' {
            star = Some((gi, ki));
            gi += 1;
        } else if let Some((sg, sk)) = star {
            gi = sg + 1;
            ki = sk + 1;
            star = Some((sg, sk + 1));
        } else {
            return false;
        }
    }
    g[gi..].iter().all(|c| *c == '*')
}

/// The signature header value for a delivery: `sha256=` and the hex HMAC-SHA256, under
/// `secret`, of the timestamp, a dot and the exact request body.
pub fn sign(secret: &[u8], timestamp: &str, body: &[u8]) -> String {
    let mut message = Vec::with_capacity(timestamp.len() + 1 + body.len());
    message.extend_from_slice(timestamp.as_bytes());
    message.push(b'.');
    message.extend_from_slice(body);
    format!("sha256={}", hmac_sha256_hex(secret, &message))
}

/// The URL without credentials, safe to log.
fn display_url(url: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => {
            let (authority, tail) = rest.split_at(rest.find(['/', '?', '#']).unwrap_or(rest.len()));
            let host = authority.rsplit('@').next().unwrap_or(authority);
            format!("{scheme}://{host}{tail}")
        }
        None => url.to_string(),
    }
}

struct Worker {
    queue: SyncSender<Value>,
    thread: JoinHandle<()>,
}

fn worker_key(hook: &Webhook) -> String {
    format!("{}\n{:?}\n{:?}", hook.url, hook.secret_env, hook.events)
}

/// Feeds every event of this daemon run to the webhooks configured at that moment,
/// until the daemon stops.
pub fn run(shared: &Arc<Shared>, backoff: Backoff) {
    let mut workers: HashMap<String, Worker> = HashMap::new();
    let mut cursor = 0;
    let stream = shared.events.stream().to_string();
    while !shared.stop.load(Ordering::Relaxed) {
        let page = shared
            .events
            .since(cursor, Some(&stream), Duration::from_secs(1), &shared.stop);
        if page.gap == Some(Gap::BacklogExceeded) {
            shared.logger.warn(&format!(
                "webhooks fell behind the event backlog; events before seq={} are lost",
                page.oldest
            ));
        }
        cursor = page.next;
        if page.events.is_empty() {
            continue;
        }
        let hooks = shared.lock().config.webhooks.clone();
        let keys: Vec<String> = hooks.iter().map(worker_key).collect();
        workers.retain(|key, _| keys.contains(key));
        for (hook, key) in hooks.iter().zip(&keys) {
            let wanted: Vec<&Value> = page
                .events
                .iter()
                .filter(|e| {
                    let kind = e["type"].as_str().unwrap_or("");
                    hook.events.iter().any(|g| glob_matches(g, kind))
                })
                .collect();
            if wanted.is_empty() {
                continue;
            }
            let worker = workers.entry(key.clone()).or_insert_with(|| {
                let (tx, rx) = mpsc::sync_channel(QUEUE);
                let (shared, hook) = (shared.clone(), hook.clone());
                let stream = stream.clone();
                Worker {
                    queue: tx,
                    thread: thread::spawn(move || {
                        deliver_all(&shared, &hook, &stream, &rx, backoff)
                    }),
                }
            });
            for event in wanted {
                if worker.queue.try_send(event.clone()).is_err() {
                    shared.logger.warn(&format!(
                        "webhook {} is too far behind; dropped seq={}",
                        display_url(&hook.url),
                        event["seq"]
                    ));
                }
            }
        }
    }
    let threads: Vec<JoinHandle<()>> = workers.into_values().map(|w| w.thread).collect();
    for t in threads {
        let _ = t.join();
    }
}

fn deliver_all(
    shared: &Shared,
    hook: &Webhook,
    stream: &str,
    rx: &mpsc::Receiver<Value>,
    backoff: Backoff,
) {
    loop {
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(event) => deliver(shared, hook, stream, &event, backoff),
            Err(RecvTimeoutError::Timeout) => {
                if shared.stop.load(Ordering::Relaxed) {
                    return;
                }
            }
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

/// Sends one event, retrying with backoff. Failures are logged and the event is dropped
/// after the last attempt; nothing else waits for it.
fn deliver(shared: &Shared, hook: &Webhook, stream: &str, event: &Value, backoff: Backoff) {
    let target = display_url(&hook.url);
    let seq = event["seq"].as_u64().unwrap_or(0);
    let kind = event["type"].as_str().unwrap_or("");
    let secret = match &hook.secret_env {
        None => None,
        Some(name) => match std::env::var(name) {
            Ok(value) if !value.is_empty() => Some(value),
            _ => {
                shared.logger.error(&format!(
                    "webhook {target}: environment variable {name} is not set; dropped seq={seq}"
                ));
                return;
            }
        },
    };
    let body = event.to_string();
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(backoff.timeout))
        .http_status_as_error(false)
        .max_redirects(0)
        .build()
        .into();
    let delivery_id = format!("{stream}-{seq}");
    for attempt in 1..=backoff.attempts.max(1) {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
            .to_string();
        let mut request = agent
            .post(&hook.url)
            .header("Content-Type", "application/json")
            .header("User-Agent", format!("simpletickets/{VERSION}"))
            .header(API_VERSION_HEADER, API_VERSION.to_string())
            .header(EVENT_HEADER, kind)
            .header(DELIVERY_HEADER, &delivery_id)
            .header(TIMESTAMP_HEADER, &timestamp);
        if let Some(secret) = &secret {
            request = request.header(
                SIGNATURE_HEADER,
                sign(secret.as_bytes(), &timestamp, body.as_bytes()),
            );
        }
        let failure = match request.send(body.as_str()) {
            Ok(response) if response.status().is_success() => {
                shared.logger.debug(&format!(
                    "webhook {target}: delivered seq={seq} type={kind} attempt={attempt}"
                ));
                return;
            }
            Ok(response) => format!("status {}", response.status().as_u16()),
            Err(e) => e.to_string(),
        };
        if attempt >= backoff.attempts.max(1) {
            shared.logger.error(&format!(
                "webhook {target}: gave up on seq={seq} type={kind} after {attempt} attempts: {failure}"
            ));
            return;
        }
        let wait = backoff.delay(attempt);
        shared.logger.warn(&format!(
            "webhook {target}: seq={seq} type={kind} attempt={attempt} failed ({failure}); retrying in {}ms",
            wait.as_millis()
        ));
        let until = Instant::now() + wait;
        while Instant::now() < until {
            if shared.stop.load(Ordering::Relaxed) {
                return;
            }
            thread::sleep(Duration::from_millis(25).min(wait));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn globs() {
        assert!(glob_matches("*", "ticket.created"));
        assert!(glob_matches("ticket.*", "ticket.created"));
        assert!(glob_matches("ticket.*", "ticket.status_changed"));
        assert!(!glob_matches("ticket.*", "scan"));
        assert!(glob_matches("ticket.created", "ticket.created"));
        assert!(!glob_matches("ticket.created", "ticket.created.x"));
        assert!(glob_matches("*.created", "ticket.created"));
        assert!(glob_matches("t?cket.*", "ticket.moved"));
        assert!(glob_matches("*status*", "ticket.status_changed"));
        assert!(glob_matches("a*b*c", "aXXbYYc"));
        assert!(!glob_matches("a*b*c", "aXXbYY"));
        assert!(glob_matches("", ""));
        assert!(!glob_matches("", "x"));
    }

    #[test]
    fn backoff_doubles_up_to_the_cap() {
        let b = Backoff {
            attempts: 10,
            base: Duration::from_secs(1),
            cap: Duration::from_secs(10),
            timeout: Duration::from_secs(1),
        };
        let delays: Vec<u64> = (1..=6).map(|n| b.delay(n).as_secs()).collect();
        assert_eq!(delays, [1, 2, 4, 8, 10, 10]);
        assert_eq!(b.delay(100), Duration::from_secs(10));
    }

    #[test]
    fn signature_covers_timestamp_and_body() {
        let a = sign(b"secret", "1700000000", b"{}");
        assert!(a.starts_with("sha256="));
        assert_eq!(a.len(), "sha256=".len() + 64);
        assert_eq!(a, sign(b"secret", "1700000000", b"{}"));
        assert_ne!(a, sign(b"secret", "1700000001", b"{}"));
        assert_ne!(a, sign(b"secret", "1700000000", b"{ }"));
        assert_ne!(a, sign(b"other", "1700000000", b"{}"));
    }

    #[test]
    fn urls_are_logged_without_credentials() {
        assert_eq!(
            display_url("https://user:pw@example.org:8443/hook?x=1"),
            "https://example.org:8443/hook?x=1"
        );
        assert_eq!(display_url("http://127.0.0.1:9/h"), "http://127.0.0.1:9/h");
    }
}
