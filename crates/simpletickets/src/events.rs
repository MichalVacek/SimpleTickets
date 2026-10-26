//! The event stream: a bounded, sequence-numbered log with blocking reads and gap detection.

use crate::logger::{self, Level, Logger};
use serde_json::{json, Value};
use simpletickets_core::time::now_timestamp;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Events kept when no other size is configured.
pub const DEFAULT_BACKLOG: usize = 1000;

struct Log {
    /// Sequence number the next event gets.
    next_seq: u64,
    queue: VecDeque<Value>,
}

/// Sequence-numbered log of recent events, with blocking reads.
pub struct Events {
    log: Mutex<Log>,
    changed: Condvar,
    logger: Arc<Logger>,
    backlog: usize,
    /// Identifies this daemon run; sequence numbers restart with every run.
    stream: String,
}

/// Why a reader cannot continue from the sequence number it passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Gap {
    /// Events after `since` were dropped from the backlog.
    BacklogExceeded,
    /// `since` is newer than anything published: the client holds a number from before a
    /// daemon restart.
    AheadOfStream,
    /// The client's `stream` is not this daemon run's.
    StreamChanged,
}

impl Gap {
    pub fn key(self) -> &'static str {
        match self {
            Gap::BacklogExceeded => "backlog_exceeded",
            Gap::AheadOfStream => "ahead_of_stream",
            Gap::StreamChanged => "stream_changed",
        }
    }
}

/// One read of the stream.
#[derive(Debug, Clone)]
pub struct Page {
    pub events: Vec<Value>,
    /// Sequence number to pass as `since` next time.
    pub next: u64,
    /// Sequence number of the oldest retained event; `latest + 1` when none is retained.
    pub oldest: u64,
    /// Sequence number of the newest event ever published, 0 when there is none.
    pub latest: u64,
    pub gap: Option<Gap>,
    pub stream: String,
}

/// The log level and message line for an event. Rule applications use the level the
/// rule engine chose from the ticket's tags (`tag_logging`).
fn log_entry(kind: &str, data: &Value) -> (Level, String) {
    let text = |key: &str| {
        data.get(key)
            .map(|v| v.as_str().map_or_else(|| v.to_string(), str::to_string))
    };
    let keys: &[&str] = match kind {
        "rule.applied" => &["rule", "id", "from", "to", "reason"],
        "scan" => &["reason", "tickets", "problems"],
        "root.offline" => &["root", "message", "frozen"],
        "root.online" => &["root"],
        "lease.lost" | "state.error" | "config.invalid" | "migration.error" => &["message"],
        "migration.started" | "migration.suggested" => &["steps", "conflicts", "actions"],
        "migration.finished" => &["state", "steps", "conflicts"],
        "update.available" => &["current", "latest", "url"],
        "warning.raised" => &["id", "rule", "message"],
        "ticket.created" | "ticket.deleted" | "ticket.moved" | "ticket.updated" => {
            &["id", "title", "source"]
        }
        "ticket.status_changed" => &["id", "from", "to", "source"],
        "tombstone.forgotten" => &["id"],
        "plugin.started" => &["name", "pid", "restarts"],
        "plugin.stopped" => &["name", "reason"],
        "plugin.crashed" => &["name", "exit", "error", "crashes", "will_restart"],
        "plugin.failed" => &["name", "reason", "crashes"],
        _ => &[],
    };
    let pairs: Vec<(&str, String)> = keys
        .iter()
        .filter_map(|k| text(k).map(|v| (*k, v)))
        .collect();
    let level = match kind {
        "state.error" | "config.invalid" | "migration.error" => Level::Error,
        "root.offline" | "lease.lost" | "warning.raised" | "plugin.crashed" => Level::Warn,
        "plugin.failed" => Level::Error,
        "rule.applied" => match data.get("level").and_then(Value::as_str) {
            Some("warn") => Level::Warn,
            _ => Level::Info,
        },
        "scan" if data.get("reason").and_then(Value::as_str) == Some("startup") => Level::Info,
        "scan" | "ticket.created" | "ticket.updated" | "ticket.status_changed" | "ticket.moved" => {
            Level::Debug
        }
        _ => Level::Info,
    };
    let fields = logger::fields(&pairs);
    let line = if fields.is_empty() {
        kind.to_string()
    } else {
        format!("{kind} {fields}")
    };
    (level, line)
}

impl Events {
    pub fn new(logger: Arc<Logger>, backlog: usize) -> Events {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        Events {
            log: Mutex::new(Log {
                next_seq: 1,
                queue: VecDeque::new(),
            }),
            changed: Condvar::new(),
            logger,
            backlog: backlog.max(1),
            stream: format!("{nanos:x}-{:x}", std::process::id()),
        }
    }

    pub fn stream(&self) -> &str {
        &self.stream
    }

