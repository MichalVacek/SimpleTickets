//! Differences between two scans of the ticket folders, as ticket-level events.

use crate::config::Config;
use crate::store::{IndexedTicket, Store};
use serde_json::{json, Map, Value};

/// One ticket-level difference between a previous and a current index.
#[derive(Debug, Clone, PartialEq)]
pub enum TicketChange {
    Created {
        id: String,
        title: String,
        status: String,
        path: String,
    },
    Deleted {
        id: String,
        title: String,
        path: String,
    },
    Moved {
        id: String,
        title: String,
        from: String,
        to: String,
    },
    /// Frontmatter fields (`changes`, field to `{from, to}`) and/or the body changed.
    Updated {
        id: String,
        title: String,
        path: String,
        changes: Map<String, Value>,
        body_changed: bool,
    },
    StatusChanged {
        id: String,
        title: String,
        path: String,
        from: String,
        to: String,
        from_category: Option<&'static str>,
        to_category: Option<&'static str>,
    },
}

impl TicketChange {
    /// The event type this change is published as.
    pub fn kind(&self) -> &'static str {
        match self {
            TicketChange::Created { .. } => "ticket.created",
            TicketChange::Deleted { .. } => "ticket.deleted",
            TicketChange::Moved { .. } => "ticket.moved",
            TicketChange::Updated { .. } => "ticket.updated",
            TicketChange::StatusChanged { .. } => "ticket.status_changed",
        }
    }

    /// The event payload; `source` is `api`, `rule` or `scan`.
    pub fn data(&self, source: &str) -> Value {
        match self {
            TicketChange::Created {
                id,
                title,
                status,
                path,
            } => json!({"id": id, "title": title, "status": status, "path": path,
                        "source": source}),
            TicketChange::Deleted { id, title, path } => {
                json!({"id": id, "title": title, "path": path, "source": source})
            }
            TicketChange::Moved {
                id,
                title,
                from,
                to,
            } => json!({"id": id, "title": title, "path": to, "from": from, "to": to,
                        "source": source}),
            TicketChange::Updated {
                id,
                title,
                path,
                changes,
                body_changed,
            } => json!({"id": id, "title": title, "path": path, "changes": changes,
                        "body_changed": body_changed, "source": source}),
            TicketChange::StatusChanged {
                id,
                title,
                path,
                from,
                to,
                from_category,
                to_category,
            } => json!({"id": id, "title": title, "path": path, "from": from, "to": to,
                        "from_category": from_category, "to_category": to_category,
                        "source": source}),
        }
    }
}

/// The `ticket.updated` and `ticket.status_changed` changes for a status that a rule
/// rewrote from `from` to `to`.
pub fn status_rewrite(
    config: &Config,
    entry: &IndexedTicket,
    from: &str,
    to: &str,
) -> Vec<TicketChange> {
    let t = &entry.ticket;
    let path = entry.path.display().to_string();
    let mut changes = Map::new();
    changes.insert("status".into(), json!({"from": from, "to": to}));
    vec![
        TicketChange::Updated {
            id: t.id.clone(),
            title: t.title.clone(),
            path: path.clone(),
            changes,
            body_changed: false,
        },
        TicketChange::StatusChanged {
            id: t.id.clone(),
            title: t.title.clone(),
            path,
            from: from.to_string(),
            to: to.to_string(),
            from_category: config.category_of(from).map(|c| c.key()),
            to_category: config.category_of(to).map(|c| c.key()),
        },
    ]
}

fn fields(entry: &IndexedTicket) -> Map<String, Value> {
    let t = &entry.ticket;
    let mut map = Map::new();
    map.insert("title".into(), json!(t.title));
    map.insert("status".into(), json!(t.status));
    map.insert("priority".into(), json!(t.priority));
    map.insert("assignee".into(), json!(t.assignee));
    map.insert("due".into(), json!(t.due));
    map.insert("tags".into(), json!(t.tags));
    map.insert("parent".into(), json!(t.parent));
    map.insert("blocked_by".into(), json!(t.blocked_by));
    map.insert("related".into(), json!(t.related));
    if !t.extra.is_empty() {
        map.insert(
            "extra".into(),
            serde_json::to_value(&t.extra).unwrap_or(Value::Null),
        );
    }
    map
}

fn field_changes(old: &IndexedTicket, new: &IndexedTicket) -> Map<String, Value> {
    let (before, after) = (fields(old), fields(new));
    let mut keys: Vec<&String> = before.keys().chain(after.keys()).collect();
    keys.sort();
    keys.dedup();
    let mut changes = Map::new();
    for key in keys {
        let (a, b) = (before.get(key), after.get(key));
        if a != b {
            changes.insert(
                key.clone(),
                json!({"from": a.unwrap_or(&Value::Null), "to": b.unwrap_or(&Value::Null)}),
            );
        }
    }
    changes
}

