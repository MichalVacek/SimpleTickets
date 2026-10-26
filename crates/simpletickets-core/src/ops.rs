//! Creating and updating tickets with validation, shared by the CLI and the API.

use crate::config::Category;
use crate::id::next_id;
use crate::state::is_within;
use crate::store::Store;
use crate::templates::{self, DEFAULT_TEMPLATE};
use crate::ticket::Ticket;
use crate::time::is_date;
use crate::write::{patch_file, Patch, WriteError, WriteOptions, WriteOutcome};
use serde::{Deserialize, Deserializer};
use serde_yaml_ng::Mapping;
use std::collections::HashSet;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum OpError {
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Io(String),
}

impl From<WriteError> for OpError {
    fn from(e: WriteError) -> Self {
        match e {
            WriteError::Conflict(_) | WriteError::RecentlyModified(_) => {
                OpError::Conflict(e.to_string())
            }
            WriteError::InvalidPatch(_) | WriteError::Malformed { .. } => {
                OpError::Invalid(e.to_string())
            }
            WriteError::Io { .. } => OpError::Io(e.to_string()),
        }
    }
}

/// Which configured folder a new ticket goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Parent,
    Sub,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewTicket {
    pub title: String,
    pub status: Option<String>,
    pub priority: Option<String>,
    pub due: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub parent: Option<String>,
    #[serde(default)]
    pub blocked_by: Vec<String>,
    #[serde(default)]
    pub related: Vec<String>,
    /// Template name; the body is taken from it unless `body` is given.
    pub template: Option<String>,
    pub body: Option<String>,
    /// Target folder role; defaults to the sub-ticket folder for tickets with a parent.
    pub role: Option<Role>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Created {
    pub id: String,
    pub path: PathBuf,
}

/// Changes to a ticket. Absent fields stay as they are; `null` clears
/// `priority`, `due` and `parent`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Update {
    pub title: Option<String>,
    pub status: Option<String>,
    #[serde(default, deserialize_with = "nullable")]
    pub priority: Option<Option<String>>,
    #[serde(default, deserialize_with = "nullable")]
    pub due: Option<Option<String>>,
    #[serde(default, deserialize_with = "nullable")]
    pub parent: Option<Option<String>>,
    pub tags: Option<Vec<String>>,
    pub blocked_by: Option<Vec<String>>,
    pub related: Option<Vec<String>>,
}

fn nullable<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(d).map(Some)
}

fn invalid<T>(message: impl Into<String>) -> Result<T, OpError> {
    Err(OpError::Invalid(message.into()))
}

fn check_status(store: &Store, status: &str) -> Result<(), OpError> {
    if store.config().category_of(status).is_none() {
        let known: Vec<&str> = store
            .config()
            .statuses()
            .keys()
            .map(String::as_str)
            .collect();
        return invalid(format!(
            "unknown status `{status}` (known: {})",
            known.join(", ")
        ));
    }
    Ok(())
}

fn check_due(due: &str) -> Result<(), OpError> {
    if !is_date(due) {
        return invalid(format!("`due` must be YYYY-MM-DD, got `{due}`"));
    }
    Ok(())
}

fn check_exists(store: &Store, field: &str, target: &str) -> Result<(), OpError> {
    if store.contains(target) {
        Ok(())
    } else if store.is_duplicate(target) {
        invalid(format!("`{field}` target {target} has duplicate IDs"))
    } else {
        invalid(format!("`{field}` points to missing ticket {target}"))
    }
}

/// Validates the links of ticket `id` as they would be after a change.
///
/// Targets must exist, there are two levels only, and `blocked_by` must stay acyclic.
fn check_links(
    store: &Store,
    id: &str,
    parent: Option<&str>,
    blocked_by: &[String],
    related: &[String],
) -> Result<(), OpError> {
    if let Some(p) = parent {
        if p == id {
            return invalid("a ticket cannot be its own parent");
        }
        check_exists(store, "parent", p)?;
        if store.get(p).is_some_and(|e| e.ticket.parent.is_some()) {
            return invalid(format!(
                "parent {p} is itself a sub-ticket; only two levels are allowed"
            ));
        }
        if !store.children_of(id).is_empty() {
            return invalid(format!(
                "{id} has sub-tickets and cannot become a sub-ticket"
            ));
        }
    }
    for r in related {
        if r == id {
            return invalid("a ticket cannot be related to itself");
        }
        check_exists(store, "related", r)?;
    }
    for b in blocked_by {
        if b == id {
            return invalid("a ticket cannot block itself");
        }
        check_exists(store, "blocked_by", b)?;
    }
    // A cycle exists if `id` is reachable from any of its blockers.
    let mut seen = HashSet::new();
    let mut stack: Vec<&str> = blocked_by.iter().map(String::as_str).collect();
    while let Some(current) = stack.pop() {
        if current == id {
            return invalid(format!("blocked_by would create a cycle through {id}"));
        }
        if !seen.insert(current) {
            continue;
        }
        if let Some(e) = store.get(current) {
            stack.extend(e.ticket.blocked_by.iter().map(String::as_str));
        }
    }
    Ok(())
}

