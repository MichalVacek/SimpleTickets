//! In-memory index of all tickets under the configured roots.
//!
//! Tickets are keyed by `id`; the path is only their current location.
//! Blocked state and parent status are derived here and never stored.

use crate::config::{Category, Config, RootKind};
use crate::hash::content_hash;
use crate::ticket::{has_ticket_marker, Ticket};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct IndexedTicket {
    pub ticket: Ticket,
    pub path: PathBuf,
    /// Hash of the full file content at scan time.
    pub hash: String,
}

/// A file that could not be turned into a ticket. It is skipped, never rewritten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScanProblem {
    Malformed {
        path: PathBuf,
        line: Option<usize>,
        message: String,
    },
    Unreadable {
        path: PathBuf,
        message: String,
    },
}

#[derive(Debug)]
pub struct Store {
    config: Config,
    tickets: BTreeMap<String, IndexedTicket>,
    problems: Vec<ScanProblem>,
    /// IDs claimed by more than one file; those files are left out of the index.
    duplicates: BTreeMap<String, Vec<PathBuf>>,
    children: BTreeMap<String, Vec<String>>,
    blocks: BTreeMap<String, Vec<String>>,
    related_by: BTreeMap<String, Vec<String>>,
}

impl Store {
    pub fn scan(config: &Config) -> Store {
        let mut problems = Vec::new();
        let mut files: BTreeMap<PathBuf, RootKind> = BTreeMap::new();

        let scan_roots = config
            .roots
            .iter()
            .map(|r| (r.path.as_path(), r.kind))
            .chain(
                config
                    .auto_scan
                    .iter()
                    .map(|p| (p.as_path(), RootKind::Tickets)),
            );
        for (root, kind) in scan_roots {
            if !root.is_dir() {
                problems.push(ScanProblem::Unreadable {
                    path: root.to_path_buf(),
                    message: "root folder does not exist or is not a directory".into(),
                });
                continue;
            }
            let mut found = Vec::new();
            collect_markdown(root, &mut found, &mut problems);
            for path in found {
                let key = fs::canonicalize(&path).unwrap_or(path);
                // A file reachable from several roots is a ticket if any of them says so.
                files
                    .entry(key)
                    .and_modify(|k| {
                        if kind == RootKind::Tickets {
                            *k = RootKind::Tickets;
                        }
                    })
                    .or_insert(kind);
            }
        }

        let mut by_id: BTreeMap<String, Vec<IndexedTicket>> = BTreeMap::new();
        for (path, kind) in files {
            let bytes = match fs::read(&path) {
                Ok(b) => b,
                Err(e) => {
                    problems.push(ScanProblem::Unreadable {
                        path,
                        message: e.to_string(),
                    });
                    continue;
                }
            };
            let Ok(text) = String::from_utf8(bytes) else {
                if kind == RootKind::Tickets {
                    problems.push(ScanProblem::Unreadable {
                        path,
                        message: "file is not valid UTF-8".into(),
                    });
                }
                continue;
            };
            if kind == RootKind::Repo && !has_ticket_marker(&text) {
                continue;
            }
            match Ticket::parse(&text) {
                Ok(ticket) => by_id
                    .entry(ticket.id.clone())
                    .or_default()
                    .push(IndexedTicket {
                        hash: content_hash(text.as_bytes()),
                        ticket,
                        path,
                    }),
                Err(e) => problems.push(ScanProblem::Malformed {
                    path,
                    line: e.line,
                    message: e.message,
                }),
            }
        }

        let mut tickets = BTreeMap::new();
        let mut duplicates = BTreeMap::new();
        for (id, mut group) in by_id {
            if group.len() == 1 {
                tickets.insert(id, group.pop().expect("one element"));
            } else {
                duplicates.insert(id, group.into_iter().map(|t| t.path).collect());
            }
        }

        let mut store = Store {
            config: config.clone(),
            tickets,
            problems,
            duplicates,
            children: BTreeMap::new(),
            blocks: BTreeMap::new(),
            related_by: BTreeMap::new(),
        };
        store.index_links();
        store
    }

    fn index_links(&mut self) {
        let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut blocks: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut related_by: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for (id, entry) in &self.tickets {
            let t = &entry.ticket;
            if let Some(parent) = &t.parent {
                children.entry(parent.clone()).or_default().push(id.clone());
            }
            for blocker in &t.blocked_by {
                blocks.entry(blocker.clone()).or_default().push(id.clone());
            }
            for other in &t.related {
                related_by
                    .entry(other.clone())
                    .or_default()
                    .push(id.clone());
            }
        }
        self.children = children;
        self.blocks = blocks;
        self.related_by = related_by;
    }

    pub fn config(&self) -> &Config {
        &self.config
    }

    pub fn get(&self, id: &str) -> Option<&IndexedTicket> {
        self.tickets.get(id)
    }

    pub fn contains(&self, id: &str) -> bool {
        self.tickets.contains_key(id)
    }

    /// All indexed tickets, ordered by ID.
    pub fn tickets(&self) -> impl Iterator<Item = &IndexedTicket> {
        self.tickets.values()
    }

