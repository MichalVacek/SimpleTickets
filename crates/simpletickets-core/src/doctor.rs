//! Consistency report over a scanned store. Reports problems; never fixes them.

use crate::state::is_within;
use crate::store::{ScanProblem, Store};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Issue {
    /// A file that could not be parsed as a ticket.
    Malformed {
        path: PathBuf,
        line: Option<usize>,
        message: String,
    },
    /// A file or folder that could not be read.
    Unreadable { path: PathBuf, message: String },
    /// Several files claim the same ID.
    DuplicateId { id: String, paths: Vec<PathBuf> },
    /// A link points at an ID that no ticket has. `tombstoned` marks an ID that
    /// existed earlier: a missing blocker of that kind keeps blocking.
    MissingTarget {
        from: String,
        field: &'static str,
        target: String,
        tombstoned: bool,
    },
    /// A ticket's status is not mapped to a category.
    UnknownStatus { id: String, status: String },
    /// A cycle in `parent` or `blocked_by` links, listed in link order.
    Cycle {
        field: &'static str,
        ids: Vec<String>,
    },
    /// A ticket in the sub-ticket folder without a parent.
    Orphan { id: String, path: PathBuf },
    /// A root that cannot be read; its `frozen` tickets keep their last known
    /// state and no rule touches them.
    RootOffline {
        path: PathBuf,
        message: String,
        frozen: usize,
    },
}

impl fmt::Display for Issue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Issue::Malformed {
                path,
                line,
                message,
            } => match line {
                Some(l) => write!(
                    f,
                    "{}:{l}: malformed frontmatter: {message}",
                    path.display()
                ),
                None => write!(f, "{}: malformed frontmatter: {message}", path.display()),
            },
            Issue::Unreadable { path, message } => {
                write!(f, "{}: unreadable: {message}", path.display())
            }
            Issue::DuplicateId { id, paths } => {
                let list: Vec<_> = paths.iter().map(|p| p.display().to_string()).collect();
                write!(f, "duplicate id {id}: {}", list.join(", "))
            }
            Issue::MissingTarget {
                from,
                field,
                target,
                tombstoned: false,
            } => write!(f, "{from}: `{field}` points to missing ticket {target}"),
            Issue::MissingTarget {
                from,
                field,
                target,
                tombstoned: true,
            } => {
                let effect = if *field == "blocked_by" {
                    "it still blocks"
                } else {
                    "the link is kept"
                };
                write!(
                    f,
                    "{from}: `{field}` points to ticket {target}, which existed before but is missing now \
                     (tombstone: {effect} until you run `forget {target}`)"
                )
            }
            Issue::UnknownStatus { id, status } => {
                write!(f, "{id}: status `{status}` is not mapped to a category")
            }
            Issue::Cycle { field, ids } => {
                write!(f, "cycle in `{field}`: {} -> {}", ids.join(" -> "), ids[0])
            }
            Issue::Orphan { id, path } => {
                write!(f, "{id}: sub-ticket without parent ({})", path.display())
            }
            Issue::RootOffline {
                path,
                message,
                frozen,
            } => write!(
                f,
                "{}: root is offline ({message}); {frozen} ticket(s) frozen at their last known state",
                path.display()
            ),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub issues: Vec<Issue>,
}

impl Report {
    pub fn is_clean(&self) -> bool {
        self.issues.is_empty()
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.issues.is_empty() {
            return writeln!(f, "no problems found");
        }
        for issue in &self.issues {
            writeln!(f, "{issue}")?;
        }
        Ok(())
    }
}

