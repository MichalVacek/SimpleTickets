//! Remembers the answers to writes sent with an `Idempotency-Key`, so a retried request
//! is answered again instead of being applied twice.
//!
//! Entries are scoped by actor, expire after a time to live, and are kept in
//! `.simpletickets/idempotency.jsonl` so a retry after a daemon restart is still caught.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use simpletickets_core::hash::content_hash;
use std::collections::{HashMap, VecDeque};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// How long an answer is remembered when nothing else is configured.
pub const DEFAULT_TTL: Duration = Duration::from_secs(24 * 3600);
/// Most answers kept; older ones are forgotten first.
pub const MAX_ENTRIES: usize = 10_000;
/// Longest key accepted.
pub const MAX_KEY_LEN: usize = 128;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    scope: String,
    key: String,
    fingerprint: String,
    status: u16,
    body: Value,
    /// Unix seconds when the answer was recorded.
    at: u64,
}

/// What the store knows about a key.
#[derive(Debug, Clone, PartialEq)]
pub enum Lookup {
    /// Never seen, or expired: apply the request.
    Miss,
    /// The same request was answered before: send this answer again.
    Replay(u16, Value),
    /// The key was used for a different request.
    Mismatch,
}

pub struct Store {
    path: Option<PathBuf>,
    ttl: Duration,
    max: usize,
    entries: HashMap<String, Entry>,
    order: VecDeque<String>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn slot(scope: &str, key: &str) -> String {
    format!("{scope}\n{key}")
}

/// True for a key of 1 to [`MAX_KEY_LEN`] visible ASCII characters.
pub fn is_valid_key(key: &str) -> bool {
    !key.is_empty() && key.len() <= MAX_KEY_LEN && key.bytes().all(|b| (0x21..=0x7e).contains(&b))
}

/// A fingerprint of a request: its method, path and JSON body, so that whitespace and key
/// order in the body do not matter.
pub fn fingerprint(method: &str, path: &str, body: &[u8]) -> String {
    let canonical = serde_json::from_slice::<Value>(body).map_or_else(
        |_| String::from_utf8_lossy(body).into_owned(),
        |v| v.to_string(),
    );
    content_hash(format!("{method} {path}\n{canonical}").as_bytes())
}

impl Store {
    /// A store that keeps its answers in memory only.
    pub fn in_memory(ttl: Duration) -> Store {
        Store {
            path: None,
            ttl,
            max: MAX_ENTRIES,
            entries: HashMap::new(),
            order: VecDeque::new(),
        }
    }

    /// Loads the unexpired answers from `path` and rewrites the file without the rest.
    pub fn load(path: PathBuf, ttl: Duration) -> Store {
        let mut store = Store::in_memory(ttl);
        let text = fs::read_to_string(&path).unwrap_or_default();
        let total = text.lines().count();
        for entry in text
            .lines()
            .filter_map(|l| serde_json::from_str::<Entry>(l).ok())
        {
            store.insert(entry);
        }
        if store.entries.len() < total {
            let mut out = String::new();
            for slot in &store.order {
                if let Ok(line) = serde_json::to_string(&store.entries[slot]) {
                    out.push_str(&line);
                    out.push('\n');
                }
            }
            let _ = fs::write(&path, out);
        }
        store.path = Some(path);
        store
    }

    fn expired(&self, entry: &Entry) -> bool {
        now().saturating_sub(entry.at) >= self.ttl.as_secs().max(1)
    }

    fn insert(&mut self, entry: Entry) {
        if self.expired(&entry) {
            return;
        }
        let slot = slot(&entry.scope, &entry.key);
        if self.entries.insert(slot.clone(), entry).is_some() {
            self.order.retain(|s| *s != slot);
        }
        self.order.push_back(slot);
        while self.order.len() > self.max {
            if let Some(old) = self.order.pop_front() {
                self.entries.remove(&old);
            }
        }
    }

    pub fn lookup(&self, scope: &str, key: &str, fingerprint: &str) -> Lookup {
        match self.entries.get(&slot(scope, key)) {
            Some(e) if !self.expired(e) => {
                if e.fingerprint == fingerprint {
                    Lookup::Replay(e.status, e.body.clone())
                } else {
                    Lookup::Mismatch
                }
            }
            _ => Lookup::Miss,
        }
    }

