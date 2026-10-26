//! Append-only audit log of automatic changes, one JSON object per line.

use serde::Serialize;
use serde_json::Value;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AuditEntry {
    pub ts: String,
    pub rule: String,
    pub id: String,
    pub field: String,
    pub from: String,
    pub to: String,
    pub reason: String,
    /// `info` or `warn`, taken from the tag logging configuration.
    pub level: String,
}

#[derive(Debug, Clone)]
pub struct AuditLog {
    path: PathBuf,
}

impl AuditLog {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn append(&self, entry: &AuditEntry) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut line = serde_json::to_string(entry).expect("entry serializes");
        line.push('\n');
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        file.write_all(line.as_bytes())
    }

    /// The last `limit` entries, oldest first. Unreadable lines are skipped.
    pub fn tail(&self, limit: usize) -> Vec<Value> {
        let Ok(text) = fs::read_to_string(&self.path) else {
            return Vec::new();
        };
        let mut entries: Vec<Value> = text
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();
        let start = entries.len().saturating_sub(limit);
        entries.drain(..start);
        entries
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str) -> AuditEntry {
        AuditEntry {
            ts: "2026-01-01T00:00:00Z".into(),
            rule: "parent-in-progress".into(),
            id: id.into(),
            field: "status".into(),
            from: "todo".into(),
            to: "in-progress".into(),
            reason: "r".into(),
            level: "info".into(),
        }
    }

    #[test]
    fn appends_and_tails() {
        let dir = tempfile::tempdir().unwrap();
        let log = AuditLog::new(dir.path().join("sub/audit.jsonl"));
        assert!(log.tail(5).is_empty());
        for id in ["A", "B", "C"] {
            log.append(&entry(id)).unwrap();
        }
        let tail = log.tail(2);
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0]["id"], "B");
        assert_eq!(tail[1]["to"], "in-progress");
    }
}
