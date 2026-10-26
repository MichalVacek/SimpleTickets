//! JSON representation of tickets, shared by the CLI and the HTTP API.

use crate::doctor;
use crate::frontmatter::Document;
use crate::store::{IndexedTicket, Store};
use crate::warnings;
use serde_json::{json, Value};
use std::fs;

/// A ticket with its derived data. `detail` adds the Markdown body.
pub fn ticket_json(store: &Store, entry: &IndexedTicket, detail: bool) -> Value {
    let t = &entry.ticket;
    let category = |c: Option<crate::config::Category>| c.map(|c| c.key());
    let mut v = json!({
        "id": t.id,
        "title": t.title,
        "status": t.status,
        "category": category(store.own_category(&t.id)),
        "effective_category": category(store.effective_category(&t.id)),
        "priority": t.priority,
        "assignee": t.assignee,
        "due": t.due,
        "tags": t.tags,
        "parent": t.parent,
        "blocked_by": t.blocked_by,
        "related": t.related,
        "blocked": store.is_blocked(&t.id),
        "open_blockers": store.open_blockers(&t.id),
        "blocks": store.blocked_by_me(&t.id),
        "children": store.children_of(&t.id),
        "related_by": store.related_to_me(&t.id),
        "extra": serde_json::to_value(&t.extra).unwrap_or(Value::Null),
        "path": entry.path.display().to_string(),
        "frozen": store.is_frozen(&t.id),
        "ready_for_review": store.is_ready_for_review(&t.id),
    });
    if detail {
        let body = fs::read_to_string(&entry.path)
            .ok()
            .and_then(|text| Document::parse(text).ok())
            .map(|d| d.body().to_string());
        v["body"] = json!(body);
    }
    v
}

/// The tombstones of a store as JSON.
pub fn tombstones_json(store: &Store) -> Value {
    let items: Vec<Value> = store
        .tombstones()
        .into_iter()
        .map(|t| {
            json!({
                "id": t.id,
                "path": t.path,
                "first_seen": t.first_seen,
                "referenced_by": t.referenced_by,
            })
        })
        .collect();
    Value::Array(items)
}

/// The doctor report as JSON: `clean`, the `issues` as text, and structured detail.
pub fn doctor_json(store: &Store) -> Value {
    let report = doctor::check(store);
    let issues: Vec<String> = report.issues.iter().map(ToString::to_string).collect();
    json!({
        "clean": report.is_clean(),
        "issues": issues,
        "tombstones": tombstones_json(store),
        "warnings": warnings::load(&store.config().dir),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::store::testutil::{ticket, write};

    #[test]
    fn includes_derived_data_and_body() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "a.md", &ticket("A", "todo", ""));
        write(
            dir.path(),
            "b.md",
            &ticket("B", "todo", "blocked_by: [A]\n"),
        );
        let store = Store::scan(&Config::default_for(dir.path()));
        let a = ticket_json(&store, store.get("A").unwrap(), true);
        assert_eq!(a["blocks"][0], "B");
        assert_eq!(a["body"], "body\n");
        let b = ticket_json(&store, store.get("B").unwrap(), false);
        assert_eq!(b["blocked"], true);
        assert_eq!(b["open_blockers"][0], "A");
        assert!(b.get("body").is_none());
    }
}
