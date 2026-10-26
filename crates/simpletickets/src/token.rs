//! The API token: generated on first run, kept in a private file.

use std::fs;
use std::io::{self, Write};
use std::path::Path;

const TOKEN_BYTES: usize = 32;

/// Reads the token from `path`, creating the file with a new random token if missing.
///
/// The file is created readable and writable by the owner only.
pub fn load_or_create(path: &Path) -> io::Result<String> {
    match fs::read_to_string(path) {
        Ok(text) if !text.trim().is_empty() => return Ok(text.trim().to_string()),
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let token = generate()?;
    match create_private(path) {
        Ok(mut file) => {
            file.write_all(token.as_bytes())?;
            file.sync_all()?;
            Ok(token)
        }
        // Another process created it first.
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
            Ok(fs::read_to_string(path)?.trim().to_string())
        }
        Err(e) => Err(e),
    }
}

fn generate() -> io::Result<String> {
    let mut bytes = [0u8; TOKEN_BYTES];
    getrandom::fill(&mut bytes).map_err(|e| io::Error::other(e.to_string()))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

#[cfg(unix)]
fn create_private(path: &Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn create_private(path: &Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
}

/// Compares two strings without stopping at the first difference.
pub fn matches(expected: &str, given: &str) -> bool {
    let (a, b) = (expected.as_bytes(), given.as_bytes());
    let mut diff = a.len() ^ b.len();
    for i in 0..a.len().max(b.len()) {
        diff |= usize::from(a.get(i).copied().unwrap_or(0) ^ b.get(i).copied().unwrap_or(0));
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_once_with_private_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/token");
        let first = load_or_create(&path).unwrap();
        assert_eq!(first.len(), TOKEN_BYTES * 2);
        assert_eq!(load_or_create(&path).unwrap(), first);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn tokens_differ_between_files() {
        let dir = tempfile::tempdir().unwrap();
        let a = load_or_create(&dir.path().join("a")).unwrap();
        let b = load_or_create(&dir.path().join("b")).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn comparison() {
        assert!(matches("abc", "abc"));
        assert!(!matches("abc", "abd"));
        assert!(!matches("abc", "ab"));
        assert!(!matches("abc", ""));
    }
}
