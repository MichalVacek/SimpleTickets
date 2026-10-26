//! The tools exposed to MCP clients and their mapping onto the HTTP API.

use crate::client::{encode, Backend};
use serde_json::{json, Map, Value};
use std::cmp::Ordering;

const FILTER_KEYS: [&str; 8] = [
    "status", "category", "priority", "assignee", "tag", "parent", "title", "due_by",
];
const DEFAULT_ORDER: [&str; 2] = ["due", "priority"];

/// Definitions returned by `tools/list`.
pub fn definitions() -> Value {
    let filters = json!({
        "status": {"type": "string", "description": "Status name, for example todo"},
        "category": {"type": "string", "enum": ["todo", "in-progress", "done", "cancelled"]},
        "priority": {"type": "string"},
        "assignee": {"type": "string", "description": "Case-insensitive assignee name"},
        "tag": {"type": "string"},
        "parent": {"type": "string", "description": "ID of the parent ticket"},
        "title": {"type": "string", "description": "Case-insensitive substring of the title"},
        "due_by": {"type": "string", "description": "YYYY-MM-DD or today; tickets due on or before it"},
        "fields": {"type": "object", "description": "Exact matches on custom frontmatter keys", "additionalProperties": {"type": "string"}}
    });
    let mut list_props = filters.as_object().cloned().unwrap_or_default();
    list_props.insert(
        "blocked".into(),
        json!({"type": "boolean", "description": "Only blocked or only unblocked tickets"}),
    );
    list_props.insert(
        "limit".into(),
        json!({"type": "integer", "minimum": 1, "description": "Maximum number of tickets"}),
    );
    let mut next_props = filters.as_object().cloned().unwrap_or_default();
    next_props.remove("status");
    next_props.remove("category");
    next_props.insert(
        "order".into(),
        json!({
            "type": "array",
            "items": {"type": "string", "enum": ["due", "priority", "id"]},
            "description": "Sort keys, most significant first. Default: due, priority. Tickets without a due date sort last."
        }),
    );
    let mut search_props = filters.as_object().cloned().unwrap_or_default();
    search_props.remove("title");
    search_props.remove("due_by");
    search_props.remove("fields");
    search_props.insert(
        "query".into(),
        json!({"type": "string", "description": "Words and \"quoted phrases\""}),
    );
    search_props.insert(
        "limit".into(),
        json!({"type": "integer", "minimum": 1, "description": "Maximum number of results"}),
    );
    let ticket_props = json!({
        "title": {"type": "string"},
        "status": {"type": "string"},
        "priority": {"type": ["string", "null"]},
        "assignee": {"type": ["string", "null"], "description": "Who owns the ticket; null clears it on update"},
        "due": {"type": ["string", "null"], "description": "YYYY-MM-DD"},
        "tags": {"type": "array", "items": {"type": "string"}},
        "parent": {"type": ["string", "null"]},
        "blocked_by": {"type": "array", "items": {"type": "string"}},
        "related": {"type": "array", "items": {"type": "string"}}
    });
    let mut create_props = ticket_props.as_object().cloned().unwrap_or_default();
    create_props.insert(
        "body".into(),
        json!({"type": "string", "description": "Markdown body, replacing the template"}),
    );
    create_props.insert("template".into(), json!({"type": "string"}));
    create_props.insert(
        "role".into(),
        json!({"type": "string", "enum": ["parent", "sub"]}),
    );
    let mut update_props = ticket_props.as_object().cloned().unwrap_or_default();
    update_props.insert("id".into(), json!({"type": "string"}));
    json!([
        {
            "name": "list_tickets",
            "description": "List tickets, optionally filtered. All given filters must match.",
            "inputSchema": {"type": "object", "properties": list_props}
        },
        {
            "name": "search_tickets",
            "description": "Full-text search over ticket titles and bodies. Case-insensitive; every word must appear as a whole word, and \"quoted phrases\" must appear in order. Best matches first, each with a score and a snippet. The optional filters narrow the results.",
            "inputSchema": {"type": "object", "properties": search_props, "required": ["query"]}
        },
        {
            "name": "show_ticket",
            "description": "Show one ticket with its body, links and derived state.",
            "inputSchema": {"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"]}
        },
        {
            "name": "create_ticket",
            "description": "Create a ticket and return it.",
            "inputSchema": {"type": "object", "properties": create_props, "required": ["title"]}
        },
        {
            "name": "update_ticket",
            "description": "Change fields of a ticket. Only the given fields change; null clears priority, assignee, due and parent.",
            "inputSchema": {"type": "object", "properties": update_props, "required": ["id"]}
        },
        {
            "name": "next_ticket",
            "description": "The ticket to work on next: unblocked and todo, matching the optional filters, ordered by due date then priority unless `order` says otherwise. Returns the first one.",
            "inputSchema": {"type": "object", "properties": next_props}
        }
    ])
}