pub fn check(store: &Store) -> Report {
    let mut issues = Vec::new();

    for problem in store.problems() {
        issues.push(match problem {
            ScanProblem::Malformed {
                path,
                line,
                message,
            } => Issue::Malformed {
                path: path.clone(),
                line: *line,
                message: message.clone(),
            },
            ScanProblem::Unreadable { path, message } => Issue::Unreadable {
                path: path.clone(),
                message: message.clone(),
            },
        });
    }

    for (path, message) in store.offline_roots() {
        let frozen = store
            .tickets()
            .filter(|e| store.is_frozen(&e.ticket.id) && is_within(&e.path, path))
            .count();
        issues.push(Issue::RootOffline {
            path: path.clone(),
            message: message.clone(),
            frozen,
        });
    }

    for (id, paths) in store.duplicates() {
        let mut paths = paths.clone();
        paths.sort();
        issues.push(Issue::DuplicateId {
            id: id.clone(),
            paths,
        });
    }

    let config = store.config();
    for entry in store.tickets() {
        let t = &entry.ticket;
        let links = t
            .parent
            .iter()
            .map(|p| ("parent", p))
            .chain(t.blocked_by.iter().map(|b| ("blocked_by", b)))
            .chain(t.related.iter().map(|r| ("related", r)));
        for (field, target) in links {
            if !store.contains(target) && !store.is_duplicate(target) {
                issues.push(Issue::MissingTarget {
                    from: t.id.clone(),
                    field,
                    target: target.clone(),
                    tombstoned: store.is_tombstoned(target),
                });
            }
        }

        if config.category_of(&t.status).is_none() {
            issues.push(Issue::UnknownStatus {
                id: t.id.clone(),
                status: t.status.clone(),
            });
        }

        if t.parent.is_none() {
            if let Some(folder) = &config.subticket_folder {
                let folder = std::fs::canonicalize(folder).unwrap_or_else(|_| folder.clone());
                if entry.path.starts_with(folder) {
                    issues.push(Issue::Orphan {
                        id: t.id.clone(),
                        path: entry.path.clone(),
                    });
                }
            }
        }
    }

    let edges = |field: fn(&crate::ticket::Ticket) -> Vec<&String>| {
        store
            .tickets()
            .map(|e| {
                let targets = field(&e.ticket)
                    .into_iter()
                    .filter(|t| store.contains(t))
                    .cloned()
                    .collect::<Vec<_>>();
                (e.ticket.id.clone(), targets)
            })
            .collect::<BTreeMap<_, _>>()
    };
    for ids in find_cycles(&edges(|t| t.parent.iter().collect())) {
        issues.push(Issue::Cycle {
            field: "parent",
            ids,
        });
    }
    for ids in find_cycles(&edges(|t| t.blocked_by.iter().collect())) {
        issues.push(Issue::Cycle {
            field: "blocked_by",
            ids,
        });
    }

    issues.sort();
    Report { issues }
}

