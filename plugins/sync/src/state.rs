//! What the sync plugin remembers, kept in `.simpletickets/sync/` and nowhere else: never
//! in ticket files. One JSON file per target maps ticket ids to remote references and
//! records what was last synced.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const STATE_VERSION: u32 = 1;
/// How many resolved conflicts are kept as history per target.
const RESOLVED_KEPT: usize = 100;
/// A lock not touched for this long belongs to a process that died.
pub const LOCK_TTL_SECONDS: u64 = 60;

pub fn sync_dir(tickets_dir: &Path) -> PathBuf {
    tickets_dir.join(".simpletickets").join("sync")
}

pub fn config_path(tickets_dir: &Path) -> PathBuf {
    sync_dir(tickets_dir).join("config.yml")
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// `YYYY-MM-DDTHH:MM:SSZ` for a Unix time.
pub fn iso(unix: u64) -> String {
    let days = (unix / 86_400) as i64;
    let rest = unix % 86_400;
    // Civil date from a day count (proleptic Gregorian calendar).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

pub fn now_iso() -> String {
    iso(now_unix())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum Phase {
    #[default]
    Synced,
    /// The last attempt failed; see `error` and `next_attempt`.
    Error,
    /// Local and remote changed the same field and the policy is `manual`.
    Conflict,
    /// The remote item is gone and was not recreated.
    RemoteMissing,
    /// The ticket was deleted locally and the remote item was closed.
    Closed,
    /// The ticket was deleted locally and the remote item was left alone.
    Deleted,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Synced => "synced",
            Phase::Error => "error",
            Phase::Conflict => "conflict",
            Phase::RemoteMissing => "remote-missing",
            Phase::Closed => "closed",
            Phase::Deleted => "deleted",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TicketState {
    /// The remote item, once one exists.
    #[serde(rename = "ref", default)]
    pub remote: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    /// Digest of the payload last pushed.
    #[serde(default)]
    pub hash: String,
    /// The remote's values as of the last sync: the base for telling who changed what.
    #[serde(default)]
    pub base: Map<String, Value>,
    /// Remote references of the blockers linked at the last push.
    #[serde(default)]
    pub links: Vec<String>,
    #[serde(default)]
    pub phase: Phase,
    #[serde(default)]
    pub error: Option<String>,
    /// Digest of a payload the remote refused for good; it is not sent again.
    #[serde(default)]
    pub failed_hash: Option<String>,
    #[serde(default)]
    pub attempts: u32,
    /// Unix time before which a transient failure is not retried.
    #[serde(default)]
    pub next_attempt: Option<u64>,
    #[serde(default)]
    pub warnings: Vec<String>,
    #[serde(default)]
    pub synced_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conflict {
    pub id: String,
    /// `field` (both sides changed it) or `remote-missing`.
    pub kind: String,
    #[serde(default)]
    pub field: Option<String>,
    /// The local value, mapped for the target.
    #[serde(default)]
    pub local: Value,
    /// The remote value as the connector reported it.
    #[serde(default)]
    pub remote: Value,
    pub detected_at: String,
    /// `local-wins`, `remote-wins`, or null while it waits for a decision.
    #[serde(default)]
    pub resolution: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TargetState {
    pub version: u32,
    #[serde(default)]
    pub tickets: BTreeMap<String, TicketState>,
    #[serde(default)]
    pub conflicts: Vec<Conflict>,
    /// The connector's change cursor.
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub last_pass: Option<String>,
    #[serde(default)]
    pub last_error: Option<String>,
}

impl Default for TargetState {
    fn default() -> TargetState {
        TargetState {
            version: STATE_VERSION,
            tickets: BTreeMap::new(),
            conflicts: Vec::new(),
            cursor: None,
            last_pass: None,
            last_error: None,
        }
    }
}

impl TargetState {
    fn path(tickets_dir: &Path, target: &str) -> PathBuf {
        sync_dir(tickets_dir).join(format!("{target}.json"))
    }

    pub fn load(tickets_dir: &Path, target: &str) -> Result<TargetState, String> {
        let path = TargetState::path(tickets_dir, target);
        let text = match fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(TargetState::default()),
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        };
        let state: TargetState = serde_json::from_str(&text)
            .map_err(|e| format!("{} is damaged: {e}", path.display()))?;
        if state.version > STATE_VERSION {
            return Err(format!(
                "{} was written by a newer version of the sync plugin",
                path.display()
            ));
        }
        Ok(state)
    }

    /// Writes the state through a temporary file, so a crash never leaves half a file.
    pub fn save(&mut self, tickets_dir: &Path, target: &str) -> Result<(), String> {
        let resolved = self
            .conflicts
            .iter()
            .filter(|c| c.resolution.is_some())
            .count();
        if resolved > RESOLVED_KEPT {
            let mut drop = resolved - RESOLVED_KEPT;
            self.conflicts.retain(|c| {
                if c.resolution.is_some() && drop > 0 {
                    drop -= 1;
                    false
                } else {
                    true
                }
            });
        }
        let dir = sync_dir(tickets_dir);
        fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let path = TargetState::path(tickets_dir, target);
        let tmp = path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(&self).map_err(|e| e.to_string())?;
        fs::write(&tmp, text + "\n").map_err(|e| format!("cannot write {}: {e}", tmp.display()))?;
        fs::rename(&tmp, &path).map_err(|e| format!("cannot replace {}: {e}", path.display()))
    }

    pub fn open_conflicts(&self) -> impl Iterator<Item = &Conflict> {
        self.conflicts.iter().filter(|c| c.resolution.is_none())
    }
}

/// Exclusive ownership of the state folder: one process syncs at a time.
pub struct Lock {
    path: PathBuf,
}

impl Lock {
    fn path(tickets_dir: &Path) -> PathBuf {
        sync_dir(tickets_dir).join("lock")
    }

    fn age(path: &Path) -> Option<u64> {
        let modified = fs::metadata(path).ok()?.modified().ok()?;
        SystemTime::now()
            .duration_since(modified)
            .ok()
            .map(|d| d.as_secs())
    }

    /// Whether a live process holds the lock.
    pub fn is_held(tickets_dir: &Path) -> bool {
        Lock::age(&Lock::path(tickets_dir)).is_some_and(|age| age <= LOCK_TTL_SECONDS)
    }

    pub fn acquire(tickets_dir: &Path) -> Result<Lock, String> {
        let dir = sync_dir(tickets_dir);
        fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let path = Lock::path(tickets_dir);
        for _ in 0..2 {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    let _ = write!(file, "{}", std::process::id());
                    return Ok(Lock { path });
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                    if Lock::age(&path).is_some_and(|age| age > LOCK_TTL_SECONDS) {
                        let _ = fs::remove_file(&path);
                        continue;
                    }
                    return Err("another sync process is running for this folder".into());
                }
                Err(e) => return Err(format!("cannot create {}: {e}", path.display())),
            }
        }
        Err("cannot take over a stale sync lock".into())
    }

    /// Takes the lock whatever its state. Only for a process the daemon supervises, which
    /// the daemon runs at most once: a lock left by a killed predecessor is not a rival.
    pub fn take_over(tickets_dir: &Path) -> Result<Lock, String> {
        let dir = sync_dir(tickets_dir);
        fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        let path = Lock::path(tickets_dir);
        fs::write(&path, std::process::id().to_string())
            .map_err(|e| format!("cannot create {}: {e}", path.display()))?;
        Ok(Lock { path })
    }

    /// Marks the owner as alive.
    pub fn touch(&self) {
        let _ = fs::write(&self.path, std::process::id().to_string());
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

/// A request from the command line to the running service.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct Request {
    #[serde(default)]
    pub ids: Vec<String>,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub force: bool,
    /// `local` or `remote`: how to settle the conflicts of `ids`.
    #[serde(default)]
    pub prefer: Option<String>,
}

fn request_path(tickets_dir: &Path) -> PathBuf {
    sync_dir(tickets_dir).join("request.json")
}

pub fn write_request(tickets_dir: &Path, request: &Request) -> Result<(), String> {
    let dir = sync_dir(tickets_dir);
    fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let text = serde_json::to_string(request).map_err(|e| e.to_string())?;
    let tmp = dir.join("request.json.tmp");
    fs::write(&tmp, text).map_err(|e| e.to_string())?;
    fs::rename(&tmp, request_path(tickets_dir)).map_err(|e| e.to_string())
}

/// Takes the pending request, if any, removing it.
pub fn take_request(tickets_dir: &Path) -> Option<Request> {
    let path = request_path(tickets_dir);
    let text = fs::read_to_string(&path).ok()?;
    let _ = fs::remove_file(&path);
    serde_json::from_str(&text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn formats_unix_times() {
        assert_eq!(iso(0), "1970-01-01T00:00:00Z");
        assert_eq!(iso(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(iso(1_793_059_200 + 3_725), "2026-10-27T01:02:05Z");
    }

    #[test]
    fn state_round_trips_and_missing_files_are_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(TargetState::load(dir.path(), "t")
            .unwrap()
            .tickets
            .is_empty());
        let mut state = TargetState::default();
        state.tickets.insert(
            "ST-1".into(),
            TicketState {
                remote: Some("K-1".into()),
                hash: "h".into(),
                base: json!({"status": "To Do"}).as_object().unwrap().clone(),
                ..TicketState::default()
            },
        );
        state.cursor = Some("c".into());
        state.save(dir.path(), "t").unwrap();
        let loaded = TargetState::load(dir.path(), "t").unwrap();
        assert_eq!(loaded.tickets["ST-1"].remote.as_deref(), Some("K-1"));
        assert_eq!(loaded.tickets["ST-1"].base["status"], "To Do");
        assert_eq!(loaded.cursor.as_deref(), Some("c"));
        assert!(!sync_dir(dir.path()).join("t.json.tmp").exists());
    }

    #[test]
    fn damaged_or_newer_state_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = sync_dir(dir.path());
        fs::create_dir_all(&path).unwrap();
        fs::write(path.join("a.json"), "{not json").unwrap();
        assert!(TargetState::load(dir.path(), "a").is_err());
        fs::write(path.join("b.json"), r#"{"version": 99}"#).unwrap();
        assert!(TargetState::load(dir.path(), "b")
            .unwrap_err()
            .contains("newer"));
    }

    #[test]
    fn only_the_newest_resolved_conflicts_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let mut state = TargetState::default();
        for n in 0..RESOLVED_KEPT + 5 {
            state.conflicts.push(Conflict {
                id: format!("ST-{n}"),
                kind: "field".into(),
                field: None,
                local: Value::Null,
                remote: Value::Null,
                detected_at: now_iso(),
                resolution: Some("local-wins".into()),
            });
        }
        state.conflicts.push(Conflict {
            id: "OPEN".into(),
            kind: "field".into(),
            field: None,
            local: Value::Null,
            remote: Value::Null,
            detected_at: now_iso(),
            resolution: None,
        });
        state.save(dir.path(), "t").unwrap();
        assert_eq!(state.conflicts.len(), RESOLVED_KEPT + 1);
        assert_eq!(state.conflicts[0].id, "ST-5");
        assert_eq!(state.open_conflicts().count(), 1);
    }

    #[test]
    fn the_lock_is_exclusive_and_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!Lock::is_held(dir.path()));
        let lock = Lock::acquire(dir.path()).unwrap();
        assert!(Lock::is_held(dir.path()));
        assert!(Lock::acquire(dir.path()).is_err());
        lock.touch();
        drop(lock);
        assert!(!Lock::is_held(dir.path()));
        assert!(Lock::acquire(dir.path()).is_ok());
    }

    #[test]
    fn requests_are_taken_once() {
        let dir = tempfile::tempdir().unwrap();
        assert!(take_request(dir.path()).is_none());
        let request = Request {
            ids: vec!["ST-1".into()],
            force: true,
            prefer: Some("local".into()),
            target: None,
        };
        write_request(dir.path(), &request).unwrap();
        assert_eq!(take_request(dir.path()), Some(request));
        assert!(take_request(dir.path()).is_none());
    }
}