    pub fn ids(&self) -> BTreeSet<String> {
        self.tickets.keys().cloned().collect()
    }

    pub fn problems(&self) -> &[ScanProblem] {
        &self.problems
    }

    pub fn duplicates(&self) -> &BTreeMap<String, Vec<PathBuf>> {
        &self.duplicates
    }

    /// True if `id` is used by at least two files.
    pub fn is_duplicate(&self, id: &str) -> bool {
        self.duplicates.contains_key(id)
    }

    /// Tickets that name `id` as their parent.
    pub fn children_of(&self, id: &str) -> &[String] {
        slice(&self.children, id)
    }

    /// Tickets that list `id` in `blocked_by`.
    pub fn blocked_by_me(&self, id: &str) -> &[String] {
        slice(&self.blocks, id)
    }

    /// Tickets that list `id` in `related`.
    pub fn related_to_me(&self, id: &str) -> &[String] {
        slice(&self.related_by, id)
    }

    /// Blockers of `id` that still block it.
    pub fn open_blockers(&self, id: &str) -> Vec<&str> {
        let Some(entry) = self.tickets.get(id) else {
            return Vec::new();
        };
        entry
            .ticket
            .blocked_by
            .iter()
            .filter(|b| self.is_open_blocker(b))
            .map(String::as_str)
            .collect()
    }

    /// A ticket is blocked while any of its blockers is neither done nor
    /// (when configured) cancelled. Blockers that do not exist do not block.
    pub fn is_blocked(&self, id: &str) -> bool {
        !self.open_blockers(id).is_empty()
    }

    fn is_open_blocker(&self, blocker: &str) -> bool {
        let Some(b) = self.tickets.get(blocker) else {
            return false;
        };
        match self.config.category_of(&b.ticket.status) {
            Some(Category::Done) => false,
            Some(Category::Cancelled) => !self.config.cancelled_unblocks,
            _ => true,
        }
    }

    /// The category from the ticket's own status.
    pub fn own_category(&self, id: &str) -> Option<Category> {
        self.config
            .category_of(&self.tickets.get(id)?.ticket.status)
    }

    /// The category of a ticket with sub-tickets is derived from them; any
    /// other ticket uses its own status.
    ///
    /// - any sub-ticket in progress, or some done while others remain open: in progress
    /// - all sub-tickets finished: done if at least one is done, otherwise cancelled
    /// - otherwise: todo
    ///
    /// Sub-tickets with unmapped statuses are ignored.
    pub fn effective_category(&self, id: &str) -> Option<Category> {
        let own = self.own_category(id);
        let kids: Vec<Category> = self
            .children_of(id)
            .iter()
            .filter_map(|c| self.own_category(c))
            .collect();
        if kids.is_empty() {
            return own;
        }
        let has = |c: Category| kids.contains(&c);
        let all_finished = kids
            .iter()
            .all(|c| matches!(c, Category::Done | Category::Cancelled));
        Some(if all_finished {
            if has(Category::Done) {
                Category::Done
            } else {
                Category::Cancelled
            }
        } else if has(Category::InProgress) || has(Category::Done) {
            Category::InProgress
        } else {
            Category::Todo
        })
    }
}

fn slice<'a>(map: &'a BTreeMap<String, Vec<String>>, id: &str) -> &'a [String] {
    map.get(id).map_or(&[], Vec::as_slice)
}

const SKIPPED_DIRS: [&str; 2] = ["node_modules", "target"];

fn collect_markdown(dir: &Path, out: &mut Vec<PathBuf>, problems: &mut Vec<ScanProblem>) {
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            problems.push(ScanProblem::Unreadable {
                path: dir.to_path_buf(),
                message: e.to_string(),
            });
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_dir() {
            if !name.starts_with('.') && !SKIPPED_DIRS.contains(&name.as_ref()) {
                collect_markdown(&path, out, problems);
            }
        } else if file_type.is_file()
            && !name.starts_with('.')
            && path
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("md"))
        {
            out.push(path);
        }
    }
}

#[cfg(test)]
pub(crate) mod testutil {
    use std::path::Path;

    pub fn write(root: &Path, rel: &str, text: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    pub fn ticket(id: &str, status: &str, extra: &str) -> String {
        format!("---\nid: {id}\ntitle: Ticket {id}\nstatus: {status}\n{extra}---\nbody\n")
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;

    fn scan(dir: &Path) -> Store {
        Store::scan(&Config::default_for(dir))
    }

    #[test]
    fn indexes_tickets_by_id_regardless_of_path() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "a.md", &ticket("A", "todo", ""));
        write(d.path(), "deep/er/b.md", &ticket("B", "done", ""));
        write(d.path(), "notes.txt", "ignored");
        write(d.path(), ".hidden/c.md", &ticket("C", "todo", ""));
        let s = scan(d.path());
        assert_eq!(s.ids(), BTreeSet::from(["A".to_string(), "B".to_string()]));
        assert!(s.get("B").unwrap().path.ends_with("deep/er/b.md"));
        assert!(s.problems().is_empty());
    }

