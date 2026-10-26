//! Safe frontmatter writes.
//!
//! A write patches only the given frontmatter keys, succeeds only if the file
//! still has the content the caller last read (compare-and-swap on the content
//! hash), refuses files that changed very recently, and replaces the file
//! atomically through a temporary file in the same directory.

use crate::frontmatter::{Document, FrontmatterError, Value};
use crate::hash::content_hash;
use crate::ticket::{ParseError, Ticket};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};
use thiserror::Error;

/// Files modified more recently than this are left alone by default.
pub const DEFAULT_MIN_AGE: Duration = Duration::from_secs(2);

#[derive(Debug, Error)]
pub enum WriteError {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{0} changed since it was read")]
    Conflict(PathBuf),
    #[error("{0} was modified too recently")]
    RecentlyModified(PathBuf),
    #[error("{path} is not a valid ticket: {error}")]
    Malformed { path: PathBuf, error: ParseError },
    #[error("patch would produce an invalid ticket: {0}")]
    InvalidPatch(String),
}

#[derive(Debug, Clone, Copy)]
pub struct WriteOptions {
    /// Minimum time since the file's last modification; zero disables the check.
    pub min_age: Duration,
}

impl Default for WriteOptions {
    fn default() -> Self {
        Self {
            min_age: DEFAULT_MIN_AGE,
        }
    }
}

/// A set of changes to owned frontmatter keys. The `id` cannot be patched.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Patch {
    fields: Vec<(&'static str, Option<Value>)>,
}

impl Patch {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    fn put(mut self, key: &'static str, value: Option<Value>) -> Self {
        self.fields.retain(|(k, _)| *k != key);
        self.fields.push((key, value));
        self
    }

    fn scalar(self, key: &'static str, value: Option<&str>) -> Self {
        self.put(key, value.map(|v| Value::Scalar(v.to_string())))
    }

    fn list(self, key: &'static str, items: &[String]) -> Self {
        self.put(
            key,
            (!items.is_empty()).then(|| Value::List(items.to_vec())),
        )
    }

    pub fn title(self, title: &str) -> Self {
        self.scalar("title", Some(title))
    }

    pub fn status(self, status: &str) -> Self {
        self.scalar("status", Some(status))
    }

    pub fn priority(self, priority: Option<&str>) -> Self {
        self.scalar("priority", priority)
    }

    pub fn due(self, due: Option<&str>) -> Self {
        self.scalar("due", due)
    }

    pub fn parent(self, parent: Option<&str>) -> Self {
        self.scalar("parent", parent)
    }

    pub fn tags(self, tags: &[String]) -> Self {
        self.list("tags", tags)
    }

    pub fn blocked_by(self, ids: &[String]) -> Self {
        self.list("blocked_by", ids)
    }