/// Finds the cycles of a directed graph. Each cycle is reported once, starting
/// at its smallest ID.
fn find_cycles(graph: &BTreeMap<String, Vec<String>>) -> Vec<Vec<String>> {
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        Active,
        Done,
    }

    fn visit(
        node: &str,
        graph: &BTreeMap<String, Vec<String>>,
        marks: &mut BTreeMap<String, Mark>,
        stack: &mut Vec<String>,
        found: &mut BTreeSet<Vec<String>>,
    ) {
        marks.insert(node.to_string(), Mark::Active);
        stack.push(node.to_string());
        for next in graph.get(node).map_or(&[][..], Vec::as_slice) {
            match marks.get(next.as_str()) {
                None => visit(next, graph, marks, stack, found),
                Some(Mark::Active) => {
                    let from = stack.iter().position(|s| s == next).expect("on stack");
                    let mut cycle = stack[from..].to_vec();
                    let min = (0..cycle.len())
                        .min_by_key(|&i| &cycle[i])
                        .expect("non-empty");
                    cycle.rotate_left(min);
                    found.insert(cycle);
                }
                Some(Mark::Done) => {}
            }
        }
        stack.pop();
        marks.insert(node.to_string(), Mark::Done);
    }

    let mut marks = BTreeMap::new();
    let mut found = BTreeSet::new();
    for node in graph.keys() {
        if !marks.contains_key(node) {
            visit(node, graph, &mut marks, &mut Vec::new(), &mut found);
        }
    }
    found.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::store::testutil::*;
    use std::path::Path;

    fn report(d: &Path) -> Report {
        check(&Store::scan(&Config::default_for(d)))
    }

    #[test]
    fn clean_store() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "p.md", &ticket("P", "todo", ""));
        write(
            d.path(),
            "c.md",
            &ticket("C", "todo", "parent: P\nblocked_by: [P]\nrelated: [P]\n"),
        );
        let r = report(d.path());
        assert!(r.is_clean(), "{r}");
        assert_eq!(r.to_string(), "no problems found\n");
    }

    #[test]
    fn reports_malformed_with_line_and_duplicates() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            "bad.md",
            "---\nid: X\ntitle: [oops\nstatus: todo\n---\n",
        );
        write(d.path(), "a1.md", &ticket("A", "todo", ""));
        write(d.path(), "a2.md", &ticket("A", "todo", ""));
        let r = report(d.path());
        assert!(r
            .issues
            .iter()
            .any(|i| matches!(i, Issue::Malformed { line: Some(_), .. })));
        assert!(r.issues.iter().any(
            |i| matches!(i, Issue::DuplicateId { id, paths } if id == "A" && paths.len() == 2)
        ));
        assert!(r.to_string().contains("bad.md:"));
    }

    #[test]
    fn reports_missing_targets_and_unknown_status() {
        let d = tempfile::tempdir().unwrap();
        write(
            d.path(),
            "a.md",
            &ticket(
                "A",
                "weird",
                "parent: NOPE\nblocked_by: [GONE]\nrelated: [ALSO]\n",
            ),
        );
        let r = report(d.path());
        let missing: Vec<_> = r
            .issues
            .iter()
            .filter_map(|i| match i {
                Issue::MissingTarget { field, target, .. } => Some((*field, target.as_str())),
                _ => None,
            })
            .collect();
        assert_eq!(missing.len(), 3);
        assert!(missing.contains(&("parent", "NOPE")));
        assert!(r
            .issues
            .iter()
            .any(|i| matches!(i, Issue::UnknownStatus { .. })));
    }

    #[test]
    fn missing_links_to_known_ids_are_reported_as_tombstones() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "b.md", &ticket("B", "done", ""));
        write(d.path(), "a.md", &ticket("A", "todo", "blocked_by: [B]\n"));
        Store::scan(&Config::default_for(d.path()))
            .record_known()
            .unwrap();
        std::fs::remove_file(d.path().join("b.md")).unwrap();
        let r = report(d.path());
        assert!(r.issues.iter().any(|i| matches!(
            i,
            Issue::MissingTarget { target, tombstoned: true, .. } if target == "B"
        )));
        let text = r.to_string();
        assert!(
            text.contains("tombstone") && text.contains("forget B"),
            "{text}"
        );
    }

    #[test]
    fn links_to_duplicate_ids_are_not_reported_missing() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "d1.md", &ticket("D", "todo", ""));
        write(d.path(), "d2.md", &ticket("D", "todo", ""));
        write(d.path(), "a.md", &ticket("A", "todo", "blocked_by: [D]\n"));
        let r = report(d.path());
        assert!(!r
            .issues
            .iter()
            .any(|i| matches!(i, Issue::MissingTarget { .. })));
    }

    #[test]
    fn detects_cycles_once() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "a.md", &ticket("A", "todo", "blocked_by: [B]\n"));
        write(d.path(), "b.md", &ticket("B", "todo", "blocked_by: [C]\n"));
        write(d.path(), "c.md", &ticket("C", "todo", "blocked_by: [A]\n"));
        write(
            d.path(),
            "s.md",
            &ticket("S", "todo", "blocked_by: [S]\nparent: S\n"),
        );
        let r = report(d.path());
        let cycles: Vec<_> = r
            .issues
            .iter()
            .filter_map(|i| match i {
                Issue::Cycle { field, ids } => Some((*field, ids.clone())),
                _ => None,
            })
            .collect();
        assert_eq!(cycles.len(), 3);
        assert!(cycles.contains(&("blocked_by", vec!["A".into(), "B".into(), "C".into()])));
        assert!(cycles.contains(&("blocked_by", vec!["S".into()])));
        assert!(cycles.contains(&("parent", vec!["S".into()])));
    }

    #[test]
    fn reports_offline_roots_with_frozen_count() {
        let main = tempfile::tempdir().unwrap();
        let ext = tempfile::tempdir().unwrap();
        let ext_path = ext.path().join("vault");
        write(&ext_path, "x.md", &ticket("X", "todo", ""));
        let text = format!(
            "roots:\n  - path: {}\n  - path: {}\n",
            main.path().display(),
            ext_path.display()
        );
        let config = Config::parse(&text, main.path()).unwrap();
        let before = Store::scan(&config);
        assert!(check(&before).is_clean());
        std::fs::rename(&ext_path, ext.path().join("away")).unwrap();
        let r = check(&Store::scan_after(&config, Some(&before)));
        assert!(r
            .issues
            .iter()
            .any(|i| matches!(i, Issue::RootOffline { path, frozen: 1, .. } if *path == ext_path)));
        assert!(r.to_string().contains("offline"));
    }

    #[test]
    fn detects_orphans_in_subticket_folder() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "work/o.md", &ticket("O", "todo", ""));
        write(d.path(), "work/k.md", &ticket("K", "todo", "parent: P\n"));
        write(d.path(), "specs/p.md", &ticket("P", "todo", ""));
        let mut config = Config::default_for(d.path());
        config.subticket_folder = Some(d.path().join("work"));
        config.parent_folder = Some(d.path().join("specs"));
        let r = check(&Store::scan(&config));
        let orphans: Vec<_> = r
            .issues
            .iter()
            .filter(|i| matches!(i, Issue::Orphan { .. }))
            .collect();
        assert_eq!(orphans.len(), 1);
        assert!(matches!(orphans[0], Issue::Orphan { id, .. } if id == "O"));
    }
}
