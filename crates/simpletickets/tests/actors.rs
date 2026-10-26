mod common;

use common::*;
use serde_json::Value;
use std::time::Duration;

const SLOW: Duration = Duration::from_secs(3600);
const ACTOR: &str = "X-SimpleTickets-Actor";
const KEY: &str = "Idempotency-Key";

fn count(api: &Api) -> usize {
    api.get("/v1/tickets").1.as_array().unwrap().len()
}

fn events_of(api: &Api, kind: &str) -> Vec<Value> {
    api.get("/v1/events").1["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == kind)
        .cloned()
        .collect()
}

#[test]
fn writes_are_attributed_to_the_actor_in_the_audit_log_and_events() {
    let dir = tempfile::tempdir().unwrap();
    let api = start(dir.path(), false, SLOW);
    let (status, created, _) = api.call_with(
        "POST",
        "/v1/tickets",
        &[(ACTOR, "plugin:demo")],
        Some(r#"{"title":"Write report"}"#),
    );
    assert_eq!(status, 201);
    let id = created["id"].as_str().unwrap();
    let (status, _, _) = api.call_with(
        "PATCH",
        &format!("/v1/tickets/{id}"),
        &[(ACTOR, "ann@example.org")],
        Some(r#"{"status":"in-progress","priority":"high","tags":["a","b"]}"#),
    );
    assert_eq!(status, 200);
    api.post("/v1/tickets", r#"{"title":"Anonymous"}"#);

    let made = events_of(&api, "ticket.created");
    assert_eq!(made[0]["data"]["actor"], "plugin:demo");
    assert_eq!(made[0]["data"]["source"], "api");
    assert_eq!(made[1]["data"]["actor"], "api");
    let changed = events_of(&api, "ticket.status_changed");
    assert_eq!(changed[0]["data"]["actor"], "ann@example.org");

    let (_, by_plugin) = api.get("/v1/audit?actor=plugin%3Ademo");
    let by_plugin = by_plugin.as_array().unwrap();
    assert_eq!(by_plugin.len(), 1);
    assert_eq!(by_plugin[0]["rule"], "api.create");
    assert_eq!(by_plugin[0]["id"], id);
    assert_eq!(by_plugin[0]["to"], "Write report");
    assert_eq!(by_plugin[0]["actor"], "plugin:demo");

    let (_, by_ann) = api.get("/v1/audit?actor=ann%40example.org");
    let by_ann = by_ann.as_array().unwrap();
    let fields: Vec<&str> = by_ann
        .iter()
        .map(|e| e["field"].as_str().unwrap())
        .collect();
    assert_eq!(fields, ["status", "priority", "tags"]);
    assert_eq!(by_ann[0]["from"], "todo");
    assert_eq!(by_ann[0]["to"], "in-progress");
    assert_eq!(by_ann[2]["to"], "a,b");

    let (_, anonymous) = api.get("/v1/audit?actor=api");
    assert_eq!(anonymous.as_array().unwrap().len(), 1);
    let (_, one_ticket) = api.get(&format!("/v1/audit?id={id}"));
    assert_eq!(one_ticket.as_array().unwrap().len(), 4);
    let (_, last_two) = api.get(&format!("/v1/audit?id={id}&limit=2"));
    assert_eq!(last_two.as_array().unwrap().len(), 2);
    assert_eq!(last_two[1]["field"], "tags");
    assert!(api
        .get("/v1/audit?actor=nobody")
        .1
        .as_array()
        .unwrap()
        .is_empty());
}

#[test]
fn rule_changes_are_attributed_to_the_rule_actor() {
    let dir = tempfile::tempdir().unwrap();
    let api = start(dir.path(), false, SLOW);
    api.post("/v1/tickets", r#"{"title":"Spec"}"#);
    api.post("/v1/tickets", r#"{"title":"Part","parent":"ST-1"}"#);
    api.call_with(
        "PATCH",
        "/v1/tickets/ST-2",
        &[(ACTOR, "plugin:demo")],
        Some(r#"{"status":"in-progress"}"#),
    );
    assert!(wait_for(|| {
        let (_, rules) = api.get("/v1/audit?actor=rule");
        rules.as_array().is_some_and(|r| {
            r.iter()
                .any(|e| e["id"] == "ST-1" && e["to"] == "in-progress" && e["actor"] == "rule")
        })
    }));
    let (_, plugin) = api.get("/v1/audit?actor=plugin%3Ademo");
    assert!(plugin
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["id"] == "ST-2" && e["rule"] == "api.update"));
}

#[test]
fn invalid_actors_are_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let api = start(dir.path(), false, SLOW);
    for actor in ["has space", "", &"x".repeat(65), "semi;colon"] {
        let (status, body, _) = api.call_with(
            "POST",
            "/v1/tickets",
            &[(ACTOR, actor)],
            Some(r#"{"title":"A"}"#),
        );
        assert_eq!(status, 400, "{actor:?}");
        assert!(body["error"].as_str().unwrap().contains(ACTOR));
    }
    assert_eq!(count(&api), 0);
    let (status, _, _) = api.call_with(
        "GET",
        "/v1/tickets",
        &[(ACTOR, "x".repeat(64).as_str())],
        None,
    );
    assert_eq!(status, 200);
}

#[test]
fn a_retried_create_returns_the_first_answer_without_a_second_ticket() {
    let dir = tempfile::tempdir().unwrap();
    let api = start(dir.path(), false, SLOW);
    let headers = [(ACTOR, "plugin:demo"), (KEY, "create-1")];
    let (status, first, head) =
        api.call_with("POST", "/v1/tickets", &headers, Some(r#"{"title":"Once"}"#));
    assert_eq!(status, 201);
    assert!(!head.contains("Idempotent-Replayed"), "{head}");
    // Same request, written differently.
    let (status, second, head) = api.call_with(
        "POST",
        "/v1/tickets",
        &headers,
        Some("{ \"title\": \"Once\" }"),
    );
    assert_eq!(status, 201);
    assert!(head.contains("Idempotent-Replayed: true"), "{head}");
    assert_eq!(first, second);
    assert_eq!(count(&api), 1);
    assert_eq!(events_of(&api, "ticket.created").len(), 1);
    assert_eq!(
        api.get("/v1/audit?actor=plugin%3Ademo")
            .1
            .as_array()
            .unwrap()
            .len(),
        1
    );

    let (status, body, _) = api.call_with(
        "POST",
        "/v1/tickets",
        &headers,
        Some(r#"{"title":"Different"}"#),
    );
    assert_eq!(status, 422);
    assert!(body["error"].as_str().unwrap().contains(KEY));
    assert_eq!(count(&api), 1);

    // Keys are scoped by actor, and a request without a key is never deduplicated.
    let other = [(ACTOR, "plugin:other"), (KEY, "create-1")];
    let (status, third, _) =
        api.call_with("POST", "/v1/tickets", &other, Some(r#"{"title":"Once"}"#));
    assert_eq!(status, 201);
    assert_ne!(third["id"], first["id"]);
    api.post("/v1/tickets", r#"{"title":"Once"}"#);
    api.post("/v1/tickets", r#"{"title":"Once"}"#);
    assert_eq!(count(&api), 4);
}

#[test]
fn a_retried_update_is_applied_once() {
    let dir = tempfile::tempdir().unwrap();
    let api = start(dir.path(), false, SLOW);
    api.post("/v1/tickets", r#"{"title":"A"}"#);
    let headers = [(ACTOR, "bot"), (KEY, "u-1")];
    let body = Some(r#"{"priority":"high"}"#);
    let (status, first, _) = api.call_with("PATCH", "/v1/tickets/ST-1", &headers, body);
    assert_eq!(status, 200);
    // Another writer changes the ticket in between; the retry must not undo that.
    api.patch("/v1/tickets/ST-1", r#"{"priority":"low"}"#);
    let (status, again, head) = api.call_with("PATCH", "/v1/tickets/ST-1", &headers, body);
    assert_eq!(status, 200);
    assert!(head.contains("Idempotent-Replayed: true"));
    assert_eq!(again, first);
    assert_eq!(api.get("/v1/tickets/ST-1").1["priority"], "low");
    assert_eq!(
        api.get("/v1/audit?actor=bot").1.as_array().unwrap().len(),
        1
    );
    // The same key on another ticket is a different request.
    api.post("/v1/tickets", r#"{"title":"B"}"#);
    let (status, _, _) = api.call_with("PATCH", "/v1/tickets/ST-2", &headers, body);
    assert_eq!(status, 422);
}

#[test]
fn failed_requests_do_not_consume_their_key() {
    let dir = tempfile::tempdir().unwrap();
    let api = start(dir.path(), false, SLOW);
    let headers = [(KEY, "k")];
    let (status, _, _) = api.call_with(
        "POST",
        "/v1/tickets",
        &headers,
        Some(r#"{"title":"A","status":"bogus"}"#),
    );
    assert_eq!(status, 400);
    let (status, _, head) =
        api.call_with("POST", "/v1/tickets", &headers, Some(r#"{"title":"A"}"#));
    assert_eq!(status, 201);
    assert!(!head.contains("Idempotent-Replayed"));
    assert_eq!(count(&api), 1);
    for bad in ["", "has space", &"k".repeat(129)] {
        let (status, _, _) = api.call_with(
            "POST",
            "/v1/tickets",
            &[(KEY, bad)],
            Some(r#"{"title":"B"}"#),
        );
        assert_eq!(status, 400, "{bad:?}");
    }
    assert_eq!(count(&api), 1);
}

#[test]
fn concurrent_retries_with_one_key_create_one_ticket() {
    let dir = tempfile::tempdir().unwrap();
    let api = start(dir.path(), false, SLOW);
    let statuses: Vec<u16> = std::thread::scope(|scope| {
        let workers: Vec<_> = (0..6)
            .map(|_| {
                scope.spawn(|| {
                    api.call_with(
                        "POST",
                        "/v1/tickets",
                        &[(KEY, "race")],
                        Some(r#"{"title":"Race"}"#),
                    )
                    .0
                })
            })
            .collect();
        workers.into_iter().map(|w| w.join().unwrap()).collect()
    });
    assert!(statuses.iter().all(|s| *s == 201), "{statuses:?}");
    assert_eq!(count(&api), 1);
}

#[test]
fn keys_are_remembered_across_a_restart_and_expire() {
    let dir = tempfile::tempdir().unwrap();
    let headers = [(KEY, "restart-1")];
    let body = Some(r#"{"title":"Persisted"}"#);
    let first = {
        let api = start(dir.path(), false, SLOW);
        api.call_with("POST", "/v1/tickets", &headers, body).1
    };
    let api = start(dir.path(), false, SLOW);
    let (status, again, head) = api.call_with("POST", "/v1/tickets", &headers, body);
    assert_eq!(status, 201);
    assert!(head.contains("Idempotent-Replayed: true"), "{head}");
    assert_eq!(again, first);
    assert_eq!(count(&api), 1);
    drop(api);

    let api = start_with(dir.path(), false, SLOW, |o| {
        o.idempotency_ttl = Duration::from_secs(1);
    });
    std::thread::sleep(Duration::from_millis(2200));
    let (status, fresh, head) = api.call_with("POST", "/v1/tickets", &headers, body);
    assert_eq!(status, 201);
    assert!(!head.contains("Idempotent-Replayed"));
    assert_ne!(fresh["id"], first["id"]);
    assert_eq!(count(&api), 2);
}