    pub fn related(self, ids: &[String]) -> Self {
        self.list("related", ids)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteOutcome {
    /// Hash of the file content after the write.
    pub hash: String,
    /// False when the patch did not alter the file and nothing was written.
    pub changed: bool,
}

/// Applies `patch` to the ticket file at `path`.
///
/// `expected_hash` is the content hash the caller last saw for the file.
pub fn patch_file(
    path: &Path,
    expected_hash: &str,
    patch: &Patch,
    options: WriteOptions,
) -> Result<WriteOutcome, WriteError> {
    let io = |source| WriteError::Io {
        path: path.to_path_buf(),
        source,
    };

    let before = fs::metadata(path).map_err(io)?;
    let bytes = fs::read(path).map_err(io)?;
    if content_hash(&bytes) != expected_hash {
        return Err(WriteError::Conflict(path.to_path_buf()));
    }
    if is_too_recent(&before, options.min_age) {
        return Err(WriteError::RecentlyModified(path.to_path_buf()));
    }

    let malformed = |error| WriteError::Malformed {
        path: path.to_path_buf(),
        error,
    };
    let text = String::from_utf8(bytes).map_err(|_| {
        malformed(ParseError {
            line: None,
            message: "file is not valid UTF-8".into(),
        })
    })?;
    let mut doc = Document::parse(text).map_err(|e| malformed(e.into()))?;
    Ticket::from_document(&doc).map_err(malformed)?;

    for (key, value) in &patch.fields {
        doc.set(key, value.as_ref())
            .map_err(|e: FrontmatterError| WriteError::InvalidPatch(e.to_string()))?;
    }
    Ticket::from_document(&doc).map_err(|e| WriteError::InvalidPatch(e.to_string()))?;

    let new_hash = content_hash(doc.text().as_bytes());
    if new_hash == expected_hash {
        return Ok(WriteOutcome {
            hash: new_hash,
            changed: false,
        });
    }

    let temp = temp_path(path, &new_hash);
    let result = write_temp(&temp, doc.text(), &before)
        .and_then(|()| {
            // Narrow the window for a concurrent edit between the first read and the rename.
            Ok(content_hash(&fs::read(path)?) == expected_hash)
        })
        .and_then(|unchanged| {
            if unchanged {
                fs::rename(&temp, path).map(|()| true)
            } else {
                Ok(false)
            }
        });
    match result {
        Ok(true) => {
            sync_parent(path);
            Ok(WriteOutcome {
                hash: new_hash,
                changed: true,
            })
        }
        Ok(false) => {
            let _ = fs::remove_file(&temp);
            Err(WriteError::Conflict(path.to_path_buf()))
        }
        Err(e) => {
            let _ = fs::remove_file(&temp);
            Err(io(e))
        }
    }
}

fn is_too_recent(meta: &fs::Metadata, min_age: Duration) -> bool {
    if min_age.is_zero() {
        return false;
    }
    match meta
        .modified()
        .ok()
        .and_then(|m| SystemTime::now().duration_since(m).ok())
    {
        Some(age) => age < min_age,
        // A modification time in the future counts as recent.
        None => meta.modified().is_ok(),
    }
}

fn temp_path(path: &Path, hash: &str) -> PathBuf {
    let name = path
        .file_name()
        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
    path.with_file_name(format!(".{name}.{}.{}.tmp", &hash[..8], std::process::id()))
}

fn write_temp(temp: &Path, text: &str, original: &fs::Metadata) -> std::io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temp)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    fs::set_permissions(temp, original.permissions())?;
    Ok(())
}

#[cfg(unix)]
fn sync_parent(path: &Path) {
    if let Some(dir) = path.parent().and_then(|d| fs::File::open(d).ok()) {
        let _ = dir.sync_all();
    }
}

