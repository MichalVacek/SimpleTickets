//! Migration after a configuration change: a dry-run plan, then a journaled,
//! resumable apply.
//!
//! A migration relocates tickets whose folders changed in the configuration
//! (`parent_folder`, `subticket_folder`) and rewrites statuses that the new
//! status mapping no longer knows. Ticket IDs are never touched and links are
//! IDs, so they stay valid; a check after completion confirms it.
//!
//! The journal is written before the first step and after every step. Every
//! step is idempotent, so a crash at any point is resolved by running the
//! journal again. The new config file is written last, when no step is left.

use crate::config::{self, Config};
use crate::doctor::{self, Issue};
use crate::hash::content_hash;
use crate::state::{self, is_within, journal_path, resolve};
use crate::store::Store;
use crate::ticket::Ticket;
use crate::time::now_timestamp;
use crate::write::{patch_file, Patch, WriteError, WriteOptions};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;
use thiserror::Error;

const JOURNAL_VERSION: u32 = 1;

#[derive(Debug, Error)]
pub enum MigrationError {
    #[error("a migration is already in progress; resume it first")]
    Busy,
    #[error("invalid config: {0}")]
    Config(String),
    #[error("journal {path}: {message}")]
    Journal { path: PathBuf, message: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    /// Move a ticket file; the ticket keeps its ID and content.
    Move {
        id: String,
        from: PathBuf,
        to: PathBuf,
    },
    /// Rewrite a status the new mapping no longer contains. `path` is the
    /// location after all moves.
    SetStatus {
        id: String,
        path: PathBuf,
        from: String,
        to: String,
    },
}

impl Action {
    pub fn id(&self) -> &str {
        match self {
            Action::Move { id, .. } | Action::SetStatus { id, .. } => id,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Plan {
    pub actions: Vec<Action>,
    /// Changes the plan cannot make, with the reason.
    pub conflicts: Vec<Conflict>,
}

impl Plan {
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty() && self.conflicts.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conflict {
    pub id: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StepState {
    Pending,
    Done,
    /// Final: the step no longer applies or cannot be made safely.
    Skipped,
    /// Retried on the next run, for example after a locked or read-only file.
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Step {
    pub action: Action,
    pub state: StepState,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JournalState {
    Applying,
    Complete,
}

/// Result of the check after completion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Integrity {
    pub ok: bool,
    pub missing_ids: Vec<String>,
    pub missing_targets_before: usize,
    pub missing_targets_after: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Journal {
    pub version: u32,
    pub started: String,
    pub state: JournalState,
    /// The config file content written when the migration completes.
    pub new_config: String,
    /// IDs of all tickets when the migration started.
    pub ids: Vec<String>,
    pub missing_targets_before: usize,
    pub steps: Vec<Step>,
    pub conflicts: Vec<Conflict>,
    pub integrity: Option<Integrity>,
}

impl Journal {
    pub fn is_complete(&self) -> bool {
        self.state == JournalState::Complete
    }

    fn count(&self, state: StepState) -> usize {
        self.steps.iter().filter(|s| s.state == state).count()
    }

    pub fn summary(&self) -> serde_json::Value {
        serde_json::json!({
            "state": self.state,
            "started": self.started,
            "steps": self.steps.len(),
            "done": self.count(StepState::Done),
            "skipped": self.count(StepState::Skipped),
            "failed": self.count(StepState::Failed),
            "pending": self.count(StepState::Pending),
        })
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RunOptions {
    /// Stop after this many executed steps, leaving the journal resumable.
    pub max_steps: Option<usize>,
}

/// Computes what moving from the store's config to `new` requires. Touches nothing.
///
/// Tickets inside an old `parent_folder` or `subticket_folder` move to the
/// new folder of the same kind when it changed. Tickets whose status the new
/// mapping drops get the first status of their old category.
pub fn plan(store: &Store, new: &Config) -> Plan {
    let old = store.config();
    let mut plan = Plan::default();
    let mut targets: BTreeMap<PathBuf, String> = BTreeMap::new();
    let mut final_path: BTreeMap<String, PathBuf> = BTreeMap::new();

    let folders = [
        (&old.parent_folder, &new.parent_folder),
        (&old.subticket_folder, &new.subticket_folder),
    ];
    for (from, to) in folders {
        let (Some(from), Some(to)) = (from, to) else {
            continue;
        };
        let (from, to) = (resolve(from), resolve(to));
        if from == to {
            continue;
        }
        for entry in store.tickets() {
            let id = &entry.ticket.id;
            let Ok(rel) = entry.path.strip_prefix(&from) else {
                continue;
            };
            if is_within(&entry.path, &to) && to.starts_with(&from) {
                continue;
            }
            let target = to.join(rel);
            let conflict = if target.exists() {
                Some(format!("{} already exists", target.display()))
            } else if let Some(other) = targets.get(&target) {
                Some(format!(
                    "{other} moves to the same file {}",
                    target.display()
                ))
            } else if !scanned_by(new, &target) {
                Some(format!(
                    "{} would be outside every root and auto-scan folder",
                    target.display()
                ))
            } else {
                None
            };
            match conflict {
                Some(message) => plan.conflicts.push(Conflict {
                    id: id.clone(),
                    message,
                }),
                None => {
                    targets.insert(target.clone(), id.clone());
                    final_path.insert(id.clone(), target.clone());
                    plan.actions.push(Action::Move {
                        id: id.clone(),
                        from: entry.path.clone(),
                        to: target,
                    });
                }
            }
        }
    }

    for entry in store.tickets() {
        let t = &entry.ticket;
        let Some(category) = old.category_of(&t.status) else {
            continue;
        };
        if new.category_of(&t.status).is_some() {
            continue;
        }
        match new.statuses_in(category).first() {
            Some(target) => plan.actions.push(Action::SetStatus {
                id: t.id.clone(),
                path: final_path
                    .get(&t.id)
                    .cloned()
                    .unwrap_or_else(|| entry.path.clone()),
                from: t.status.clone(),
                to: (*target).to_string(),
            }),
            None => plan.conflicts.push(Conflict {
                id: t.id.clone(),
                message: format!("no status for category `{category}` in the new config"),
            }),
        }
    }
    plan
}

fn scanned_by(config: &Config, path: &Path) -> bool {
    config
        .roots
        .iter()
        .map(|r| &r.path)
        .chain(config.auto_scan.iter())
        .any(|root| is_within(path, root))
}

fn missing_targets(store: &Store) -> usize {
    doctor::check(store)
        .issues
        .iter()
        .filter(|i| matches!(i, Issue::MissingTarget { .. }))
        .count()
}

fn journal_error(dir: &Path, message: impl ToString) -> MigrationError {
    MigrationError::Journal {
        path: journal_path(dir),
        message: message.to_string(),
    }
}

/// Loads the journal, if one exists.
pub fn load(dir: &Path) -> Result<Option<Journal>, MigrationError> {
    match fs::read_to_string(journal_path(dir)) {
        Ok(text) => {
            let journal: Journal =
                serde_json::from_str(&text).map_err(|e| journal_error(dir, e))?;
            if journal.version != JOURNAL_VERSION {
                return Err(journal_error(dir, "unsupported journal version"));
            }
            Ok(Some(journal))
        }
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(journal_error(dir, e)),
    }
}

fn save(dir: &Path, journal: &Journal) -> Result<(), MigrationError> {
    let path = journal_path(dir);
    let temp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(journal).expect("journal serializes");
    let result = (|| {
        state::ensure_state_dir(dir)?;
        let mut file = fs::File::create(&temp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temp, &path)
    })();
    result.map_err(|e| {
        let _ = fs::remove_file(&temp);
        journal_error(dir, e)
    })
}

/// Plans the move to the config in `new_yaml` and writes the journal.
///
/// Fails when another migration is unfinished. Nothing else is touched.
pub fn begin(dir: &Path, store: &Store, new_yaml: &str) -> Result<Journal, MigrationError> {
    if load(dir)?.is_some_and(|j| !j.is_complete()) {
        return Err(MigrationError::Busy);
    }
    let new = Config::parse(new_yaml, dir).map_err(|e| MigrationError::Config(e.to_string()))?;
    let plan = plan(store, &new);
    let journal = Journal {
        version: JOURNAL_VERSION,
        started: now_timestamp(),
        state: JournalState::Applying,
        new_config: new_yaml.to_string(),
        ids: store.ids().into_iter().collect(),
        missing_targets_before: missing_targets(store),
        steps: plan
            .actions
            .into_iter()
            .map(|action| Step {
                action,
                state: StepState::Pending,
                note: None,
            })
            .collect(),
        conflicts: plan.conflicts,
        integrity: None,
    };
    save(dir, &journal)?;
    Ok(journal)
}

/// Resumes an unfinished migration, if there is one.
pub fn resume(dir: &Path, options: RunOptions) -> Result<Option<Journal>, MigrationError> {
    match load(dir)? {
        Some(j) if !j.is_complete() => run(dir, j, options).map(Some),
        _ => Ok(None),
    }
}

/// Executes the steps that are still open, then completes the migration if none are left.
pub fn run(
    dir: &Path,
    mut journal: Journal,
    options: RunOptions,
) -> Result<Journal, MigrationError> {
    let mut executed = 0;
    for i in 0..journal.steps.len() {
        if !matches!(
            journal.steps[i].state,
            StepState::Pending | StepState::Failed
        ) {
            continue;
        }
        if options.max_steps.is_some_and(|max| executed >= max) {
            return Ok(journal);
        }
        let (state, note) = execute(&journal.steps[i].action);
        journal.steps[i].state = state;
        journal.steps[i].note = note;
        save(dir, &journal)?;
        executed += 1;
    }
    if journal
        .steps
        .iter()
        .any(|s| matches!(s.state, StepState::Pending | StepState::Failed))
    {
        return Ok(journal);
    }
    config::write_file(dir, &journal.new_config).map_err(|e| journal_error(dir, e))?;
    let new = Config::parse(&journal.new_config, dir)
        .map_err(|e| MigrationError::Config(e.to_string()))?;
    journal.integrity = Some(check_integrity(&journal, &Store::scan(&new)));
    journal.state = JournalState::Complete;
    save(dir, &journal)?;
    Ok(journal)
}

fn check_integrity(journal: &Journal, after: &Store) -> Integrity {
    let present: BTreeSet<String> = after.ids();
    let missing_ids: Vec<String> = journal
        .ids
        .iter()
        .filter(|id| !present.contains(*id))
        .cloned()
        .collect();
    let missing_after = missing_targets(after);
    Integrity {
        ok: missing_ids.is_empty() && missing_after <= journal.missing_targets_before,
        missing_ids,
        missing_targets_before: journal.missing_targets_before,
        missing_targets_after: missing_after,
    }
}

fn skipped(note: impl ToString) -> (StepState, Option<String>) {
    (StepState::Skipped, Some(note.to_string()))
}

fn failed(note: impl ToString) -> (StepState, Option<String>) {
    (StepState::Failed, Some(note.to_string()))
}

fn done() -> (StepState, Option<String>) {
    (StepState::Done, None)
}

fn ticket_id_at(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    Ticket::parse(&text).ok().map(|t| t.id)
}

fn execute(action: &Action) -> (StepState, Option<String>) {
    match action {
        Action::Move { id, from, to } => execute_move(id, from, to),
        Action::SetStatus { id, path, from, to } => execute_set_status(id, path, from, to),
    }
}

fn execute_move(id: &str, from: &Path, to: &Path) -> (StepState, Option<String>) {
    match (from.exists(), to.exists()) {
        (false, true) => {
            if ticket_id_at(to).as_deref() == Some(id) {
                done()
            } else {
                skipped(format!("{} exists but is not ticket {id}", to.display()))
            }
        }
        (false, false) => skipped("source file no longer exists"),
        (true, true) => {
            // A copy finished but the source was not removed yet.
            let same = fs::read(from)
                .ok()
                .zip(fs::read(to).ok())
                .is_some_and(|(a, b)| content_hash(&a) == content_hash(&b));
            if same && ticket_id_at(to).as_deref() == Some(id) {
                match fs::remove_file(from) {
                    Ok(()) => done(),
                    Err(e) => failed(format!("cannot remove {}: {e}", from.display())),
                }
            } else {
                skipped(format!("{} already exists", to.display()))
            }
        }
        (true, false) => {
            if ticket_id_at(from).as_deref() != Some(id) {
                return skipped("file no longer holds this ticket");
            }
            match move_file(from, to) {
                Ok(()) => done(),
                Err(e) => failed(format!("cannot move to {}: {e}", to.display())),
            }
        }
    }
}

/// Renames the file, falling back to copy and remove across file systems.
fn move_file(from: &Path, to: &Path) -> io::Result<()> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    if fs::rename(from, to).is_ok() {
        return Ok(());
    }
    let temp = to.with_extension("md.migrating");
    let result = fs::copy(from, &temp).and_then(|_| fs::rename(&temp, to));
    if let Err(e) = result {
        let _ = fs::remove_file(&temp);
        return Err(e);
    }
    fs::remove_file(from)
}

fn execute_set_status(id: &str, path: &Path, from: &str, to: &str) -> (StepState, Option<String>) {
    let Ok(bytes) = fs::read(path) else {
        return skipped("file no longer exists");
    };
    let Some(ticket) = String::from_utf8(bytes.clone())
        .ok()
        .and_then(|t| Ticket::parse(&t).ok())
    else {
        return skipped("file is no longer a valid ticket");
    };
    if ticket.id != id {
        return skipped("file no longer holds this ticket");
    }
    if ticket.status == to {
        return done();
    }
    if ticket.status != from {
        return skipped(format!("status is now `{}`", ticket.status));
    }
    let options = WriteOptions {
        min_age: Duration::ZERO,
    };
    match patch_file(
        path,
        &content_hash(&bytes),
        &Patch::new().status(to),
        options,
    ) {
        Ok(_) => done(),
        Err(e @ (WriteError::Conflict(_) | WriteError::Io { .. })) => failed(e),
        Err(e) => skipped(e),
    }
}

/// A one-line description of an action, for display.
pub fn describe(action: &Action) -> String {
    match action {
        Action::Move { id, from, to } => {
            format!("move {id}: {} -> {}", from.display(), to.display())
        }
        Action::SetStatus { id, from, to, .. } => format!("set {id} status: {from} -> {to}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::testutil::{ticket, write};

    const OLD: &str = "parent_folder: specs\nsubticket_folder: work\n";
    const NEW: &str = "parent_folder: plans\nsubticket_folder: tasks\n\
statuses:\n  todo: [todo, backlog]\n  in-progress: [doing]\n  done: [done]\n  cancelled: [cancelled]\n";

    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let d = dir.path();
        write(d, "simpletickets.yml", OLD);
        write(d, "specs/P-1.md", &ticket("P-1", "todo", ""));
        write(d, "specs/deep/P-2.md", &ticket("P-2", "done", ""));
        write(
            d,
            "work/a/T-1.md",
            &ticket("T-1", "in-progress", "parent: P-1\nblocked_by: [T-2]\n"),
        );
        write(
            d,
            "work/T-2.md",
            &ticket("T-2", "done", "parent: P-1\nrelated: [P-2]\n"),
        );
        write(
            d,
            "elsewhere/E-1.md",
            &ticket("E-1", "todo", "blocked_by: [T-1]\n"),
        );
        dir
    }

    fn store(dir: &Path) -> Store {
        Store::scan(&Config::load_from_dir(dir).unwrap())
    }

    /// Relative file paths with content, ignoring the state folder.
    fn snapshot(dir: &Path) -> BTreeMap<String, String> {
        fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, String>) {
            for e in fs::read_dir(dir).unwrap().flatten() {
                let p = e.path();
                if e.file_name() == state::STATE_DIR {
                    continue;
                }
                if p.is_dir() {
                    walk(base, &p, out);
                } else {
                    let rel = p.strip_prefix(base).unwrap().display().to_string();
                    out.insert(rel, fs::read_to_string(&p).unwrap());
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(dir, dir, &mut out);
        out
    }

    fn files_with_ids(dir: &Path) -> BTreeMap<String, String> {
        let s = store(dir);
        s.tickets()
            .map(|e| {
                let rel = e.path.strip_prefix(resolve(dir)).unwrap();
                (e.ticket.id.clone(), rel.display().to_string())
            })
            .collect()
    }

    #[test]
    fn plan_lists_moves_and_status_rewrites_without_touching_files() {
        let dir = project();
        let before = snapshot(dir.path());
        let new = Config::parse(NEW, dir.path()).unwrap();
        let p = plan(&store(dir.path()), &new);
        let moves: Vec<_> = p
            .actions
            .iter()
            .filter(|a| matches!(a, Action::Move { .. }))
            .collect();
        assert_eq!(moves.len(), 4);
        assert!(p.actions.iter().any(|a| matches!(a,
            Action::SetStatus { id, to, .. } if id == "T-1" && to == "doing")));
        assert!(p.conflicts.is_empty());
        assert_eq!(snapshot(dir.path()), before);
        assert!(load(dir.path()).unwrap().is_none());
    }

    #[test]
    fn plan_is_empty_when_nothing_relevant_changes() {
        let dir = project();
        let same = Config::parse(OLD, dir.path()).unwrap();
        assert!(plan(&store(dir.path()), &same).is_empty());
        let prefix = Config::parse(&format!("{OLD}id_prefix: XX\n"), dir.path()).unwrap();
        assert!(plan(&store(dir.path()), &prefix).is_empty());
    }

    #[test]
    fn plan_reports_conflicts() {
        let dir = project();
        write(dir.path(), "plans/P-1.md", "taken");
        let new = Config::parse(NEW, dir.path()).unwrap();
        let p = plan(&store(dir.path()), &new);
        assert!(p
            .conflicts
            .iter()
            .any(|c| c.id == "P-1" && c.message.contains("already exists")));
        let outside = tempfile::tempdir().unwrap();
        let text = format!(
            "parent_folder: specs\nsubticket_folder: {}\n",
            outside.path().display()
        );
        let new = Config::parse(&text, dir.path()).unwrap();
        let p = plan(&store(dir.path()), &new);
        assert!(p
            .conflicts
            .iter()
            .any(|c| c.message.contains("outside every root")));
    }

    #[test]
    fn apply_moves_files_keeps_ids_and_links_and_writes_config_last() {
        let dir = project();
        let d = dir.path();
        let ids_before: BTreeSet<_> = files_with_ids(d).keys().cloned().collect();
        let journal = begin(d, &store(d), NEW).unwrap();
        assert_eq!(journal.state, JournalState::Applying);
        assert_eq!(
            fs::read_to_string(d.join("simpletickets.yml")).unwrap(),
            OLD
        );
        let done = run(d, journal, RunOptions::default()).unwrap();
        assert!(done.is_complete());
        assert!(done.steps.iter().all(|s| s.state == StepState::Done));
        assert_eq!(
            fs::read_to_string(d.join("simpletickets.yml")).unwrap(),
            NEW
        );

        let after = files_with_ids(d);
        assert_eq!(after.keys().cloned().collect::<BTreeSet<_>>(), ids_before);
        assert_eq!(after["P-1"], "plans/P-1.md");
        assert_eq!(after["P-2"], "plans/deep/P-2.md");
        assert_eq!(after["T-1"], "tasks/a/T-1.md");
        assert_eq!(after["E-1"], "elsewhere/E-1.md");
        assert!(!d.join("specs/P-1.md").exists());

        let s = store(d);
        assert_eq!(s.get("T-1").unwrap().ticket.status, "doing");
        assert_eq!(s.get("T-1").unwrap().ticket.parent.as_deref(), Some("P-1"));
        assert_eq!(s.get("T-1").unwrap().ticket.blocked_by, ["T-2"]);
        assert!(doctor::check(&s).is_clean(), "{}", doctor::check(&s));
        let integrity = done.integrity.unwrap();
        assert!(integrity.ok, "{integrity:?}");
        assert_eq!(load(d).unwrap().unwrap().state, JournalState::Complete);
    }

    #[test]
    fn body_and_unknown_keys_survive_status_rewrite() {
        let dir = project();
        let d = dir.path();
        write(d, "work/T-3.md", "---\r\n# keep\r\nid: T-3\r\ntitle: X\r\nstatus: in-progress\r\nx: [1]\r\n---\r\nBody\r\n");
        let j = begin(d, &store(d), NEW).unwrap();
        run(d, j, RunOptions::default()).unwrap();
        let text = fs::read_to_string(d.join("tasks/T-3.md")).unwrap();
        assert_eq!(
            text,
            "---\r\n# keep\r\nid: T-3\r\ntitle: X\r\nstatus: doing\r\nx: [1]\r\n---\r\nBody\r\n"
        );
    }

    #[test]
    fn resume_after_interruption_at_every_step_matches_a_clean_run() {
        let reference = project();
        let rd = reference.path();
        let j = begin(rd, &store(rd), NEW).unwrap();
        let steps = j.steps.len();
        run(rd, j, RunOptions::default()).unwrap();
        let expected = snapshot(rd);
        assert!(steps >= 5);

        for stop in 0..steps {
            let dir = project();
            let d = dir.path();
            let j = begin(d, &store(d), NEW).unwrap();
            let partial = run(
                d,
                j,
                RunOptions {
                    max_steps: Some(stop),
                },
            )
            .unwrap();
            assert_eq!(partial.state, JournalState::Applying, "stop at {stop}");
            assert_eq!(
                fs::read_to_string(d.join("simpletickets.yml")).unwrap(),
                OLD
            );
            // The journal on disk is what a restarted process sees.
            let loaded = load(d).unwrap().unwrap();
            assert_eq!(loaded, partial);
            let finished = resume(d, RunOptions::default()).unwrap().unwrap();
            assert!(finished.is_complete(), "stop at {stop}");
            assert_eq!(snapshot(d), expected, "stop at {stop}");
            assert!(finished.integrity.unwrap().ok);
        }
    }

    #[test]
    fn resume_handles_a_step_done_on_disk_but_not_journaled() {
        let dir = project();
        let d = dir.path();
        let j = begin(d, &store(d), NEW).unwrap();
        let Action::Move { from, to, .. } = &j.steps[0].action else {
            panic!("first step is a move");
        };
        fs::create_dir_all(to.parent().unwrap()).unwrap();
        fs::rename(from, to).unwrap();
        let finished = resume(d, RunOptions::default()).unwrap().unwrap();
        assert!(finished.is_complete());
        assert_eq!(finished.steps[0].state, StepState::Done);
        assert!(finished.integrity.unwrap().ok);
    }

    #[test]
    fn resume_cleans_up_an_unfinished_cross_device_copy() {
        let dir = project();
        let d = dir.path();
        let j = begin(d, &store(d), NEW).unwrap();
        let Action::Move { from, to, .. } = &j.steps[0].action else {
            panic!("first step is a move");
        };
        fs::create_dir_all(to.parent().unwrap()).unwrap();
        fs::copy(from, to).unwrap();
        let finished = resume(d, RunOptions::default()).unwrap().unwrap();
        assert!(finished.is_complete());
        assert!(!from.exists() && to.exists());
    }

    #[test]
    fn edited_status_since_plan_is_skipped_not_overwritten() {
        let dir = project();
        let d = dir.path();
        let j = begin(d, &store(d), NEW).unwrap();
        let path = d.join("work/a/T-1.md");
        fs::write(
            &path,
            ticket("T-1", "cancelled", "parent: P-1\nblocked_by: [T-2]\n"),
        )
        .unwrap();
        let finished = run(d, j, RunOptions::default()).unwrap();
        assert!(finished.is_complete());
        let step = finished
            .steps
            .iter()
            .find(|s| matches!(s.action, Action::SetStatus { .. }))
            .unwrap();
        assert_eq!(step.state, StepState::Skipped);
        assert_eq!(store(d).get("T-1").unwrap().ticket.status, "cancelled");
    }

    #[test]
    fn only_one_migration_at_a_time() {
        let dir = project();
        let d = dir.path();
        let j = begin(d, &store(d), NEW).unwrap();
        run(d, j, RunOptions { max_steps: Some(1) }).unwrap();
        assert!(matches!(
            begin(d, &store(d), NEW),
            Err(MigrationError::Busy)
        ));
        resume(d, RunOptions::default()).unwrap();
        assert!(begin(d, &store(d), OLD).is_ok());
    }

    #[test]
    fn invalid_config_is_rejected_before_any_journal() {
        let dir = project();
        let d = dir.path();
        assert!(matches!(
            begin(d, &store(d), "id_prefix: a-b\n"),
            Err(MigrationError::Config(_))
        ));
        assert!(load(d).unwrap().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn failed_steps_are_retried_on_resume() {
        use std::os::unix::fs::PermissionsExt;
        let dir = project();
        let d = dir.path();
        fs::create_dir_all(d.join("plans")).unwrap();
        fs::set_permissions(d.join("plans"), fs::Permissions::from_mode(0o555)).unwrap();
        let j = begin(d, &store(d), NEW).unwrap();
        let first = run(d, j, RunOptions::default()).unwrap();
        assert_eq!(first.state, JournalState::Applying);
        assert!(first.steps.iter().any(|s| s.state == StepState::Failed));
        assert_eq!(
            fs::read_to_string(d.join("simpletickets.yml")).unwrap(),
            OLD
        );
        fs::set_permissions(d.join("plans"), fs::Permissions::from_mode(0o755)).unwrap();
        let second = resume(d, RunOptions::default()).unwrap().unwrap();
        assert!(second.is_complete());
        assert!(second.steps.iter().all(|s| s.state == StepState::Done));
        assert!(second.integrity.unwrap().ok);
    }

    #[test]
    fn corrupt_journal_is_reported() {
        let dir = project();
        let d = dir.path();
        state::ensure_state_dir(d).unwrap();
        fs::write(journal_path(d), "{ nope").unwrap();
        assert!(matches!(load(d), Err(MigrationError::Journal { .. })));
    }
}