    #[test]
    fn malformed_files_are_skipped_and_reported_unchanged() {
        let d = tempfile::tempdir().unwrap();
        let bad = "---\nid: X\ntitle: [oops\nstatus: todo\n---\n";
        write(d.path(), "bad.md", bad);
        write(d.path(), "plain.md", "# no frontmatter\n");
        write(d.path(), "ok.md", &ticket("A", "todo", ""));
        let s = scan(d.path());
        assert_eq!(s.ids().len(), 1);
        assert_eq!(s.problems().len(), 2);
        assert!(s
            .problems()
            .iter()
            .any(|p| matches!(p, ScanProblem::Malformed { line: Some(_), .. })));
        assert_eq!(fs::read_to_string(d.path().join("bad.md")).unwrap(), bad);
    }

    #[test]
    fn repo_roots_require_the_marker() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "README.md", "# readme\n");
        write(d.path(), "docs/spec.md", "---\ntitle: spec\n---\n");
        write(d.path(), "docs/t.md", &ticket("A", "todo", ""));
        write(d.path(), "docs/broken.md", "---\nid: B\nstatus: [\n---\n");
        let mut config = Config::default_for(d.path());
        config.roots[0].kind = RootKind::Repo;
        let s = Store::scan(&config);
        assert_eq!(s.ids(), BTreeSet::from(["A".to_string()]));
        assert_eq!(s.problems().len(), 1);
    }

    #[test]
    fn duplicate_ids_are_excluded_and_recorded() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "one.md", &ticket("A", "todo", ""));
        write(d.path(), "two.md", &ticket("A", "done", ""));
        write(d.path(), "three.md", &ticket("B", "todo", ""));
        let s = scan(d.path());
        assert!(!s.contains("A"));
        assert!(s.is_duplicate("A"));
        assert_eq!(s.duplicates()["A"].len(), 2);
        assert!(s.contains("B"));
    }

    #[test]
    fn overlapping_roots_do_not_duplicate_files() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "sub/a.md", &ticket("A", "todo", ""));
        let mut config = Config::default_for(d.path());
        config.auto_scan = vec![d.path().join("sub")];
        let s = Store::scan(&config);
        assert_eq!(s.ids().len(), 1);
        assert!(s.duplicates().is_empty());
    }

    #[test]
    fn missing_root_is_reported() {
        let d = tempfile::tempdir().unwrap();
        let config = Config::default_for(&d.path().join("nope"));
        assert_eq!(Store::scan(&config).problems().len(), 1);
    }

    #[test]
    fn reverse_links() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "p.md", &ticket("P", "todo", ""));
        write(
            d.path(),
            "c.md",
            &ticket("C", "todo", "parent: P\nblocked_by: [B]\nrelated: [P]\n"),
        );
        write(d.path(), "b.md", &ticket("B", "todo", ""));
        let s = scan(d.path());
        assert_eq!(s.children_of("P"), ["C"]);
        assert_eq!(s.blocked_by_me("B"), ["C"]);
        assert_eq!(s.related_to_me("P"), ["C"]);
        assert!(s.children_of("C").is_empty());
    }

    #[test]
    fn blocked_state_follows_blocker_status() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            "w.md",
            &ticket("W", "todo", "blocked_by: [B1, B2, GONE]\n"),
        );
        write(d.path(), "b1.md", &ticket("B1", "done", ""));
        write(d.path(), "b2.md", &ticket("B2", "in-progress", ""));
        let s = scan(d.path());
        assert_eq!(s.open_blockers("W"), ["B2"]);
        assert!(s.is_blocked("W"));
        assert!(!s.is_blocked("B1"));
    }

    #[test]
    fn cancelled_blockers_follow_config() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "w.md", &ticket("W", "todo", "blocked_by: [B]\n"));
        write(d.path(), "b.md", &ticket("B", "cancelled", ""));
        let mut config = Config::default_for(d.path());
        assert!(!Store::scan(&config).is_blocked("W"));
        config.cancelled_unblocks = false;
        assert!(Store::scan(&config).is_blocked("W"));
    }

    fn parent_with(children: &[&str]) -> Category {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "p.md", &ticket("P", "todo", ""));
        for (i, status) in children.iter().enumerate() {
            write(
                d.path(),
                &format!("c{i}.md"),
                &ticket(&format!("C{i}"), status, "parent: P\n"),
            );
        }
        scan(d.path()).effective_category("P").unwrap()
    }

    #[test]
    fn parent_status_is_derived_from_children() {
        assert_eq!(parent_with(&["todo", "todo"]), Category::Todo);
        assert_eq!(parent_with(&["todo", "in-progress"]), Category::InProgress);
        assert_eq!(parent_with(&["todo", "done"]), Category::InProgress);
        assert_eq!(parent_with(&["done", "cancelled"]), Category::Done);
        assert_eq!(
            parent_with(&["cancelled", "cancelled"]),
            Category::Cancelled
        );
        assert_eq!(parent_with(&["done", "done"]), Category::Done);
    }

    #[test]
    fn parent_without_children_uses_own_status() {
        assert_eq!(parent_with(&[]), Category::Todo);
    }
}
