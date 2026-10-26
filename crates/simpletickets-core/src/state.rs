//! Locations of the files SimpleTickets keeps next to the tickets.
//!
//! Everything lives in a hidden `.simpletickets` folder inside the tickets
//! folder. Scans skip hidden folders, so these files are never read as tickets.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Name of the hidden state folder inside the tickets folder.
pub const STATE_DIR: &str = ".simpletickets";

/// The state folder for a tickets folder.
pub fn state_dir(dir: &Path) -> PathBuf {
    dir.join(STATE_DIR)
}

/// Creates the state folder if needed, with a `.gitignore` that excludes its content.
pub fn ensure_state_dir(dir: &Path) -> io::Result<PathBuf> {
    let state = state_dir(dir);
    fs::create_dir_all(&state)?;
    let ignore = state.join(".gitignore");
    if !ignore.exists() {
        fs::write(ignore, "*\n")?;
    }
    Ok(state)
}

pub fn audit_log_path(dir: &Path) -> PathBuf {
    state_dir(dir).join("audit.jsonl")
}

pub fn known_ids_path(dir: &Path) -> PathBuf {
    state_dir(dir).join("known-ids.json")
}

pub fn lease_path(dir: &Path) -> PathBuf {
    state_dir(dir).join("daemon.lock")
}

pub fn templates_dir(dir: &Path) -> PathBuf {
    state_dir(dir).join("templates")
}

pub fn token_path(dir: &Path) -> PathBuf {
    state_dir(dir).join("token")
}

pub fn journal_path(dir: &Path) -> PathBuf {
    state_dir(dir).join("migration.json")
}

/// True when `path` is `dir` or lies inside it, comparing resolved paths where possible.
pub fn is_within(path: &Path, dir: &Path) -> bool {
    resolve(path).starts_with(resolve(dir))
}

/// Canonicalizes the longest existing ancestor of `path` and re-appends the rest.
pub fn resolve(path: &Path) -> PathBuf {
    let mut tail = Vec::new();
    let mut current = path;
    loop {
        if let Ok(real) = fs::canonicalize(current) {
            return tail.iter().rev().fold(real, |acc, part| acc.join(part));
        }
        match (current.parent(), current.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name);
                current = parent;
            }
            _ => return path.to_path_buf(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_dir_is_self_ignoring() {
        let dir = tempfile::tempdir().unwrap();
        let state = ensure_state_dir(dir.path()).unwrap();
        assert_eq!(fs::read_to_string(state.join(".gitignore")).unwrap(), "*\n");
        ensure_state_dir(dir.path()).unwrap();
    }

    #[test]
    fn within_handles_missing_and_nested_paths() {
        let dir = tempfile::tempdir().unwrap();
        let inner = dir.path().join("a/b");
        fs::create_dir_all(&inner).unwrap();
        assert!(is_within(&inner, dir.path()));
        assert!(is_within(&dir.path().join("new/x.md"), dir.path()));
        assert!(!is_within(dir.path(), &inner));
    }
}