#[cfg(not(unix))]
fn sync_parent(_path: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: WriteOptions = WriteOptions {
        min_age: Duration::ZERO,
    };
    const SRC: &str =
        "---\r\n# note\r\nid: A\r\ntitle: T\r\nstatus: todo\r\nx-extra: [1, 2]\r\n---\r\nBody\r\n";

    fn file(text: &str) -> (tempfile::TempDir, PathBuf, String) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.md");
        fs::write(&path, text).unwrap();
        (dir, path, content_hash(text.as_bytes()))
    }

    fn leftovers(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| n != "a.md")
            .collect()
    }

    #[test]
    fn patches_only_requested_keys() {
        let (dir, path, hash) = file(SRC);
        let out = patch_file(&path, &hash, &Patch::new().status("done"), NOW).unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text, SRC.replace("status: todo", "status: done"));
        assert!(out.changed);
        assert_eq!(out.hash, content_hash(text.as_bytes()));
        assert!(leftovers(dir.path()).is_empty());
    }

    #[test]
    fn returned_hash_allows_consecutive_writes() {
        let (_dir, path, hash) = file(SRC);
        let a = patch_file(&path, &hash, &Patch::new().status("done"), NOW).unwrap();
        let b = patch_file(&path, &a.hash, &Patch::new().due(Some("2026-12-01")), NOW).unwrap();
        let t = Ticket::parse(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(t.status, "done");
        assert_eq!(t.due.as_deref(), Some("2026-12-01"));
        assert_ne!(a.hash, b.hash);
    }

    #[test]
    fn stale_hash_is_a_conflict_and_leaves_file_alone() {
        let (dir, path, hash) = file(SRC);
        fs::write(&path, SRC.replace("Body", "Edited")).unwrap();
        let err = patch_file(&path, &hash, &Patch::new().status("done"), NOW).unwrap_err();
        assert!(matches!(err, WriteError::Conflict(_)));
        assert!(fs::read_to_string(&path).unwrap().contains("Edited"));
        assert!(leftovers(dir.path()).is_empty());
    }

    #[test]
    fn recently_modified_files_are_skipped() {
        let (_dir, path, hash) = file(SRC);
        let err = patch_file(
            &path,
            &hash,
            &Patch::new().status("done"),
            WriteOptions::default(),
        )
        .unwrap_err();
        assert!(matches!(err, WriteError::RecentlyModified(_)));
        assert_eq!(fs::read_to_string(&path).unwrap(), SRC);
    }

    #[test]
    fn old_enough_files_are_written() {
        let (_dir, path, hash) = file(SRC);
        let opts = WriteOptions {
            min_age: Duration::from_millis(50),
        };
        std::thread::sleep(Duration::from_millis(120));
        patch_file(&path, &hash, &Patch::new().status("done"), opts).unwrap();
    }

    #[test]
    fn malformed_files_are_never_rewritten() {
        let bad = "---\nid: A\ntitle: [oops\nstatus: todo\n---\n";
        let (dir, path, hash) = file(bad);
        let err = patch_file(&path, &hash, &Patch::new().status("done"), NOW).unwrap_err();
        assert!(matches!(err, WriteError::Malformed { .. }));
        assert_eq!(fs::read_to_string(&path).unwrap(), bad);
        assert!(leftovers(dir.path()).is_empty());
    }

    #[test]
    fn invalid_patch_is_rejected_without_writing() {
        let (dir, path, hash) = file(SRC);
        let err = patch_file(&path, &hash, &Patch::new().status(""), NOW).unwrap_err();
        assert!(matches!(err, WriteError::InvalidPatch(_)));
        let err = patch_file(&path, &hash, &Patch::new().title("a\nb"), NOW).unwrap_err();
        assert!(matches!(err, WriteError::InvalidPatch(_)));
        assert_eq!(fs::read_to_string(&path).unwrap(), SRC);
        assert!(leftovers(dir.path()).is_empty());
    }

    #[test]
    fn no_op_patch_does_not_write() {
        let (_dir, path, hash) = file(SRC);
        let before = fs::metadata(&path).unwrap().modified().unwrap();
        let out = patch_file(&path, &hash, &Patch::new().status("todo"), NOW).unwrap();
        assert!(!out.changed);
        assert_eq!(out.hash, hash);
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), before);
    }

    #[test]
    fn adds_removes_and_sets_lists() {
        let (_dir, path, hash) = file("---\nid: A\ntitle: T\nstatus: todo\ntags: [x]\n---\nB\n");
        let patch = Patch::new()
            .tags(&[])
            .blocked_by(&["B".to_string(), "C".to_string()])
            .priority(Some("high"));
        patch_file(&path, &hash, &patch, NOW).unwrap();
        let t = Ticket::parse(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(t.tags.is_empty());
        assert_eq!(t.blocked_by, ["B", "C"]);
        assert_eq!(t.priority.as_deref(), Some("high"));
    }

    #[test]
    fn stale_temp_file_from_a_crash_does_not_affect_the_original() {
        let (dir, path, hash) = file(SRC);
        fs::write(dir.path().join(".a.md.deadbeef.1.tmp"), "partial").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), SRC);
        patch_file(&path, &hash, &Patch::new().status("done"), NOW).unwrap();
        let store = crate::store::Store::scan(&crate::config::Config::default_for(dir.path()));
        assert_eq!(store.ids().len(), 1);
        assert!(store.problems().is_empty());
    }

    #[test]
    fn missing_file_is_an_io_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = patch_file(&dir.path().join("none.md"), "x", &Patch::new(), NOW).unwrap_err();
        assert!(matches!(err, WriteError::Io { .. }));
    }

    #[cfg(unix)]
    #[test]
    fn keeps_file_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let (_dir, path, hash) = file(SRC);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        patch_file(&path, &hash, &Patch::new().status("done"), NOW).unwrap();
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o640);
    }
}
