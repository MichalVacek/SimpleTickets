//! Turning a ticket into what a target receives, and remote values back into local ones.

use crate::config::{Target, Unmapped};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// Mapped field values by local field name: `title`, `status`, `priority`, `assignee`,
/// `tags`, `due`. A missing key means "do not touch"; `null` means "clear".
pub type Fields = Map<String, Value>;

fn lookup<'a>(map: &'a BTreeMap<String, String>, key: &str) -> Option<&'a String> {
    map.get(key).or_else(|| {
        map.iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(key))
            .map(|(_, v)| v)
    })
}

/// Whether the target's filter lets the ticket through.
pub fn passes_filter(target: &Target, ticket: &Value) -> bool {
    let tags: Vec<&str> = ticket["tags"]
        .as_array()
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    if !target.filter.tags.is_empty()
        && !target
            .filter
            .tags
            .iter()
            .any(|t| tags.iter().any(|have| have.eq_ignore_ascii_case(t)))
    {
        return false;
    }
    if !target.filter.categories.is_empty() {
        let category = category_of(ticket).unwrap_or_default();
        if !target.filter.categories.iter().any(|c| c == category) {
            return false;
        }
    }
    true
}

fn category_of(ticket: &Value) -> Option<&str> {
    ticket["effective_category"]
        .as_str()
        .or_else(|| ticket["category"].as_str())
}

/// The values to send to a target for this ticket.
pub fn map_fields(target: &Target, ticket: &Value) -> Fields {
    let pass = target.unmapped == Unmapped::Pass;
    let mut fields = Fields::new();
    fields.insert("title".into(), ticket["title"].clone());
    fields.insert("tags".into(), ticket["tags"].clone());
    fields.insert("due".into(), ticket["due"].clone());

    let status = ticket["status"].as_str().unwrap_or_default();
    let mapped = lookup(&target.map.status, status)
        .or_else(|| category_of(ticket).and_then(|c| lookup(&target.map.category, c)));
    match mapped {
        Some(s) => {
            fields.insert("status".into(), json!(s));
        }
        None if pass => {
            fields.insert("status".into(), json!(status));
        }
        None => {}
    }

    for (name, map) in [
        ("priority", &target.map.priority),
        ("assignee", &target.map.assignee),
    ] {
        match ticket[name].as_str() {
            Some(local) => match lookup(map, local) {
                Some(remote) => {
                    fields.insert(name.into(), json!(remote));
                }
                None if pass => {
                    fields.insert(name.into(), json!(local));
                }
                None => {}
            },
            // Clearing is only sent where the target maps the field at all.
            None if pass || !map.is_empty() => {
                fields.insert(name.into(), Value::Null);
            }
            None => {}
        }
    }
    fields
}

fn invert(
    forward: &BTreeMap<String, String>,
    explicit: &BTreeMap<String, String>,
    remote: &str,
    pass: bool,
) -> Result<Value, String> {
    if let Some(local) = lookup(explicit, remote) {
        return Ok(json!(local));
    }
    let matches: Vec<&String> = forward
        .iter()
        .filter(|(_, v)| v.eq_ignore_ascii_case(remote))
        .map(|(k, _)| k)
        .collect();
    match matches.as_slice() {
        [one] => Ok(json!(one)),
        [] if pass => Ok(json!(remote)),
        [] => Err(format!("no local value maps to remote `{remote}`")),
        _ => Err(format!(
            "remote `{remote}` is the target of several local values; name one under `pull`"
        )),
    }
}

/// The local value for a remote value of `field`, or why there is none.
pub fn from_remote(target: &Target, field: &str, remote: &Value) -> Result<Value, String> {
    let pass = target.unmapped == Unmapped::Pass;
    let text = match remote {
        Value::Null => return Ok(Value::Null),
        Value::String(s) => s.as_str(),
        _ => "",
    };
    match field {
        "status" => invert(&target.map.status, &target.pull.status, text, pass),
        "priority" => invert(&target.map.priority, &target.pull.priority, text, pass),
        "assignee" => invert(&target.map.assignee, &target.pull.assignee, text, pass),
        _ => Ok(remote.clone()),
    }
}

/// The ticket fields a connector receives. The file location is left out: it differs per
/// machine and says nothing about the ticket.
pub fn snapshot(ticket: &Value) -> Value {
    let mut out = Map::new();
    for key in [
        "id",
        "title",
        "status",
        "category",
        "effective_category",
        "priority",
        "assignee",
        "due",
        "tags",
        "parent",
        "blocked_by",
        "related",
        "extra",
        "body",
    ] {
        out.insert(key.into(), ticket[key].clone());
    }
    Value::Object(out)
}

