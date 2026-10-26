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
            restrict_to_owner(path)?;
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

#[cfg(unix)]
fn restrict_to_owner(_path: &Path) -> io::Result<()> {
    Ok(())
}

/// Replaces the file's ACL with a single entry for the current user.
///
/// Windows has no creation mode, so the file inherits the folder's ACL, which on a shared
/// folder may let other users read it. This runs before the token is written and fails
/// closed. It shells out to the `whoami` and `icacls` tools that ship with every Windows
/// install, which avoids a dependency on the Win32 security API for a one-time call; the
/// user is granted by SID, so the result does not depend on the display language or on
/// domain naming.
#[cfg(windows)]
fn restrict_to_owner(path: &Path) -> io::Result<()> {
    use std::process::Command;
    let who = Command::new("whoami")
        .args(["/user", "/fo", "csv", "/nh"])
        .output()?;
    let text = String::from_utf8_lossy(&who.stdout);
    let sid = parse_sid(&text)
        .ok_or_else(|| io::Error::other("cannot determine the current user's SID"))?;
    let out = Command::new("icacls")
        .arg(path)
        .args(["/inheritance:r", "/grant:r"])
        .arg(format!("*{sid}:(F)"))
        .output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "icacls failed: {}",
            String::from_utf8_lossy(&out.stdout).trim()
        )))
    }
}

/// Extracts the SID from `"DOMAIN\\user","S-1-5-..."`.
#[cfg(any(windows, test))]
fn parse_sid(whoami_csv: &str) -> Option<&str> {
    let sid = whoami_csv
        .trim()
        .rsplit(',')
        .next()?
        .trim()
        .trim_matches('"');
    (sid.starts_with("S-1-") && sid.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
        .then_some(sid)
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
    fn parses_whoami_output() {
        let line = "\"HOST\\user\",\"S-1-5-21-1-2-3-1001\"\r\n";
        assert_eq!(parse_sid(line), Some("S-1-5-21-1-2-3-1001"));
        assert_eq!(parse_sid("garbage"), None);
        assert_eq!(parse_sid("\"a\",\"S-1-5; calc\""), None);
    }

    #[cfg(windows)]
    #[test]
    fn token_acl_has_only_the_current_user() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        load_or_create(&path).unwrap();
        let out = std::process::Command::new("icacls")
            .arg(&path)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&out.stdout).to_string();
        let entries: Vec<&str> = text.lines().filter(|l| l.contains(":(")).collect();
        assert_eq!(entries.len(), 1, "{text}");
        assert!(entries[0].contains("(F)"), "{text}");
        for other in ["Everyone", "BUILTIN\\Users", "Authenticated Users"] {
            assert!(!text.contains(other), "{text}");
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
