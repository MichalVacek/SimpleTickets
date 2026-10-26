//! Persistent record of rule warnings, shown by the doctor report.
//!
//! Condition warnings (a ticket in progress while blocked, ...) stay while the
//! condition holds and are dropped when a run no longer finds them. Event
//! warnings (a parent the rules reopened) are kept as history, newest last, up
//! to [`MAX_EVENTS`]. Warnings about frozen tickets are carried over unchanged.

use crate::rules::{Warning, WarningKind};
use crate::state::{ensure_state_dir, warnings_path};
use crate::time::now_timestamp;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::Path;

/// Event warnings kept before the oldest are dropped.
pub const MAX_EVENTS: usize = 50;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredWarning {
    pub rule: String,
    pub id: String,
    pub message: String,
    /// `condition` or `event`.
    pub kind: String,
    pub first_seen: String,
}

impl std::fmt::Display for StoredWarning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "warning [{}] {}: {} (since {})",
            self.rule, self.id, self.message, self.first_seen
        )
    }
}

fn kind_key(kind: WarningKind) -> &'static str {
    match kind {
        WarningKind::Condition => "condition",
        WarningKind::Event => "event",
    }
}

/// The stored warnings; an unreadable record counts as empty, since it is
/// rebuilt by the next run.
pub fn load(dir: &Path) -> Vec<StoredWarning> {
    fs::read_to_string(warnings_path(dir))
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

fn record(w: &Warning, now: &str) -> StoredWarning {
    StoredWarning {
        rule: w.rule.to_string(),
        id: w.id.clone(),
        message: w.message.clone(),
        kind: kind_key(w.kind).into(),
        first_seen: now.to_string(),
    }
}

/// Brings the record up to date after a run and returns the warnings that are new.
///
/// `conditions` are all condition warnings of the final state, `events` the
/// event warnings of changes the run applied, and `frozen` the tickets whose
/// stored condition warnings must be left as they are.
pub fn update(
    dir: &Path,
    conditions: &[Warning],
    events: &[Warning],
    frozen: &BTreeSet<String>,
) -> io::Result<Vec<StoredWarning>> {
    let stored = load(dir);
    let now = now_timestamp();
    let mut out: Vec<StoredWarning> = Vec::new();
    let mut raised = Vec::new();

    for old in stored.iter().filter(|s| s.kind == "condition") {
        let current = conditions
            .iter()
            .find(|c| c.rule == old.rule && c.id == old.id);
        if frozen.contains(&old.id) {
            out.push(old.clone());
        } else if let Some(c) = current {
            out.push(StoredWarning {
                message: c.message.clone(),
                ..old.clone()
            });
        }
    }
    for c in conditions {
        if !out.iter().any(|o| o.rule == c.rule && o.id == c.id) {
            let new = record(c, &now);
            raised.push(new.clone());
            out.push(new);
        }
    }

    let mut history: Vec<StoredWarning> = stored
        .iter()
        .filter(|s| s.kind == "event")
        .cloned()
        .collect();
    for e in events {
        let new = record(e, &now);
        raised.push(new.clone());
        history.push(new);
    }
    let excess = history.len().saturating_sub(MAX_EVENTS);
    history.drain(..excess);
    out.extend(history);

    if out != stored {
        ensure_state_dir(dir)?;
        let path = warnings_path(dir);
        let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        let text = serde_json::to_string_pretty(&out).expect("warnings serialize");
        let result = fs::write(&temp, text).and_then(|()| fs::rename(&temp, &path));
        if result.is_err() {
            let _ = fs::remove_file(&temp);
        }
        result?;
    }
    Ok(raised)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn warning(rule: &'static str, id: &str, kind: WarningKind) -> Warning {
        Warning {
            rule,
            id: id.into(),
            message: format!("{rule} on {id}"),
            kind,
        }
    }

    fn cond(rule: &'static str, id: &str) -> Warning {
        warning(rule, id, WarningKind::Condition)
    }

    #[test]
    fn conditions_persist_while_they_hold_and_clear_after() {
        let d = tempfile::tempdir().unwrap();
        let none = BTreeSet::new();
        let raised = update(d.path(), &[cond("blocked-in-progress", "A")], &[], &none).unwrap();
        assert_eq!(raised.len(), 1);
        assert_eq!(load(d.path()).len(), 1);

        let again = update(d.path(), &[cond("blocked-in-progress", "A")], &[], &none).unwrap();
        assert!(again.is_empty());
        assert_eq!(load(d.path())[0].first_seen, raised[0].first_seen);

        update(d.path(), &[], &[], &none).unwrap();
        assert!(load(d.path()).is_empty());
    }

    #[test]
    fn frozen_tickets_keep_their_stored_warnings() {
        let d = tempfile::tempdir().unwrap();
        let none = BTreeSet::new();
        update(
            d.path(),
            &[cond("blocked-in-progress", "A"), cond("review-stale", "B")],
            &[],
            &none,
        )
        .unwrap();
        let frozen = BTreeSet::from(["A".to_string()]);
        update(d.path(), &[], &[], &frozen).unwrap();
        let left = load(d.path());
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].id, "A");
    }

    #[test]
    fn events_are_kept_as_capped_history() {
        let d = tempfile::tempdir().unwrap();
        let none = BTreeSet::new();
        let event = warning("parent-in-progress", "P", WarningKind::Event);
        for _ in 0..MAX_EVENTS + 5 {
            update(d.path(), &[], std::slice::from_ref(&event), &none).unwrap();
        }
        let stored = load(d.path());
        assert_eq!(stored.len(), MAX_EVENTS);
        assert!(stored.iter().all(|s| s.kind == "event"));
        // History is untouched by runs without new events.
        update(d.path(), &[], &[], &none).unwrap();
        assert_eq!(load(d.path()).len(), MAX_EVENTS);
    }

    #[test]
    fn unreadable_record_is_rebuilt() {
        let d = tempfile::tempdir().unwrap();
        ensure_state_dir(d.path()).unwrap();
        fs::write(warnings_path(d.path()), "{oops").unwrap();
        assert!(load(d.path()).is_empty());
        update(
            d.path(),
            &[cond("review-stale", "P")],
            &[],
            &BTreeSet::new(),
        )
        .unwrap();
        assert_eq!(load(d.path()).len(), 1);
    }
}
