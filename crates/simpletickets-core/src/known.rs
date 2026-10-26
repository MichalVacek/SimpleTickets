//! Persistent record of every ticket ID the index has seen.
//!
//! An ID that was known but is no longer found is a tombstone: links to it keep
//! their meaning (a missing blocker stays an open blocker) until the tombstone
//! is acknowledged with [`forget`]. Entries are only ever added by scans.

use crate::state::{ensure_state_dir, known_ids_path};
use crate::time::now_timestamp;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownEntry {
    /// Where the ticket was last seen.
    pub path: String,
    pub first_seen: String,
}

#[derive(Default, Serialize, Deserialize)]
struct FileData {
    ids: BTreeMap<String, KnownEntry>,
}

/// All known IDs. A missing file is an empty record; an unreadable or corrupt
/// one is an error, so that it is never overwritten with a smaller record.
pub fn load(dir: &Path) -> Result<BTreeMap<String, KnownEntry>, String> {
    let path = known_ids_path(dir);
    match fs::read_to_string(&path) {
        Ok(text) => serde_json::from_str::<FileData>(&text)
            .map(|d| d.ids)
            .map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

fn save(dir: &Path, ids: BTreeMap<String, KnownEntry>) -> io::Result<()> {
    ensure_state_dir(dir)?;
    let path = known_ids_path(dir);
    let temp: PathBuf = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let text = serde_json::to_string_pretty(&FileData { ids }).expect("record serializes");
    let result = fs::write(&temp, text).and_then(|()| fs::rename(&temp, &path));
    if result.is_err() {
        let _ = fs::remove_file(&temp);
    }
    result
}

/// Adds the given present tickets (ID and path) that are new or have moved.
/// Nothing is removed; the file is only written when something changed.
pub fn record<'a>(
    dir: &Path,
    present: impl IntoIterator<Item = (&'a str, &'a Path)>,
) -> io::Result<()> {
    let mut ids = load(dir).map_err(|m| io::Error::new(io::ErrorKind::InvalidData, m))?;
    let mut changed = false;
    for (id, path) in present {
        let path = path.display().to_string();
        match ids.get_mut(id) {
            Some(entry) if entry.path == path => {}
            Some(entry) => {
                entry.path = path;
                changed = true;
            }
            None => {
                ids.insert(
                    id.to_string(),
                    KnownEntry {
                        path,
                        first_seen: now_timestamp(),
                    },
                );
                changed = true;
            }
        }
    }
    if changed {
        save(dir, ids)?;
    }
    Ok(())
}

/// Removes `id` from the record. Returns false if it was not known.
pub fn forget(dir: &Path, id: &str) -> io::Result<bool> {
    let mut ids = load(dir).map_err(|m| io::Error::new(io::ErrorKind::InvalidData, m))?;
    if ids.remove(id).is_none() {
        return Ok(false);
    }
    save(dir, ids)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_adds_only_and_forgets() {
        let d = tempfile::tempdir().unwrap();
        assert!(load(d.path()).unwrap().is_empty());
        record(d.path(), [("A", Path::new("/t/a.md"))]).unwrap();
        record(d.path(), [("B", Path::new("/t/b.md"))]).unwrap();
        let ids = load(d.path()).unwrap();
        assert_eq!(ids.keys().collect::<Vec<_>>(), ["A", "B"]);
        record(d.path(), [("A", Path::new("/t/moved/a.md"))]).unwrap();
        assert_eq!(load(d.path()).unwrap()["A"].path, "/t/moved/a.md");
        assert!(forget(d.path(), "A").unwrap());
        assert!(!forget(d.path(), "A").unwrap());
        assert_eq!(load(d.path()).unwrap().keys().collect::<Vec<_>>(), ["B"]);
    }

    #[test]
    fn corrupt_file_is_an_error_and_is_not_overwritten() {
        let d = tempfile::tempdir().unwrap();
        ensure_state_dir(d.path()).unwrap();
        fs::write(known_ids_path(d.path()), "{oops").unwrap();
        assert!(load(d.path()).is_err());
        assert!(record(d.path(), [("A", Path::new("/a.md"))]).is_err());
        assert_eq!(
            fs::read_to_string(known_ids_path(d.path())).unwrap(),
            "{oops"
        );
    }
}