/// Runs a tool; `Err` carries a message for the client.
pub fn call(backend: &dyn Backend, name: &str, args: &Value) -> Result<Value, String> {
    let empty = Map::new();
    let args = match args {
        Value::Null => &empty,
        Value::Object(o) => o,
        _ => return Err("arguments must be an object".into()),
    };
    match name {
        "list_tickets" => {
            let mut pairs = filter_pairs(args)?;
            for key in ["blocked", "limit"] {
                if let Some(v) = args.get(key) {
                    pairs.push((key.into(), scalar(key, v)?));
                }
            }
            backend.request("GET", &format!("/v1/tickets{}", query_string(&pairs)), None)
        }
        "search_tickets" => {
            let query = match args.get("query").and_then(Value::as_str) {
                Some(q) if !q.trim().is_empty() => q,
                _ => return Err("`query` is required".into()),
            };
            let mut pairs = vec![("q".to_string(), query.to_string())];
            pairs.extend(filter_pairs(args)?);
            if let Some(v) = args.get("limit").filter(|v| !v.is_null()) {
                pairs.push(("limit".into(), scalar("limit", v)?));
            }
            backend.request("GET", &format!("/v1/search{}", query_string(&pairs)), None)
        }
        "show_ticket" => backend.request("GET", &format!("/v1/tickets/{}", ticket_id(args)?), None),
        "create_ticket" => {
            if !args.contains_key("title") {
                return Err("`title` is required".into());
            }
            backend.request("POST", "/v1/tickets", Some(&Value::Object(args.clone())))
        }
        "update_ticket" => {
            let id = ticket_id(args)?;
            let mut changes = args.clone();
            changes.remove("id");
            backend.request(
                "PATCH",
                &format!("/v1/tickets/{id}"),
                Some(&Value::Object(changes)),
            )
        }
        "next_ticket" => next_ticket(backend, args),
        other => Err(format!("unknown tool `{other}`")),
    }
}

fn ticket_id(args: &Map<String, Value>) -> Result<String, String> {
    match args.get("id").and_then(Value::as_str) {
        Some(id) if !id.is_empty() => Ok(encode(id)),
        _ => Err("`id` is required".into()),
    }
}

fn scalar(key: &str, value: &Value) -> Result<String, String> {
    match value {
        Value::String(s) => Ok(s.clone()),
        Value::Bool(b) => Ok(b.to_string()),
        Value::Number(n) => Ok(n.to_string()),
        _ => Err(format!("`{key}` must be a string, number or boolean")),
    }
}

fn filter_pairs(args: &Map<String, Value>) -> Result<Vec<(String, String)>, String> {
    let mut pairs = Vec::new();
    for key in FILTER_KEYS {
        if let Some(v) = args.get(key).filter(|v| !v.is_null()) {
            pairs.push((key.to_string(), scalar(key, v)?));
        }
    }
    if let Some(fields) = args.get("fields").filter(|v| !v.is_null()) {
        let fields = fields.as_object().ok_or("`fields` must be an object")?;
        for (k, v) in fields {
            pairs.push((format!("field.{k}"), scalar(k, v)?));
        }
    }
    Ok(pairs)
}

fn query_string(pairs: &[(String, String)]) -> String {
    if pairs.is_empty() {
        return String::new();
    }
    let parts: Vec<String> = pairs
        .iter()
        .map(|(k, v)| format!("{}={}", encode(k), encode(v)))
        .collect();
    format!("?{}", parts.join("&"))
}