fn slug(title: &str) -> String {
    let mut out = String::new();
    for c in title.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let out: String = out.chars().take(40).collect();
    out.trim_end_matches('-').to_string()
}

/// The folder a new ticket is written to.
fn target_folder(store: &Store, new: &NewTicket) -> Result<PathBuf, OpError> {
    let config = store.config();
    let role = new
        .role
        .or_else(|| new.parent.is_some().then_some(Role::Sub));
    let configured = match role {
        Some(Role::Parent) => config.parent_folder.clone(),
        Some(Role::Sub) => config.subticket_folder.clone(),
        None => None,
    };
    let folder = configured
        .or_else(|| {
            config
                .roots
                .iter()
                .find(|r| r.kind == crate::config::RootKind::Tickets)
                .map(|r| r.path.clone())
        })
        .ok_or_else(|| OpError::Invalid("no folder available for new tickets".into()))?;
    let scanned = config
        .roots
        .iter()
        .map(|r| &r.path)
        .chain(config.auto_scan.iter())
        .any(|root| is_within(&folder, root));
    if !scanned {
        return invalid(format!(
            "{} is not inside a configured root or auto-scan folder",
            folder.display()
        ));
    }
    Ok(folder)
}

/// Creates a ticket file. `dir` is the tickets folder holding templates and config.
pub fn create(store: &Store, dir: &Path, new: NewTicket) -> Result<Created, OpError> {
    let title = new.title.trim();
    if title.is_empty() {
        return invalid("title must not be empty");
    }
    let status = match &new.status {
        Some(s) => s.clone(),
        None => store
            .config()
            .statuses_in(Category::Todo)
            .first()
            .map(|s| (*s).to_string())
            .ok_or_else(|| OpError::Invalid("no todo status configured".into()))?,
    };
    check_status(store, &status)?;
    if let Some(due) = &new.due {
        check_due(due)?;
    }
    let folder = target_folder(store, &new)?;

    let mut taken: HashSet<String> = store.ids().into_iter().collect();
    taken.extend(store.duplicates().keys().cloned());
    taken.extend(store.tombstones().into_iter().map(|t| t.id));
    let template = new.template.as_deref().unwrap_or(DEFAULT_TEMPLATE);
    for _ in 0..8 {
        let id = next_id(&store.config().id_prefix, &taken, |_| false);
        check_links(
            store,
            &id,
            new.parent.as_deref(),
            &new.blocked_by,
            &new.related,
        )?;
        let body = match &new.body {
            Some(b) => b.clone(),
            None => templates::render(dir, template, title, &id).map_err(OpError::Invalid)?,
        };
        let ticket = Ticket {
            id: id.clone(),
            title: title.to_string(),
            status: status.clone(),
            priority: new.priority.clone(),
            due: new.due.clone(),
            tags: new.tags.clone(),
            parent: new.parent.clone(),
            blocked_by: new.blocked_by.clone(),
            related: new.related.clone(),
            extra: Mapping::new(),
        };
        let text = ticket
            .render_new(&body)
            .map_err(|e| OpError::Invalid(e.to_string()))?;
        let name = match slug(title) {
            s if s.is_empty() => format!("{id}.md"),
            s => format!("{id}-{s}.md"),
        };
        let path = folder.join(name);
        match write_new_file(&path, &text) {
            Ok(()) => return Ok(Created { id, path }),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                taken.insert(id);
            }
            Err(e) => return Err(OpError::Io(format!("{}: {e}", path.display()))),
        }
    }
    Err(OpError::Conflict("could not find a free ticket ID".into()))
}

/// Acknowledges a tombstone: the missing ticket `id` was deleted on purpose,
/// so links to it stop counting (a missing blocker no longer blocks).
pub fn forget_tombstone(store: &Store, id: &str) -> Result<(), OpError> {
    if store.contains(id) {
        return Err(OpError::Invalid(format!(
            "{id} is an existing ticket; only missing tickets can be forgotten"
        )));
    }
    if !store.is_tombstoned(id) {
        return Err(OpError::NotFound(format!("no tombstone for {id}")));
    }
    crate::known::forget(&store.config().dir, id)
        .map(|_| ())
        .map_err(|e| OpError::Io(e.to_string()))
}

/// Writes a new file without replacing an existing one.
fn write_new_file(path: &Path, text: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?;
    let result = file
        .write_all(text.as_bytes())
        .and_then(|()| file.sync_all());
    if result.is_err() {
        let _ = fs::remove_file(path);
    }
    result
}