    /// Remembers the answer to a request. A file that cannot be written only costs the
    /// protection across restarts.
    pub fn record(&mut self, scope: &str, key: &str, fingerprint: &str, status: u16, body: &Value) {
        let entry = Entry {
            scope: scope.into(),
            key: key.into(),
            fingerprint: fingerprint.into(),
            status,
            body: body.clone(),
            at: now(),
        };
        if let Some(path) = &self.path {
            if let Ok(mut line) = serde_json::to_string(&entry) {
                line.push('\n');
                if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(path) {
                    let _ = file.write_all(line.as_bytes());
                }
            }
        }
        self.insert(entry);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn replays_the_same_request_and_flags_a_different_one() {
        let mut store = Store::in_memory(DEFAULT_TTL);
        let fp = fingerprint("POST", "/v1/tickets", br#"{"title":"A"}"#);
        assert_eq!(store.lookup("bot", "k1", &fp), Lookup::Miss);
        store.record("bot", "k1", &fp, 201, &json!({"id": "ST-1"}));
        assert_eq!(
            store.lookup("bot", "k1", &fp),
            Lookup::Replay(201, json!({"id": "ST-1"}))
        );
        let other = fingerprint("POST", "/v1/tickets", br#"{"title":"B"}"#);
        assert_eq!(store.lookup("bot", "k1", &other), Lookup::Mismatch);
        assert_eq!(store.lookup("other-actor", "k1", &fp), Lookup::Miss);
    }

    #[test]
    fn fingerprints_ignore_whitespace_and_key_order() {
        let a = fingerprint("POST", "/p", br#"{"a": 1, "b": 2}"#);
        let b = fingerprint("POST", "/p", b"{\"b\":2,\n\"a\":1}");
        assert_eq!(a, b);
        assert_ne!(a, fingerprint("PATCH", "/p", br#"{"a":1,"b":2}"#));
        assert_ne!(a, fingerprint("POST", "/q", br#"{"a":1,"b":2}"#));
    }

    #[test]
    fn expired_answers_are_forgotten() {
        let mut store = Store::in_memory(Duration::from_secs(1));
        store.record("a", "k", "fp", 200, &json!({}));
        store.entries.get_mut(&slot("a", "k")).unwrap().at -= 5;
        assert_eq!(store.lookup("a", "k", "fp"), Lookup::Miss);
    }

    #[test]
    fn answers_survive_a_reload_and_old_ones_are_dropped_from_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("idempotency.jsonl");
        let old =
            r#"{"scope":"a","key":"old","fingerprint":"fp","status":201,"body":{},"at":1000}"#;
        let fresh = format!(
            r#"{{"scope":"a","key":"new","fingerprint":"fp","status":201,"body":{{"id":"ST-9"}},"at":{}}}"#,
            now()
        );
        fs::write(&path, format!("{old}\n{fresh}\nnot json\n")).unwrap();
        let mut store = Store::load(path.clone(), DEFAULT_TTL);
        assert_eq!(store.lookup("a", "old", "fp"), Lookup::Miss);
        assert_eq!(
            store.lookup("a", "new", "fp"),
            Lookup::Replay(201, json!({"id": "ST-9"}))
        );
        assert_eq!(fs::read_to_string(&path).unwrap().lines().count(), 1);
        store.record("a", "later", "fp2", 200, &json!({"x": 1}));
        let again = Store::load(path, DEFAULT_TTL);
        assert_eq!(
            again.lookup("a", "later", "fp2"),
            Lookup::Replay(200, json!({"x": 1}))
        );
        assert_ne!(again.lookup("a", "new", "fp"), Lookup::Miss);
    }

    #[test]
    fn the_oldest_answers_are_evicted_first() {
        let mut store = Store::in_memory(DEFAULT_TTL);
        store.max = 2;
        for key in ["1", "2", "3"] {
            store.record("a", key, "fp", 200, &json!({}));
        }
        assert_eq!(store.lookup("a", "1", "fp"), Lookup::Miss);
        assert_ne!(store.lookup("a", "3", "fp"), Lookup::Miss);
    }

    #[test]
    fn validates_keys() {
        assert!(is_valid_key("follow-up:ST-1"));
        assert!(!is_valid_key(""));
        assert!(!is_valid_key("has space"));
        assert!(!is_valid_key(&"x".repeat(MAX_KEY_LEN + 1)));
        assert!(is_valid_key(&"x".repeat(MAX_KEY_LEN)));
    }
}
