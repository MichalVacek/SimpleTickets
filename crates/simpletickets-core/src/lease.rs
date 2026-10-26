//! Single-daemon lease for a tickets folder.
//!
//! The daemon that runs the rules holds a lock file in the state folder with its
//! process ID and a heartbeat it refreshes regularly. Another daemon refuses to
//! start while the heartbeat is fresh; a lock whose heartbeat is older than the
//! time-to-live is stale and is taken over. Folders synced between machines are
//! covered too, since only the heartbeat decides, not the process ID.

use crate::state::{ensure_state_dir, lease_path};
use crate::time::now_timestamp;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use thiserror::Error;

/// Heartbeat age after which a lease counts as abandoned.
pub const DEFAULT_TTL: Duration = Duration::from_secs(30);

static COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Error, PartialEq, Eq)]
pub enum LeaseError {
    #[error(
        "another daemon (pid {pid}, last heartbeat {age_secs}s ago) already runs the rules for \
         this folder ({path}); stop it, or wait for the lock to go stale if it is gone"
    )]
    Held {
        pid: u32,
        age_secs: u64,
        path: PathBuf,
    },
    #[error("this daemon no longer holds the lease on {0}")]
    Lost(PathBuf),
    #[error("{path}: {message}")]
    Io { path: PathBuf, message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct LeaseData {
    pid: u32,
    /// Identifies the holder; a process ID alone can repeat across machines.
    owner: String,
    started: String,
    heartbeat_ms: u64,
}

/// A held lease. Dropping it releases the lock file if it is still ours.
#[derive(Debug)]
pub struct Lease {
    path: PathBuf,
    owner: String,
    started: String,
    ttl: Duration,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

fn new_owner() -> String {
    let count = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}-{}-{count}", std::process::id(), now_ms())
}

impl Lease {
    /// Takes the lease for the tickets folder `dir`, or reports who holds it.
    pub fn acquire(dir: &Path, ttl: Duration) -> Result<Lease, LeaseError> {
        let path = lease_path(dir);
        let io_err = |e: io::Error| LeaseError::Io {
            path: path.clone(),
            message: e.to_string(),
        };
        ensure_state_dir(dir).map_err(io_err)?;
        let lease = Lease {
            path: path.clone(),
            owner: new_owner(),
            started: now_timestamp(),
            ttl,
        };
        for _ in 0..3 {
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
            {
                Ok(mut file) => {
                    let text = lease.data_now();
                    file.write_all(text.as_bytes())
                        .and_then(|()| file.sync_all())
                        .map_err(io_err)?;
                    return Ok(lease);
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(io_err(e)),
            }
            let Ok(seen) = fs::read_to_string(&path) else {
                continue;
            };
            let (pid, age) = match serde_json::from_str::<LeaseData>(&seen) {
                Ok(d) => (d.pid, now_ms().saturating_sub(d.heartbeat_ms)),
                // Partial or foreign content: judge by the file's own age.
                Err(_) => (0, file_age_ms(&path)),
            };
            if Duration::from_millis(age) < ttl {
                return Err(LeaseError::Held {
                    pid,
                    age_secs: age / 1000,
                    path,
                });
            }
            // Stale: remove it only if it is still the content we judged.
            if fs::read_to_string(&path).is_ok_and(|now| now == seen) {
                let _ = fs::remove_file(&path);
            }
        }
        Err(LeaseError::Io {
            path,
            message: "could not take over the lock file".into(),
        })
    }

    fn data_now(&self) -> String {
        let data = LeaseData {
            pid: std::process::id(),
            owner: self.owner.clone(),
            started: self.started.clone(),
            heartbeat_ms: now_ms(),
        };
        serde_json::to_string(&data).expect("lease serializes")
    }

    fn holds(&self) -> bool {
        fs::read_to_string(&self.path)
            .ok()
            .and_then(|t| serde_json::from_str::<LeaseData>(&t).ok())
            .is_some_and(|d| d.owner == self.owner)
    }

    /// Refreshes the heartbeat. Fails with [`LeaseError::Lost`] when another
    /// daemon took the lease over in the meantime.
    pub fn renew(&self) -> Result<(), LeaseError> {
        if !self.holds() {
            return Err(LeaseError::Lost(self.path.clone()));
        }
        let temp = self.path.with_extension(format!("lock.{}.tmp", self.owner));
        fs::write(&temp, self.data_now())
            .and_then(|()| fs::rename(&temp, &self.path))
            .map_err(|e| {
                let _ = fs::remove_file(&temp);
                LeaseError::Io {
                    path: self.path.clone(),
                    message: e.to_string(),
                }
            })
    }

    pub fn ttl(&self) -> Duration {
        self.ttl
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if self.holds() {
            let _ = fs::remove_file(&self.path);
        }
    }
}

fn file_age_ms(path: &Path) -> u64 {
    fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|m| SystemTime::now().duration_since(m).ok())
        .map_or(0, |d| d.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    const TTL: Duration = Duration::from_secs(30);

    fn write_lock(dir: &Path, pid: u32, heartbeat_ms: u64) {
        ensure_state_dir(dir).unwrap();
        let data = LeaseData {
            pid,
            owner: "someone-else".into(),
            started: "2026-01-01T00:00:00Z".into(),
            heartbeat_ms,
        };
        fs::write(lease_path(dir), serde_json::to_string(&data).unwrap()).unwrap();
    }

    #[test]
    fn second_holder_is_refused_with_a_clear_error() {
        let d = tempfile::tempdir().unwrap();
        let first = Lease::acquire(d.path(), TTL).unwrap();
        let err = Lease::acquire(d.path(), TTL).unwrap_err();
        assert!(matches!(&err, LeaseError::Held { pid, .. } if *pid == std::process::id()));
        let text = err.to_string();
        assert!(text.contains("already runs the rules") && text.contains("daemon.lock"));
        drop(first);
        Lease::acquire(d.path(), TTL).unwrap();
    }

    #[test]
    fn fresh_foreign_lock_is_respected_and_stale_one_is_taken_over() {
        let d = tempfile::tempdir().unwrap();
        write_lock(d.path(), 4242, now_ms());
        assert!(matches!(
            Lease::acquire(d.path(), TTL),
            Err(LeaseError::Held { pid: 4242, .. })
        ));
        write_lock(d.path(), 4242, now_ms() - 60_000);
        let lease = Lease::acquire(d.path(), TTL).unwrap();
        assert!(lease.holds());
    }

    #[test]
    fn renewing_keeps_the_lease_alive_past_the_ttl() {
        let d = tempfile::tempdir().unwrap();
        let ttl = Duration::from_millis(300);
        let lease = Lease::acquire(d.path(), ttl).unwrap();
        for _ in 0..4 {
            std::thread::sleep(Duration::from_millis(120));
            lease.renew().unwrap();
        }
        assert!(Lease::acquire(d.path(), ttl).is_err());
        std::thread::sleep(Duration::from_millis(400));
        let taker = Lease::acquire(d.path(), ttl).unwrap();
        assert_eq!(lease.renew(), Err(LeaseError::Lost(lease_path(d.path()))));
        // A holder that lost the lease must not delete the new holder's lock.
        drop(lease);
        assert!(taker.holds());
    }

    #[test]
    fn partial_lock_content_is_judged_by_file_age() {
        let d = tempfile::tempdir().unwrap();
        ensure_state_dir(d.path()).unwrap();
        fs::write(lease_path(d.path()), "").unwrap();
        assert!(matches!(
            Lease::acquire(d.path(), TTL),
            Err(LeaseError::Held { pid: 0, .. })
        ));
        assert!(Lease::acquire(d.path(), Duration::ZERO).is_ok());
    }
}