    pub fn publish(&self, kind: &str, data: Value) {
        let (level, line) = log_entry(kind, &data);
        self.logger.log(level, &line);
        let mut log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        let seq = log.next_seq;
        log.next_seq += 1;
        log.queue
            .push_back(json!({"seq": seq, "time": now_timestamp(), "type": kind, "data": data}));
        if log.queue.len() > self.backlog {
            log.queue.pop_front();
        }
        self.changed.notify_all();
    }

    /// Sequence number of the newest event published so far, 0 when there is none.
    pub fn latest(&self) -> u64 {
        self.log.lock().unwrap_or_else(|e| e.into_inner()).next_seq - 1
    }

    /// Events with a sequence number above `since`, waiting up to `wait` for the first one.
    /// `stream` is the stream identifier the client last saw, if any. A reader that cannot
    /// continue from `since` gets `gap` set and returns at once without waiting.
    pub fn since(
        &self,
        since: u64,
        stream: Option<&str>,
        wait: Duration,
        stop: &AtomicBool,
    ) -> Page {
        let deadline = Instant::now() + wait;
        let mut log = self.log.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            let latest = log.next_seq - 1;
            let oldest = log
                .queue
                .front()
                .and_then(|e| e["seq"].as_u64())
                .unwrap_or(latest + 1);
            let gap = if stream.is_some_and(|s| s != self.stream) {
                Some(Gap::StreamChanged)
            } else if since > latest {
                Some(Gap::AheadOfStream)
            } else if since + 1 < oldest {
                Some(Gap::BacklogExceeded)
            } else {
                None
            };
            let events: Vec<Value> = match gap {
                Some(Gap::StreamChanged | Gap::AheadOfStream) => Vec::new(),
                _ => log
                    .queue
                    .iter()
                    .filter(|e| e["seq"].as_u64().unwrap_or(0) > since)
                    .cloned()
                    .collect(),
            };
            let left = deadline.saturating_duration_since(Instant::now());
            if gap.is_some() || !events.is_empty() || left.is_zero() || stop.load(Ordering::Relaxed)
            {
                return Page {
                    events,
                    next: latest,
                    oldest,
                    latest,
                    gap,
                    stream: self.stream.clone(),
                };
            }
            let slice = left.min(Duration::from_millis(250));
            log = self
                .changed
                .wait_timeout(log, slice)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn events(backlog: usize) -> Events {
        Events::new(Arc::new(Logger::discard()), backlog)
    }

    #[test]
    fn events_are_sequenced_and_blocking() {
        let events = events(100);
        let stop = AtomicBool::new(false);
        let none = events.since(0, None, Duration::ZERO, &stop);
        assert!(none.events.is_empty());
        assert_eq!((none.next, none.oldest, none.latest), (0, 1, 0));
        assert!(none.gap.is_none());
        events.publish("a", json!({}));
        events.publish("b", json!({}));
        let all = events.since(0, None, Duration::ZERO, &stop);
        assert_eq!(all.events.len(), 2);
        assert_eq!(all.next, 2);
        assert_eq!(all.events[0]["seq"], 1);
        let rest = events.since(1, None, Duration::ZERO, &stop);
        assert_eq!(rest.events[0]["type"], "b");
        let idle = events.since(2, None, Duration::from_millis(10), &stop);
        assert!(idle.events.is_empty());
        assert_eq!(idle.next, 2);
    }

    #[test]
    fn a_reader_behind_the_backlog_gets_a_gap() {
        let events = events(3);
        let stop = AtomicBool::new(false);
        for i in 0..5 {
            events.publish("e", json!({"i": i}));
        }
        let behind = events.since(1, None, Duration::ZERO, &stop);
        assert_eq!(behind.gap, Some(Gap::BacklogExceeded));
        assert_eq!(behind.oldest, 3);
        assert_eq!(behind.latest, 5);
        assert_eq!(behind.next, 5);
        assert_eq!(behind.events.len(), 3);
        // Event 2 was dropped, so a reader at 1 missed it; a reader at 2 needs event 3 on.
        let edge = events.since(2, None, Duration::ZERO, &stop);
        assert!(edge.gap.is_none());
        assert_eq!(edge.events.len(), 3);
        let fine = events.since(3, None, Duration::ZERO, &stop);
        assert!(fine.gap.is_none());
        assert_eq!(fine.events.len(), 2);
    }

    #[test]
    fn a_reader_ahead_of_the_stream_or_on_another_stream_gets_a_gap() {
        let events = events(10);
        let stop = AtomicBool::new(false);
        events.publish("e", json!({}));
        let ahead = events.since(40, None, Duration::from_secs(5), &stop);
        assert_eq!(ahead.gap, Some(Gap::AheadOfStream));
        assert_eq!(ahead.next, 1);
        assert!(ahead.events.is_empty());
        let other = events.since(0, Some("elsewhere"), Duration::ZERO, &stop);
        assert_eq!(other.gap, Some(Gap::StreamChanged));
        let own = events.since(0, Some(events.stream()), Duration::ZERO, &stop);
        assert!(own.gap.is_none());
    }
}