/// Applies an update to the ticket file, checking the content hash the store saw.
pub fn update(
    store: &Store,
    id: &str,
    update: &Update,
    options: WriteOptions,
) -> Result<WriteOutcome, OpError> {
    let Some(entry) = store.get(id) else {
        return Err(if store.is_duplicate(id) {
            OpError::Conflict(format!("duplicate id {id}"))
        } else {
            OpError::NotFound(format!("no ticket {id}"))
        });
    };
    if store.is_frozen(id) {
        return Err(OpError::Conflict(format!(
            "{id} is frozen: its root is offline"
        )));
    }
    let t = &entry.ticket;
    let mut patch = Patch::new();
    if let Some(title) = &update.title {
        if title.trim().is_empty() {
            return invalid("title must not be empty");
        }
        patch = patch.title(title.trim());
    }
    if let Some(status) = &update.status {
        check_status(store, status)?;
        patch = patch.status(status);
    }
    if let Some(priority) = &update.priority {
        patch = patch.priority(priority.as_deref());
    }
    if let Some(due) = &update.due {
        if let Some(d) = due {
            check_due(d)?;
        }
        patch = patch.due(due.as_deref());
    }
    if let Some(tags) = &update.tags {
        patch = patch.tags(tags);
    }
    let parent = update.parent.clone().unwrap_or_else(|| t.parent.clone());
    let blocked_by = update
        .blocked_by
        .clone()
        .unwrap_or_else(|| t.blocked_by.clone());
    let related = update.related.clone().unwrap_or_else(|| t.related.clone());
    if update.parent.is_some() || update.blocked_by.is_some() || update.related.is_some() {
        check_links(store, id, parent.as_deref(), &blocked_by, &related)?;
        if update.parent.is_some() {
            patch = patch.parent(parent.as_deref());
        }
        if update.blocked_by.is_some() {
            patch = patch.blocked_by(&blocked_by);
        }
        if update.related.is_some() {
            patch = patch.related(&related);
        }
    }
    Ok(patch_file(&entry.path, &entry.hash, &patch, options)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::store::testutil::{ticket, write};
    use std::time::Duration;

    const NOW: WriteOptions = WriteOptions {
        min_age: Duration::ZERO,
    };

    fn new(title: &str) -> NewTicket {
        NewTicket {
            title: title.into(),
            ..NewTicket::default()
        }
    }

    fn setup() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.md", &ticket("ST-1", "todo", ""));
        let store = Store::scan(&Config::default_for(dir.path()));
        (dir, store)
    }

    fn msg(r: Result<impl std::fmt::Debug, OpError>) -> String {
        r.unwrap_err().to_string()
    }

    #[test]
    fn creates_with_next_id_template_and_slug() {
        let (dir, store) = setup();
        let created = create(&store, dir.path(), new("Write the report!")).unwrap();
        assert_eq!(created.id, "ST-2");
        assert!(created.path.ends_with("ST-2-write-the-report.md"));
        let text = fs::read_to_string(&created.path).unwrap();
        assert_eq!(
            text,
            "---\nid: ST-2\ntitle: Write the report!\nstatus: todo\n---\n# Write the report!\n\n"
        );
        let again = Store::scan(store.config());
        assert!(again.contains("ST-2"));
        assert_eq!(create(&again, dir.path(), new("x")).unwrap().id, "ST-3");
    }

    #[test]
    fn create_validates_input() {
        let (dir, store) = setup();
        let d = dir.path();
        assert!(msg(create(&store, d, new("  "))).contains("title"));
        let mut n = new("t");
        n.status = Some("bogus".into());
        assert!(msg(create(&store, d, n)).contains("unknown status"));
        let mut n = new("t");
        n.due = Some("soon".into());
        assert!(msg(create(&store, d, n)).contains("due"));
        let mut n = new("t");
        n.parent = Some("ST-9".into());
        assert!(msg(create(&store, d, n)).contains("missing ticket"));
        let mut n = new("t");
        n.template = Some("nope".into());
        assert!(msg(create(&store, d, n)).contains("unknown template"));
        assert_eq!(fs::read_dir(d).unwrap().count(), 1);
    }

    #[test]
    fn create_uses_configured_folders() {
        let dir = tempfile::tempdir().unwrap();
        let config =
            Config::parse("parent_folder: specs\nsubticket_folder: work\n", dir.path()).unwrap();
        let store = Store::scan(&config);
        let mut p = new("Spec");
        p.role = Some(Role::Parent);
        let parent = create(&store, dir.path(), p).unwrap();
        assert!(parent.path.starts_with(dir.path().join("specs")));
        let store = Store::scan(&config);
        let mut c = new("Child");
        c.parent = Some(parent.id.clone());
        let child = create(&store, dir.path(), c).unwrap();
        assert!(child.path.starts_with(dir.path().join("work")));
    }

    #[test]
    fn create_refuses_folder_outside_roots() {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let text = format!("parent_folder: {}\n", other.path().display());
        let config = Config::parse(&text, dir.path()).unwrap();
        let mut p = new("Spec");
        p.role = Some(Role::Parent);
        let store = Store::scan(&config);
        assert!(msg(create(&store, dir.path(), p)).contains("not inside"));
    }

    #[test]
    fn updates_fields_and_clears_with_null() {
        let (dir, store) = setup();
        let up: Update = serde_json::from_str(
            r#"{"status":"done","priority":"high","tags":["a"],"due":"2026-11-05"}"#,
        )
        .unwrap();
        assert!(update(&store, "ST-1", &up, NOW).unwrap().changed);
        let store = Store::scan(store.config());
        let t = &store.get("ST-1").unwrap().ticket;
        assert_eq!(
            (t.status.as_str(), t.priority.as_deref()),
            ("done", Some("high"))
        );
        let clear: Update = serde_json::from_str(r#"{"priority":null,"due":null}"#).unwrap();
        update(&store, "ST-1", &clear, NOW).unwrap();
        let store = Store::scan(store.config());
        let t = &store.get("ST-1").unwrap().ticket;
        assert_eq!((t.priority.clone(), t.due.clone()), (None, None));
        assert_eq!(t.status, "done");
        let _ = dir;
    }

    #[test]
    fn update_rejects_unknown_fields_and_bad_values() {
        assert!(serde_json::from_str::<Update>(r#"{"id":"X"}"#).is_err());
        let (_d, store) = setup();
        let bad = |json: &str| {
            msg(update(
                &store,
                "ST-1",
                &serde_json::from_str(json).unwrap(),
                NOW,
            ))
        };
        assert!(bad(r#"{"status":"nope"}"#).contains("unknown status"));
        assert!(bad(r#"{"title":" "}"#).contains("title"));
        assert!(bad(r#"{"parent":"ST-1"}"#).contains("own parent"));
        assert!(bad(r#"{"blocked_by":["ST-1"]}"#).contains("block itself"));
        assert!(bad(r#"{"related":["ST-7"]}"#).contains("missing"));
        let missing = update(&store, "ST-9", &Update::default(), NOW).unwrap_err();
        assert!(matches!(missing, OpError::NotFound(_)));
    }

    #[test]
    fn rejects_cycles_and_deep_hierarchies() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.md", &ticket("A", "todo", ""));
        write(
            dir.path(),
            "b.md",
            &ticket("B", "todo", "blocked_by: [A]\n"),
        );
        write(
            dir.path(),
            "c.md",
            &ticket("C", "todo", "blocked_by: [B]\nparent: A\n"),
        );
        let store = Store::scan(&Config::default_for(dir.path()));
        let bad = |id: &str, json: &str| {
            msg(update(
                &store,
                id,
                &serde_json::from_str(json).unwrap(),
                NOW,
            ))
        };
        assert!(bad("A", r#"{"blocked_by":["C"]}"#).contains("cycle"));
        assert!(bad("B", r#"{"parent":"C"}"#).contains("two levels"));
        assert!(bad("A", r#"{"parent":"B"}"#).contains("sub-tickets"));
        let ok: Update = serde_json::from_str(r#"{"blocked_by":[]}"#).unwrap();
        assert!(update(&store, "B", &ok, NOW).is_ok());
    }

    #[test]
    fn deleted_ids_are_not_reused_and_can_be_forgotten() {
        let (dir, store) = setup();
        store.record_known().unwrap();
        fs::remove_file(dir.path().join("a.md")).unwrap();
        let store = Store::scan(store.config());
        assert_eq!(create(&store, dir.path(), new("x")).unwrap().id, "ST-2");
        let store = Store::scan(store.config());
        assert!(matches!(
            forget_tombstone(&store, "ST-2"),
            Err(OpError::Invalid(_))
        ));
        assert!(matches!(
            forget_tombstone(&store, "ST-77"),
            Err(OpError::NotFound(_))
        ));
        forget_tombstone(&store, "ST-1").unwrap();
        assert!(!Store::scan(store.config()).is_tombstoned("ST-1"));
    }

    #[test]
    fn stale_store_conflicts() {
        let (dir, store) = setup();
        let path = &store.get("ST-1").unwrap().path;
        fs::write(path, ticket("ST-1", "todo", "priority: low\n")).unwrap();
        let up = Update {
            status: Some("done".into()),
            ..Update::default()
        };
        let e = update(&store, "ST-1", &up, NOW).unwrap_err();
        assert!(matches!(e, OpError::Conflict(_)), "{e}");
        let _ = dir;
    }
}