/// Serializes with object keys sorted, so equal values always give equal text.
pub fn canonical(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .into_iter()
                .map(|k| format!("{}:{}", Value::String(k.clone()), canonical(&map[k])))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(canonical).collect();
            format!("[{}]", parts.join(","))
        }
        other => other.to_string(),
    }
}

/// A digest of everything a push would send; equal digests mean nothing to push.
pub fn payload_hash(snapshot: &Value, fields: &Fields, links: &Value) -> String {
    let payload = json!({"ticket": snapshot, "fields": fields, "links": links});
    let digest = Sha256::digest(canonical(&payload).as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn target(yaml: &str) -> Target {
        let config = Config::parse(&format!(
            "targets:\n  t:\n    connector: {{command: x}}\n{yaml}"
        ))
        .unwrap();
        config.targets["t"].clone()
    }

    fn ticket() -> Value {
        json!({"id": "ST-1", "title": "A", "status": "doing", "category": "in-progress",
               "effective_category": "in-progress", "priority": "high", "assignee": "Ann",
               "due": "2026-11-05", "tags": ["x"], "body": "b"})
    }

    #[test]
    fn status_prefers_the_name_then_the_category_and_skips_when_unmapped() {
        let t = target("    map:\n      status: {doing: Busy}\n      category: {in-progress: Active, done: Done}\n");
        assert_eq!(map_fields(&t, &ticket())["status"], "Busy");
        let mut other = ticket();
        other["status"] = json!("review");
        assert_eq!(map_fields(&t, &other)["status"], "Active");
        let none = target("");
        assert!(!map_fields(&none, &ticket()).contains_key("status"));
        let pass = target("    unmapped: pass\n");
        assert_eq!(map_fields(&pass, &ticket())["status"], "doing");
    }

    #[test]
    fn assignee_and_priority_map_case_insensitively_and_clear_only_when_mapped() {
        let t = target("    map:\n      assignee: {ann: acc-1}\n      priority: {High: P1}\n");
        let fields = map_fields(&t, &ticket());
        assert_eq!(fields["assignee"], "acc-1");
        assert_eq!(fields["priority"], "P1");
        let mut unassigned = ticket();
        unassigned["assignee"] = Value::Null;
        assert_eq!(map_fields(&t, &unassigned)["assignee"], Value::Null);
        let plain = target("");
        let fields = map_fields(&plain, &unassigned);
        assert!(!fields.contains_key("assignee"));
        assert!(!fields.contains_key("priority"));
        assert!(!map_fields(&plain, &ticket()).contains_key("assignee"));
    }

    #[test]
    fn remote_values_map_back_when_unambiguous() {
        let t = target(
            "    map:\n      status: {todo: Open, doing: Busy, review: Busy}\n      assignee: {ann: acc-1}\n    pull:\n      status: {Busy: doing}\n",
        );
        assert_eq!(from_remote(&t, "status", &json!("open")).unwrap(), "todo");
        assert_eq!(from_remote(&t, "status", &json!("Busy")).unwrap(), "doing");
        assert!(from_remote(&t, "status", &json!("Weird")).is_err());
        assert_eq!(from_remote(&t, "assignee", &json!("acc-1")).unwrap(), "ann");
        assert_eq!(
            from_remote(&t, "assignee", &Value::Null).unwrap(),
            Value::Null
        );
        let ambiguous = target("    map:\n      status: {a: X, b: X}\n");
        let e = from_remote(&ambiguous, "status", &json!("X")).unwrap_err();
        assert!(e.contains("several"), "{e}");
    }

    #[test]
    fn filter_checks_tags_and_categories() {
        let t = target("    filter: {tags: [X], categories: [in-progress]}\n");
        assert!(passes_filter(&t, &ticket()));
        let mut done = ticket();
        done["effective_category"] = json!("done");
        assert!(!passes_filter(&t, &done));
        let mut untagged = ticket();
        untagged["tags"] = json!([]);
        assert!(!passes_filter(&t, &untagged));
    }

    #[test]
    fn the_hash_ignores_key_order_and_notices_content() {
        let a = json!({"b": 1, "a": {"y": 1, "x": 2}});
        let b = json!({"a": {"x": 2, "y": 1}, "b": 1});
        assert_eq!(canonical(&a), canonical(&b));
        let fields = Fields::new();
        let h1 = payload_hash(&a, &fields, &json!({}));
        assert_eq!(h1, payload_hash(&b, &fields, &json!({})));
        assert_ne!(h1, payload_hash(&a, &fields, &json!({"parent_ref": "K-1"})));
        assert_eq!(h1.len(), 64);
    }

    #[test]
    fn the_snapshot_leaves_out_the_path() {
        let mut t = ticket();
        t["path"] = json!("/tmp/x.md");
        let s = snapshot(&t);
        assert!(s.get("path").is_none());
        assert_eq!(s["body"], "b");
    }
}