fn next_ticket(backend: &dyn Backend, args: &Map<String, Value>) -> Result<Value, String> {
    let order = sort_keys(args)?;
    let mut pairs = filter_pairs(args)?;
    pairs.retain(|(k, _)| k != "status" && k != "category");
    pairs.push(("category".into(), "todo".into()));
    pairs.push(("blocked".into(), "false".into()));
    let listed = backend.request("GET", &format!("/v1/tickets{}", query_string(&pairs)), None)?;
    let mut tickets = listed.as_array().cloned().unwrap_or_default();
    tickets.sort_by(|a, b| compare(a, b, &order));
    Ok(tickets.into_iter().next().unwrap_or(Value::Null))
}

fn sort_keys(args: &Map<String, Value>) -> Result<Vec<String>, String> {
    let Some(order) = args.get("order").filter(|v| !v.is_null()) else {
        return Ok(DEFAULT_ORDER.iter().map(ToString::to_string).collect());
    };
    let items = order.as_array().ok_or("`order` must be an array")?;
    items
        .iter()
        .map(|v| match v.as_str() {
            Some(k @ ("due" | "priority" | "id")) => Ok(k.to_string()),
            _ => Err("`order` entries must be due, priority or id".to_string()),
        })
        .collect()
}

fn compare(a: &Value, b: &Value, order: &[String]) -> Ordering {
    for key in order {
        let ord = match key.as_str() {
            "due" => due_rank(a).cmp(&due_rank(b)),
            "priority" => priority_rank(a).cmp(&priority_rank(b)),
            _ => Ordering::Equal,
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    id_rank(a).cmp(&id_rank(b))
}

/// Tickets with a due date come first, earliest date first.
fn due_rank(ticket: &Value) -> (bool, String) {
    match ticket["due"].as_str() {
        Some(d) => (false, d.to_string()),
        None => (true, String::new()),
    }
}

/// Lower is more urgent. Known names are ranked, `P0`/`P1`/`1` style labels use their
/// number, and unrecognised or missing priorities come last.
fn priority_rank(ticket: &Value) -> u32 {
    let Some(name) = ticket["priority"].as_str() else {
        return u32::MAX;
    };
    let lower = name.trim().to_lowercase();
    match lower.as_str() {
        "critical" | "urgent" | "highest" | "blocker" => 0,
        "high" => 1,
        "medium" | "normal" | "default" => 2,
        "low" => 3,
        "lowest" | "trivial" => 4,
        other => other
            .trim_start_matches('p')
            .parse::<u32>()
            .map_or(u32::MAX - 1, |n| n.min(1000)),
    }
}

/// Ticket order of creation: the trailing number of the ID, then the ID itself.
fn id_rank(ticket: &Value) -> (u64, String) {
    let id = ticket["id"].as_str().unwrap_or("");
    let digits: String = id
        .chars()
        .rev()
        .take_while(char::is_ascii_digit)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    (digits.parse().unwrap_or(u64::MAX), id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct Fake {
        reply: Value,
        seen: RefCell<Vec<(String, String, Option<Value>)>>,
    }

    impl Fake {
        fn new(reply: Value) -> Fake {
            Fake {
                reply,
                seen: RefCell::new(Vec::new()),
            }
        }
    }

    impl Backend for Fake {
        fn request(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, String> {
            self.seen
                .borrow_mut()
                .push((method.into(), path.into(), body.cloned()));
            Ok(self.reply.clone())
        }
    }

    fn t(id: &str, due: Option<&str>, priority: Option<&str>) -> Value {
        json!({"id": id, "due": due, "priority": priority})
    }

    fn next_id(tickets: Vec<Value>, args: Value) -> Value {
        let fake = Fake::new(Value::Array(tickets));
        call(&fake, "next_ticket", &args).unwrap()["id"].clone()
    }

    #[test]
    fn next_orders_by_due_then_priority() {
        let tickets = vec![
            t("ST-1", None, Some("critical")),
            t("ST-2", Some("2026-11-05"), Some("low")),
            t("ST-3", Some("2026-11-01"), Some("low")),
            t("ST-4", Some("2026-11-01"), Some("high")),
        ];
        assert_eq!(next_id(tickets.clone(), json!({})), "ST-4");
        assert_eq!(
            next_id(tickets.clone(), json!({"order": ["priority", "due"]})),
            "ST-1"
        );
        assert_eq!(next_id(tickets, json!({"order": ["id"]})), "ST-1");
    }

    #[test]
    fn next_falls_back_to_priority_then_creation_order() {
        let tickets = vec![
            t("ST-10", None, Some("low")),
            t("ST-9", None, Some("low")),
            t("ST-11", None, None),
        ];
        assert_eq!(next_id(tickets, json!({})), "ST-9");
    }

    #[test]
    fn next_returns_null_when_nothing_qualifies() {
        assert_eq!(next_id(vec![], json!({})), Value::Null);
    }

    #[test]
    fn next_always_asks_for_unblocked_todo_tickets() {
        let fake = Fake::new(json!([]));
        call(
            &fake,
            "next_ticket",
            &json!({"tag": "work", "category": "done", "blocked": true}),
        )
        .unwrap();
        let path = fake.seen.borrow()[0].1.clone();
        assert!(path.contains("tag=work"));
        assert!(path.contains("category=todo"));
        assert!(path.contains("blocked=false"));
        assert!(!path.contains("done"));
    }

    #[test]
    fn numbered_priorities_rank_by_number() {
        let tickets = vec![t("A-1", None, Some("P2")), t("A-2", None, Some("P0"))];
        assert_eq!(next_id(tickets, json!({})), "A-2");
    }

    #[test]
    fn list_builds_an_encoded_query() {
        let fake = Fake::new(json!([]));
        call(
            &fake,
            "list_tickets",
            &json!({"title": "a b", "assignee": "Ann Lee", "blocked": false, "limit": 5, "fields": {"team": "core"}}),
        )
        .unwrap();
        let path = fake.seen.borrow()[0].1.clone();
        assert!(path.starts_with("/v1/tickets?"));
        assert!(path.contains("title=a%20b"));
        assert!(path.contains("assignee=Ann%20Lee"));
        assert!(path.contains("blocked=false"));
        assert!(path.contains("limit=5"));
        assert!(path.contains("field.team=core"));
    }

    #[test]
    fn search_builds_an_encoded_query_with_filters() {
        let fake = Fake::new(json!([]));
        call(
            &fake,
            "search_tickets",
            &json!({"query": "\"quarterly report\" draft", "assignee": "ann", "limit": 3}),
        )
        .unwrap();
        let path = fake.seen.borrow()[0].1.clone();
        assert!(
            path.starts_with("/v1/search?q=%22quarterly%20report%22%20draft"),
            "{path}"
        );
        assert!(path.contains("assignee=ann"));
        assert!(path.contains("limit=3"));
        assert!(call(&fake, "search_tickets", &json!({"query": " "})).is_err());
        assert!(call(&fake, "search_tickets", &json!({})).is_err());
    }

    #[test]
    fn update_sends_changes_without_the_id() {
        let fake = Fake::new(json!({}));
        call(
            &fake,
            "update_ticket",
            &json!({"id": "ST-1", "status": "done", "due": null}),
        )
        .unwrap();
        let (method, path, body) = fake.seen.borrow()[0].clone();
        assert_eq!(
            (method.as_str(), path.as_str()),
            ("PATCH", "/v1/tickets/ST-1")
        );
        let body = body.unwrap();
        assert_eq!(body["status"], "done");
        assert!(body["due"].is_null());
        assert!(body.get("id").is_none());
    }

    #[test]
    fn rejects_bad_arguments() {
        let fake = Fake::new(json!({}));
        assert!(call(&fake, "show_ticket", &json!({})).is_err());
        assert!(call(&fake, "create_ticket", &json!({})).is_err());
        assert!(call(&fake, "update_ticket", &json!({"status": "done"})).is_err());
        assert!(call(&fake, "nope", &json!({})).is_err());
        assert!(call(&fake, "next_ticket", &json!({"order": ["colour"]})).is_err());
        assert!(call(&fake, "list_tickets", &json!([1])).is_err());
        assert!(fake.seen.borrow().is_empty());
    }

    #[test]
    fn definitions_cover_every_tool() {
        let names: Vec<String> = definitions()
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["name"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(
            names,
            [
                "list_tickets",
                "search_tickets",
                "show_ticket",
                "create_ticket",
                "update_ticket",
                "next_ticket"
            ]
        );
    }
}