/// The ticket changes that turn `previous` into `current`, ordered by ticket ID.
/// A ticket is the same ticket when its ID is; a different path is a move.
pub fn diff(previous: &Store, current: &Store) -> Vec<TicketChange> {
    let mut out = Vec::new();
    let mut ids = previous.ids();
    ids.extend(current.ids());
    for id in ids {
        match (previous.get(&id), current.get(&id)) {
            (None, Some(new)) => out.push(TicketChange::Created {
                id,
                title: new.ticket.title.clone(),
                status: new.ticket.status.clone(),
                path: new.path.display().to_string(),
            }),
            (Some(old), None) => out.push(TicketChange::Deleted {
                id,
                title: old.ticket.title.clone(),
                path: old.path.display().to_string(),
            }),
            (Some(old), Some(new)) => {
                let from = old.path.display().to_string();
                let to = new.path.display().to_string();
                if from != to {
                    out.push(TicketChange::Moved {
                        id: id.clone(),
                        title: new.ticket.title.clone(),
                        from,
                        to: to.clone(),
                    });
                }
                let changes = field_changes(old, new);
                let body_changed = old.body != new.body;
                if !changes.is_empty() || body_changed {
                    out.push(TicketChange::Updated {
                        id: id.clone(),
                        title: new.ticket.title.clone(),
                        path: to.clone(),
                        changes,
                        body_changed,
                    });
                }
                if old.ticket.status != new.ticket.status {
                    out.push(TicketChange::StatusChanged {
                        from_category: previous.own_category(&id).map(|c| c.key()),
                        to_category: current.own_category(&id).map(|c| c.key()),
                        id,
                        title: new.ticket.title.clone(),
                        path: to,
                        from: old.ticket.status.clone(),
                        to: new.ticket.status.clone(),
                    });
                }
            }
            (None, None) => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::store::testutil::{ticket, write};
    use std::path::Path;

    fn scan(dir: &Path) -> Store {
        Store::scan(&Config::default_for(dir))
    }

    fn kinds(changes: &[TicketChange]) -> Vec<&'static str> {
        changes.iter().map(TicketChange::kind).collect()
    }

    #[test]
    fn detects_create_and_delete() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "a.md", &ticket("A", "todo", ""));
        let before = scan(d.path());
        write(d.path(), "b.md", &ticket("B", "todo", ""));
        std::fs::remove_file(d.path().join("a.md")).unwrap();
        let changes = diff(&before, &scan(d.path()));
        assert_eq!(kinds(&changes), ["ticket.deleted", "ticket.created"]);
        let created = changes[1].data("scan");
        assert_eq!(created["id"], "B");
        assert_eq!(created["title"], "Ticket B");
        assert_eq!(created["source"], "scan");
        assert!(created["path"].as_str().unwrap().ends_with("b.md"));
    }

    #[test]
    fn detects_field_and_status_changes() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "a.md", &ticket("A", "todo", "priority: low\n"));
        let before = scan(d.path());
        write(
            d.path(),
            "a.md",
            &ticket("A", "done", "priority: high\ntags: [x]\n"),
        );
        let changes = diff(&before, &scan(d.path()));
        assert_eq!(kinds(&changes), ["ticket.updated", "ticket.status_changed"]);
        let updated = changes[0].data("api");
        assert_eq!(
            updated["changes"]["priority"],
            json!({"from": "low", "to": "high"})
        );
        assert_eq!(
            updated["changes"]["status"],
            json!({"from": "todo", "to": "done"})
        );
        assert_eq!(updated["changes"]["tags"]["to"], json!(["x"]));
        assert_eq!(updated["body_changed"], false);
        let status = changes[1].data("rule");
        assert_eq!(status["from"], "todo");
        assert_eq!(status["to"], "done");
        assert_eq!(status["from_category"], "todo");
        assert_eq!(status["to_category"], "done");
    }

    #[test]
    fn detects_moves_and_body_edits() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "a.md", &ticket("A", "todo", ""));
        let before = scan(d.path());
        std::fs::remove_file(d.path().join("a.md")).unwrap();
        write(
            d.path(),
            "sub/a.md",
            &ticket("A", "todo", "").replace("body", "new body"),
        );
        let changes = diff(&before, &scan(d.path()));
        assert_eq!(kinds(&changes), ["ticket.moved", "ticket.updated"]);
        let moved = changes[0].data("scan");
        assert!(moved["from"].as_str().unwrap().ends_with("a.md"));
        assert!(moved["to"].as_str().unwrap().ends_with("sub/a.md"));
        let updated = changes[1].data("scan");
        assert_eq!(updated["body_changed"], true);
        assert!(updated["changes"].as_object().unwrap().is_empty());
    }

    #[test]
    fn identical_scans_have_no_changes() {
        let d = tempfile::tempdir().unwrap();
        write(d.path(), "a.md", &ticket("A", "todo", "x: 1\n"));
        assert!(diff(&scan(d.path()), &scan(d.path())).is_empty());
    }
}
