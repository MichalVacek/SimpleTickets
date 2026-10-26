mod common;

use common::*;
use serde_json::{json, Value};
use std::time::Duration;

const SLOW: Duration = Duration::from_secs(3600);

fn ticket_text(id: &str, status: &str, extra: &str) -> String {
    format!("---\nid: {id}\ntitle: Ticket {id}\nstatus: {status}\n{extra}---\nbody\n")
}

fn events_after(api: &Api, since: u64) -> Vec<Value> {
    api.get(&format!("/v1/events?since={since}")).1["events"]
        .as_array()
        .unwrap()
        .clone()
}

fn latest(api: &Api) -> u64 {
    api.get("/v1/events").1["next"].as_u64().unwrap()
}

fn of_type<'a>(events: &'a [Value], kind: &str) -> Vec<&'a Value> {
    events.iter().filter(|e| e["type"] == kind).collect()
}

#[test]
fn api_writes_publish_created_updated_and_status_changed() {
    let dir = tempfile::tempdir().unwrap();
    let api = start(dir.path(), false, SLOW);
    let since = latest(&api);
    let (_, created) = api.post(
        "/v1/tickets",
        r#"{"title":"Write report","priority":"low"}"#,
    );
    let id = created["id"].as_str().unwrap().to_string();
    let events = events_after(&api, since);
    let made = of_type(&events, "ticket.created");
    assert_eq!(made.len(), 1);
    assert_eq!(made[0]["data"]["id"], id.as_str());
    assert_eq!(made[0]["data"]["title"], "Write report");
    assert_eq!(made[0]["data"]["source"], "api");
    assert!(made[0]["data"]["path"].as_str().unwrap().ends_with(".md"));

    let since = latest(&api);
    api.patch(
        &format!("/v1/tickets/{id}"),
        r#"{"priority":"high","status":"in-progress"}"#,
    );
    let events = events_after(&api, since);
    let updated = of_type(&events, "ticket.updated");
    assert_eq!(updated.len(), 1);
    let data = &updated[0]["data"];
    assert_eq!(data["source"], "api");
    assert_eq!(
        data["changes"]["priority"],
        json!({"from": "low", "to": "high"})
    );
    let status = of_type(&events, "ticket.status_changed");
    assert_eq!(status.len(), 1);
    assert_eq!(status[0]["data"]["from"], "todo");
    assert_eq!(status[0]["data"]["to"], "in-progress");
    assert_eq!(status[0]["data"]["to_category"], "in-progress");

    let since = latest(&api);
    api.patch(&format!("/v1/tickets/{id}"), r#"{"priority":"high"}"#);
    assert!(of_type(&events_after(&api, since), "ticket.updated").is_empty());
}

#[test]
fn rule_writes_are_attributed_to_the_rule() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "p.md", &ticket_text("P", "todo", ""));
    write(dir.path(), "s.md", &ticket_text("S", "todo", "parent: P\n"));
    let api = start(dir.path(), false, SLOW);
    // Rules leave files alone until they are a couple of seconds old.
    std::thread::sleep(Duration::from_millis(2300));
    let since = latest(&api);
    api.patch("/v1/tickets/S", r#"{"status":"in-progress"}"#);
    let events = events_after(&api, since);
    let status = of_type(&events, "ticket.status_changed");
    let by_rule: Vec<&&Value> = status
        .iter()
        .filter(|e| e["data"]["source"] == "rule")
        .collect();
    assert_eq!(by_rule.len(), 1, "{events:?}");
    assert_eq!(by_rule[0]["data"]["id"], "P");
    assert_eq!(by_rule[0]["data"]["to"], "in-progress");
    assert!(!of_type(&events, "rule.applied").is_empty());
    let by_api: Vec<&&Value> = status
        .iter()
        .filter(|e| e["data"]["source"] == "api")
        .collect();
    assert_eq!(by_api[0]["data"]["id"], "S");
}

#[test]
fn hand_edits_found_by_a_scan_are_attributed_to_the_scan() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "a.md", &ticket_text("A", "todo", ""));
    write(dir.path(), "gone.md", &ticket_text("G", "todo", ""));
    write(dir.path(), "mv.md", &ticket_text("M", "todo", ""));
    let api = start(dir.path(), false, SLOW);
    let since = latest(&api);

    write(
        dir.path(),
        "a.md",
        &ticket_text("A", "done", "priority: high\n"),
    );
    write(dir.path(), "new.md", &ticket_text("N", "todo", ""));
    std::fs::remove_file(dir.path().join("gone.md")).unwrap();
    std::fs::remove_file(dir.path().join("mv.md")).unwrap();
    write(dir.path(), "moved/mv.md", &ticket_text("M", "todo", ""));
    api.post("/v1/rescan", "");

    let events = events_after(&api, since);
    for e in events
        .iter()
        .filter(|e| e["type"].as_str().unwrap().starts_with("ticket."))
    {
        assert_eq!(e["data"]["source"], "scan", "{e}");
    }
    assert_eq!(of_type(&events, "ticket.created")[0]["data"]["id"], "N");
    assert_eq!(of_type(&events, "ticket.deleted")[0]["data"]["id"], "G");
    let moved = of_type(&events, "ticket.moved");
    assert_eq!(moved.len(), 1);
    let (from, to) = (
        moved[0]["data"]["from"].as_str().unwrap(),
        moved[0]["data"]["to"].as_str().unwrap(),
    );
    assert_ne!(from, to);
    assert!(to.contains("moved"));
    let status = of_type(&events, "ticket.status_changed");
    assert_eq!(status[0]["data"]["id"], "A");
    assert_eq!(status[0]["data"]["to_category"], "done");
}

#[test]
fn the_watcher_publishes_ticket_events_for_hand_edits() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "a.md", &ticket_text("A", "todo", ""));
    let api = start(dir.path(), true, SLOW);
    let since = latest(&api);
    write(dir.path(), "a.md", &ticket_text("A", "in-progress", ""));
    assert!(wait_for(|| {
        !of_type(&events_after(&api, since), "ticket.status_changed").is_empty()
    }));
}
