//! Automatic rules, written as idempotent reconcilers over the full store.
//!
//! "Blocked" is derived from open blockers and never stored, so finishing (or
//! cancelling, when configured) a blocker unblocks its dependents without any
//! file change. The rules that do write are listed in [`plan`]. Every write
//! goes through [`patch_file`], so a file edited in the meantime is skipped
//! and picked up by the next run.

use crate::audit::{AuditEntry, AuditLog};
use crate::config::{Category, LogLevel};
use crate::state::is_within;
use crate::store::Store;
use crate::time::now_timestamp;
use crate::write::{patch_file, Patch, WriteError, WriteOptions};
use serde::Serialize;
use std::path::PathBuf;

/// Upper bound on plan/apply rounds in one run; chained rules settle within it.
const MAX_PASSES: usize = 4;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Change {
    pub rule: &'static str,
    pub id: String,
    pub path: PathBuf,
    #[serde(skip)]
    pub expected_hash: String,
    pub from: String,
    pub to: String,
    pub reason: String,
    pub level: &'static str,
}

/// A condition worth reporting that no rule changes automatically.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Warning {
    pub rule: &'static str,
    pub id: String,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Plan {
    pub changes: Vec<Change>,
    pub warnings: Vec<Warning>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct RunReport {
    pub dry_run: bool,
    /// Changes applied (or, in a dry run, the changes that would be applied).
    pub changes: Vec<Change>,
    /// Changes that could not be applied, with the reason.
    pub skipped: Vec<Skipped>,
    pub warnings: Vec<Warning>,
    /// Set when the state files next to the tickets could not be updated.
    pub state_error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Skipped {
    pub id: String,
    pub reason: String,
    /// True when a later run is expected to succeed (file edited recently).
    pub retry: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RunOptions {
    pub dry_run: bool,
    pub write: WriteOptions,
}

/// Computes the changes the rules want, without touching any file.
///
/// Rule `parent-in-progress`: a parent whose sub-tickets have started is set
/// to the first configured in-progress status, also when it was done or
/// cancelled (reported as a warning). Parents inside the configured parent
/// folder are specification files; their state is derived and never written,
/// so they only produce warnings.
///
/// Warning `blocked-in-progress`: a ticket is in progress while a blocker is open.
pub fn plan(store: &Store) -> Plan {
    let config = store.config();
    let mut plan = Plan::default();
    let in_progress = config
        .statuses_in(Category::InProgress)
        .first()
        .map(|s| (*s).to_string());

    for entry in store.tickets() {
        let id = &entry.ticket.id;
        if store.is_frozen(id) {
            continue;
        }
        let own = store.own_category(id);

        if own == Some(Category::InProgress) {
            let blockers = store.open_blockers(id);
            if !blockers.is_empty() {
                plan.warnings.push(Warning {
                    rule: "blocked-in-progress",
                    id: id.clone(),
                    message: format!("in progress while blocked by {}", blockers.join(", ")),
                });
            }
        }

        if store.children_of(id).is_empty()
            || store.effective_category(id) != Some(Category::InProgress)
            || store.children_of(id).iter().any(|c| store.is_frozen(c))
        {
            continue;
        }
        let finished = matches!(own, Some(Category::Done | Category::Cancelled));
        if !matches!(own, Some(Category::Todo)) && !finished {
            continue;
        }
        let derived_only = config
            .parent_folder
            .as_ref()
            .is_some_and(|f| is_within(&entry.path, f));
        if finished {
            plan.warnings.push(Warning {
                rule: "parent-in-progress",
                id: id.clone(),
                message: format!(
                    "{} parent has a sub-ticket in progress{}",
                    entry.ticket.status,
                    if derived_only { "" } else { "; reopening it" }
                ),
            });
        }
        if derived_only {
            continue;
        }
        let Some(target) = &in_progress else { continue };
        let level = if entry
            .ticket
            .tags
            .iter()
            .any(|t| config.tag_logging.get(t) == Some(&LogLevel::Warn))
        {
            "warn"
        } else {
            "info"
        };
        plan.changes.push(Change {
            rule: "parent-in-progress",
            id: id.clone(),
            path: entry.path.clone(),
            expected_hash: entry.hash.clone(),
            from: entry.ticket.status.clone(),
            to: target.clone(),
            reason: "a sub-ticket is in progress".into(),
            level,
        });
    }
    plan
}

/// Plans and applies the rules until nothing changes, returning the final store.
///
/// In a dry run nothing is written and only the first round is planned.
pub fn reconcile(
    mut store: Store,
    audit: Option<&AuditLog>,
    options: RunOptions,
) -> (Store, RunReport) {
    let mut report = RunReport {
        dry_run: options.dry_run,
        ..RunReport::default()
    };
    for _ in 0..MAX_PASSES {
        let plan = plan(&store);
        for w in plan.warnings {
            if !report.warnings.contains(&w) {
                report.warnings.push(w);
            }
        }
        if options.dry_run {
            report.changes = plan.changes;
            return (store, report);
        }
        if plan.changes.is_empty() {
            break;
        }
        let mut applied = 0;
        for change in plan.changes {
            match apply_change(&change, audit, options.write) {
                Ok(true) => {
                    applied += 1;
                    report.changes.push(change);
                }
                Ok(false) => {}
                Err((reason, retry)) => report.skipped.push(Skipped {
                    id: change.id,
                    reason,
                    retry,
                }),
            }
        }
        if applied == 0 {
            break;
        }
        store = Store::scan_after(store.config(), Some(&store));
    }
    if let Err(e) = store.record_known() {
        report.state_error = Some(format!("known IDs: {e}"));
    }
    (store, report)
}

fn apply_change(
    change: &Change,
    audit: Option<&AuditLog>,
    write: WriteOptions,
) -> Result<bool, (String, bool)> {
    let patch = Patch::new().status(&change.to);
    let outcome = patch_file(&change.path, &change.expected_hash, &patch, write).map_err(|e| {
        let retry = matches!(e, WriteError::RecentlyModified(_) | WriteError::Conflict(_));
        (e.to_string(), retry)
    })?;
    if !outcome.changed {
        return Ok(false);
    }
    if let Some(log) = audit {
        let entry = AuditEntry {
            ts: now_timestamp(),
            rule: change.rule.into(),
            id: change.id.clone(),
            field: "status".into(),
            from: change.from.clone(),
            to: change.to.clone(),
            reason: change.reason.clone(),
            level: change.level.into(),
        };
        log.append(&entry)
            .map_err(|e| (format!("audit log: {e}"), false))?;
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::store::testutil::{ticket, write};
    use std::time::Duration;

    const NOW: RunOptions = RunOptions {
        dry_run: false,
        write: WriteOptions {
            min_age: Duration::ZERO,
        },
    };

    fn setup(files: &[(&str, &str)]) -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().unwrap();
        for (name, text) in files {
            write(dir.path(), name, text);
        }
        let config = Config::default_for(dir.path());
        (dir, config)
    }

    fn status_of(store: &Store, id: &str) -> String {
        store.get(id).unwrap().ticket.status.clone()
    }

    #[test]
    fn parent_moves_to_in_progress_when_subticket_starts() {
        let (dir, config) = setup(&[
            ("p.md", &ticket("P", "todo", "")),
            ("c.md", &ticket("C", "in-progress", "parent: P\n")),
        ]);
        let log = AuditLog::new(dir.path().join("audit.jsonl"));
        let (store, report) = reconcile(Store::scan(&config), Some(&log), NOW);
        assert_eq!(report.changes.len(), 1);
        assert_eq!(status_of(&store, "P"), "in-progress");
        let entries = log.tail(10);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["id"], "P");
        assert_eq!(entries[0]["from"], "todo");
        assert_eq!(entries[0]["rule"], "parent-in-progress");
    }

    #[test]
    fn reconcile_is_idempotent() {
        let (dir, config) = setup(&[
            ("p.md", &ticket("P", "todo", "")),
            ("c.md", &ticket("C", "in-progress", "parent: P\n")),
        ]);
        let log = AuditLog::new(dir.path().join("audit.jsonl"));
        let (store, _) = reconcile(Store::scan(&config), Some(&log), NOW);
        let (_, again) = reconcile(store, Some(&log), NOW);
        assert!(again.changes.is_empty());
        assert_eq!(log.tail(10).len(), 1);
    }

    #[test]
    fn dry_run_changes_nothing() {
        let (dir, config) = setup(&[
            ("p.md", &ticket("P", "todo", "")),
            ("c.md", &ticket("C", "in-progress", "parent: P\n")),
        ]);
        let before = std::fs::read_to_string(dir.path().join("p.md")).unwrap();
        let log = AuditLog::new(dir.path().join("audit.jsonl"));
        let options = RunOptions {
            dry_run: true,
            ..NOW
        };
        let (store, report) = reconcile(Store::scan(&config), Some(&log), options);
        assert!(report.dry_run);
        assert_eq!(report.changes.len(), 1);
        assert_eq!(report.changes[0].to, "in-progress");
        assert_eq!(status_of(&store, "P"), "todo");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("p.md")).unwrap(),
            before
        );
        assert!(log.tail(10).is_empty());
    }

    #[test]
    fn finished_parent_is_reopened_with_warning() {
        let (_dir, config) = setup(&[
            ("p.md", &ticket("P", "done", "")),
            ("c.md", &ticket("C", "in-progress", "parent: P\n")),
        ]);
        let (store, report) = reconcile(Store::scan(&config), None, NOW);
        assert_eq!(status_of(&store, "P"), "in-progress");
        assert!(report
            .warnings
            .iter()
            .any(|w| w.id == "P" && w.rule == "parent-in-progress"));
    }

    #[test]
    fn parents_in_parent_folder_are_never_written() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "specs/p.md", &ticket("P", "todo", ""));
        write(
            dir.path(),
            "work/c.md",
            &ticket("C", "in-progress", "parent: P\n"),
        );
        let config = Config::parse("parent_folder: specs\n", dir.path()).unwrap();
        let (store, report) = reconcile(Store::scan(&config), None, NOW);
        assert!(report.changes.is_empty());
        assert_eq!(status_of(&store, "P"), "todo");
        assert_eq!(store.effective_category("P"), Some(Category::InProgress));
    }

    #[test]
    fn unstarted_or_finished_children_do_not_touch_parent() {
        let (_dir, config) = setup(&[
            ("p.md", &ticket("P", "todo", "")),
            ("c1.md", &ticket("C1", "todo", "parent: P\n")),
            ("c2.md", &ticket("C2", "cancelled", "parent: P\n")),
        ]);
        let (_, report) = reconcile(Store::scan(&config), None, NOW);
        assert!(report.changes.is_empty());
    }

    #[test]
    fn frozen_tickets_and_their_parents_are_left_alone() {
        let main = tempfile::tempdir().unwrap();
        let ext = tempfile::tempdir().unwrap();
        let ext_path = ext.path().join("vault");
        write(main.path(), "p.md", &ticket("P", "todo", ""));
        write(
            &ext_path,
            "c.md",
            &ticket("C", "in-progress", "parent: P\n"),
        );
        write(&ext_path, "q.md", &ticket("Q", "todo", ""));
        write(
            main.path(),
            "d.md",
            &ticket("D", "in-progress", "parent: Q\n"),
        );
        let text = format!(
            "roots:\n  - path: {}\n  - path: {}\n",
            main.path().display(),
            ext_path.display()
        );
        let config = Config::parse(&text, main.path()).unwrap();
        let before = Store::scan(&config);
        std::fs::rename(&ext_path, ext.path().join("away")).unwrap();
        let during = Store::scan_after(&config, Some(&before));
        let plan = plan(&during);
        // P's only child C is frozen; Q is frozen itself. Neither is touched.
        assert!(plan.changes.is_empty(), "{plan:?}");
        assert!(plan.warnings.is_empty());
        let (store, report) = reconcile(during, None, NOW);
        assert!(report.changes.is_empty());
        assert_eq!(status_of(&store, "P"), "todo");
        assert_eq!(status_of(&store, "Q"), "todo");
        assert!(store.is_frozen("Q"));
    }

    #[test]
    fn done_blocker_unblocks_without_writes() {
        let (_dir, config) = setup(&[
            ("a.md", &ticket("A", "done", "")),
            ("b.md", &ticket("B", "todo", "blocked_by: [A]\n")),
        ]);
        let (store, report) = reconcile(Store::scan(&config), None, NOW);
        assert!(report.changes.is_empty());
        assert!(!store.is_blocked("B"));
    }

    #[test]
    fn cancelled_blocker_follows_configuration() {
        let files = [
            ("a.md", ticket("A", "cancelled", "")),
            ("b.md", ticket("B", "todo", "blocked_by: [A]\n")),
        ];
        let (dir, config) = setup(&[(files[0].0, &files[0].1), (files[1].0, &files[1].1)]);
        assert!(!Store::scan(&config).is_blocked("B"));
        let strict = Config::parse("cancelled_unblocks: false\n", dir.path()).unwrap();
        assert!(Store::scan(&strict).is_blocked("B"));
    }

    #[test]
    fn warns_when_in_progress_while_blocked() {
        let (_dir, config) = setup(&[
            ("a.md", &ticket("A", "todo", "")),
            ("b.md", &ticket("B", "in-progress", "blocked_by: [A]\n")),
        ]);
        let plan = plan(&Store::scan(&config));
        assert!(plan.changes.is_empty());
        assert_eq!(plan.warnings[0].rule, "blocked-in-progress");
    }

    #[test]
    fn recently_modified_file_is_skipped_for_retry() {
        let (_dir, config) = setup(&[
            ("p.md", &ticket("P", "todo", "")),
            ("c.md", &ticket("C", "in-progress", "parent: P\n")),
        ]);
        let (store, report) = reconcile(Store::scan(&config), None, RunOptions::default());
        assert!(report.changes.is_empty());
        assert_eq!(report.skipped.len(), 1);
        assert!(report.skipped[0].retry);
        assert_eq!(status_of(&store, "P"), "todo");
    }

    #[test]
    fn tag_logging_sets_audit_level() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "p.md", &ticket("P", "todo", "tags: [urgent]\n"));
        write(
            dir.path(),
            "c.md",
            &ticket("C", "in-progress", "parent: P\n"),
        );
        let config = Config::parse("tag_logging:\n  urgent: warn\n", dir.path()).unwrap();
        let log = AuditLog::new(dir.path().join("audit.jsonl"));
        reconcile(Store::scan(&config), Some(&log), NOW);
        assert_eq!(log.tail(1)[0]["level"], "warn");
    }
}
