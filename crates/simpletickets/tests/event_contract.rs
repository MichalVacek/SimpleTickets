mod common;

use common::*;
use serde_json::Value;
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

#[test]
fn every_event_has_the_envelope_and_its_documented_payload_keys() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "p.md", &ticket_text("P", "todo", ""));
    write(dir.path(), "s.md", &ticket_text("S", "todo", "parent: P\n"));
    write(dir.path(), "g.md", &ticket_text("G", "todo", ""));
    write(dir.path(), "m.md", &ticket_text("M", "todo", ""));
    let api = start(dir.path(), false, SLOW);
    // Rules leave files alone until they are a couple of seconds old.
    std::thread::sleep(Duration::from_millis(2300));
    let since = latest(&api);
    api.patch("/v1/tickets/S", r#"{"status":"in-progress"}"#);
    std::fs::remove_file(dir.path().join("g.md")).unwrap();
    std::fs::remove_file(dir.path().join("m.md")).unwrap();
    write(dir.path(), "x/m.md", &ticket_text("M", "todo", ""));
    api.post("/v1/tickets", r#"{"title":"New"}"#);

    let schema: &[(&str, &[&str])] = &[
        (
            "ticket.created",
            &["id", "title", "status", "path", "source"],
        ),
        ("ticket.deleted", &["id", "title", "path", "source"]),
        (
            "ticket.moved",
            &["id", "title", "path", "from", "to", "source"],
        ),
        (
            "ticket.updated",
            &["id", "title", "path", "changes", "body_changed", "source"],
        ),
        (
            "ticket.status_changed",
            &[
                "id",
                "title",
                "path",
                "from",
                "to",
                "from_category",
                "to_category",
                "source",
            ],
        ),
        ("scan", &["reason", "tickets", "problems"]),
        (
            "rule.applied",
            &["rule", "id", "path", "from", "to", "reason", "level"],
        ),
    ];
    let events = events_after(&api, since);
    let mut seen = Vec::new();
    for e in &events {
        let mut envelope: Vec<&str> = e.as_object().unwrap().keys().map(String::as_str).collect();
        envelope.sort_unstable();
        assert_eq!(envelope, ["data", "seq", "time", "type"], "{e}");
        let kind = e["type"].as_str().unwrap();
        if let Some((_, keys)) = schema.iter().find(|(k, _)| *k == kind) {
            let mut have: Vec<&str> = e["data"]
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            let mut want = keys.to_vec();
            if e["data"]["source"] == "api" {
                want.push("actor");
            }
            have.sort_unstable();
            want.sort_unstable();
            assert_eq!(have, want, "payload of {kind}");
            seen.push(kind);
        }
    }
    // The API write above rescans, so the hand edits surface as deleted and moved events.
    for kind in [
        "ticket.created",
        "ticket.deleted",
        "ticket.moved",
        "ticket.updated",
        "ticket.status_changed",
        "scan",
        "rule.applied",
    ] {
        assert!(seen.contains(&kind), "no {kind} event in {seen:?}");
    }
    let seqs: Vec<u64> = events.iter().map(|e| e["seq"].as_u64().unwrap()).collect();
    assert!(seqs.windows(2).all(|w| w[1] == w[0] + 1), "{seqs:?}");
}

#[test]
fn a_client_behind_the_backlog_is_told_to_resync() {
    let dir = tempfile::tempdir().unwrap();
    let api = start_with(dir.path(), false, SLOW, |o| o.event_backlog = 5);
    let (_, first) = api.get("/v1/events");
    let stream = first["stream"].as_str().unwrap().to_string();
    assert_eq!(first["gap"], false);
    let cursor = first["next"].as_u64().unwrap();
    for i in 0..4 {
        api.post("/v1/tickets", &format!(r#"{{"title":"T{i}"}}"#));
    }
    let (_, page) = api.get(&format!("/v1/events?since={cursor}&stream={stream}"));
    assert_eq!(page["gap"], true);
    assert_eq!(page["gap_reason"], "backlog_exceeded");
    let oldest = page["oldest"].as_u64().unwrap();
    assert!(oldest > cursor + 1);
    assert_eq!(page["events"].as_array().unwrap().len(), 5);
    assert_eq!(page["events"][0]["seq"], oldest);
    let next = page["next"].as_u64().unwrap();
    assert_eq!(next, page["latest"].as_u64().unwrap());

    let (_, caught_up) = api.get(&format!("/v1/events?since={next}&stream={stream}"));
    assert_eq!(caught_up["gap"], false);
    assert!(caught_up["gap_reason"].is_null());
}

#[test]
fn a_cursor_from_another_daemon_run_is_a_gap() {
    let dir = tempfile::tempdir().unwrap();
    let api = start(dir.path(), false, SLOW);
    let (_, ahead) = api.get("/v1/events?since=100000");
    assert_eq!(ahead["gap_reason"], "ahead_of_stream");
    assert_eq!(ahead["next"], ahead["latest"]);
    let (_, other) = api.get("/v1/events?since=0&stream=old-run");
    assert_eq!(other["gap_reason"], "stream_changed");
    assert!(other["events"].as_array().unwrap().is_empty());
}
