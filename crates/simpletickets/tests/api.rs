mod common;

use common::*;
use serde_json::json;
use std::time::Duration;

const SLOW: Duration = Duration::from_secs(3600);

fn ticket_text(id: &str, status: &str, extra: &str) -> String {
    format!("---\nid: {id}\ntitle: Ticket {id}\nstatus: {status}\n{extra}---\nbody\n")
}

#[test]
fn token_is_required_on_every_route() {
    let dir = tempfile::tempdir().unwrap();
    let api = start(dir.path(), false, SLOW);
    let routes = [
        ("GET", "/v1/health"),
        ("GET", "/v1/parents"),
        ("GET", "/v1/tickets"),
        ("POST", "/v1/tickets"),
        ("GET", "/v1/tickets/ST-1"),
        ("PATCH", "/v1/tickets/ST-1"),
        ("POST", "/v1/rescan"),
        ("GET", "/v1/doctor"),
        ("GET", "/v1/config"),
        ("PUT", "/v1/config"),
        ("POST", "/v1/migration"),
        ("GET", "/v1/events"),
        ("GET", "/v1/audit"),
        ("GET", "/v1/nonexistent"),
    ];
    for (method, path) in routes {
        let body = Some(r#"{"title":"sneaky"}"#);
        let (status, _) = raw(api.addr, method, path, None, None, body);
        assert_eq!(status, 401, "{method} {path} without token");
        let (status, _) = raw(api.addr, method, path, Some("wrong"), None, body);
        assert_eq!(status, 401, "{method} {path} with wrong token");
    }
    assert_eq!(api.get("/v1/health").0, 200);
    let files = std::fs::read_dir(dir.path()).unwrap().flatten().count();
    assert_eq!(
        files, 1,
        "only the state folder exists; nothing was created"
    );
}

#[test]
fn binds_loopback_only_and_rejects_foreign_hosts() {
    let dir = tempfile::tempdir().unwrap();
    let api = start(dir.path(), false, SLOW);
    assert!(api.addr.ip().is_loopback());
    assert_eq!(api.addr.ip(), simpletickets::daemon::BIND_ADDRESS);
    let (status, _) = raw(
        api.addr,
        "GET",
        "/v1/health",
        Some(&api.token),
        Some("evil.example"),
        None,
    );
    assert_eq!(status, 403);
    let (status, _) = raw(
        api.addr,
        "GET",
        "/v1/health",
        Some(&api.token),
        Some("localhost:1"),
        None,
    );
    assert_eq!(status, 200);
}

#[cfg(unix)]
#[test]
fn token_file_is_private_and_generated_once() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let token = {
        let api = start(dir.path(), false, SLOW);
        let path = api.running.as_ref().unwrap().token_file().to_path_buf();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        api.token.clone()
    };
    let again = start(dir.path(), false, SLOW);
    assert_eq!(again.token, token);
}

#[test]
fn create_update_and_filter() {
    let dir = tempfile::tempdir().unwrap();
    let api = start(dir.path(), false, SLOW);

    let (status, parent) = api.post(
        "/v1/tickets",
        r#"{"title":"Spec","priority":"high","tags":["work"]}"#,
    );
    assert_eq!(status, 201, "{parent}");
    assert_eq!(parent["id"], "ST-1");
    assert!(parent["body"].as_str().unwrap().contains("# Spec"));
    let (_, child) = api.post(
        "/v1/tickets",
        r#"{"title":"Implement","parent":"ST-1","due":"2026-11-05"}"#,
    );
    assert_eq!(child["id"], "ST-2");
    let (_, blocked) = api.post("/v1/tickets", r#"{"title":"Ship","blocked_by":["ST-2"]}"#);
    assert_eq!(blocked["blocked"], true);

    let (_, all) = api.get("/v1/tickets");
    assert_eq!(all.as_array().unwrap().len(), 3);
    let ids = |v: &serde_json::Value| -> Vec<String> {
        v.as_array()
            .unwrap()
            .iter()
            .map(|t| t["id"].as_str().unwrap().to_string())
            .collect()
    };
    assert_eq!(ids(&api.get("/v1/tickets?tag=work").1), ["ST-1"]);
    assert_eq!(ids(&api.get("/v1/tickets?blocked=true").1), ["ST-3"]);
    assert_eq!(ids(&api.get("/v1/tickets?parent=ST-1").1), ["ST-2"]);
    assert_eq!(ids(&api.get("/v1/tickets?title=ship").1), ["ST-3"]);
    assert_eq!(ids(&api.get("/v1/tickets?due_by=2026-12-01").1), ["ST-2"]);
    assert_eq!(
        ids(&api.get("/v1/tickets?status=todo&limit=2").1),
        ["ST-1", "ST-2"]
    );
    assert_eq!(api.get("/v1/tickets?category=bogus").0, 400);
    assert_eq!(ids(&api.get("/v1/parents").1), ["ST-1"]);

    // Starting the sub-ticket marks the parent in progress through the rules.
    let (status, updated) = api.patch("/v1/tickets/ST-2", r#"{"status":"in-progress"}"#);
    assert_eq!(status, 200, "{updated}");
    assert_eq!(updated["status"], "in-progress");
    // The parent file is too fresh for the first attempt; a retry follows shortly.
    assert!(wait_for(
        || api.get("/v1/tickets/ST-1").1["status"] == "in-progress"
    ));
    assert_eq!(api.get("/v1/audit").1[0]["id"], "ST-1");

    // Finishing the blocker unblocks the dependent.
    api.patch("/v1/tickets/ST-2", r#"{"status":"done","priority":"low"}"#);
    let (_, shipped) = api.get("/v1/tickets/ST-3");
    assert_eq!(shipped["blocked"], false);
    assert_eq!(api.get("/v1/tickets/ST-2").1["priority"], "low");
    api.patch("/v1/tickets/ST-2", r#"{"priority":null}"#);
    assert!(api.get("/v1/tickets/ST-2").1["priority"].is_null());
}

#[test]
fn rejects_bad_requests() {
    let dir = tempfile::tempdir().unwrap();
    let api = start(dir.path(), false, SLOW);
    api.post("/v1/tickets", r#"{"title":"One"}"#);
    assert_eq!(api.post("/v1/tickets", "not json").0, 400);
    assert_eq!(api.post("/v1/tickets", r#"{"title":"x","bogus":1}"#).0, 400);
    assert_eq!(
        api.post("/v1/tickets", r#"{"title":"x","status":"nope"}"#)
            .0,
        400
    );
    assert_eq!(
        api.post("/v1/tickets", r#"{"title":"x","parent":"ST-9"}"#)
            .0,
        400
    );
    assert_eq!(api.patch("/v1/tickets/ST-9", r#"{"status":"done"}"#).0, 404);
    assert_eq!(api.patch("/v1/tickets/ST-1", r#"{"id":"ST-5"}"#).0, 400);
    assert_eq!(
        api.patch("/v1/tickets/ST-1", r#"{"blocked_by":["ST-1"]}"#)
            .0,
        400
    );
    assert_eq!(api.get("/v1/tickets/ST-9").0, 404);
    assert_eq!(api.call("DELETE", "/v1/tickets/ST-1", None).0, 405);
    assert_eq!(api.get("/v1/nope").0, 404);
    assert_eq!(api.get("/v1/tickets").1.as_array().unwrap().len(), 1);
}

#[test]
fn rescan_doctor_and_external_edits() {
    let dir = tempfile::tempdir().unwrap();
    let api = start(dir.path(), false, SLOW);
    write(
        dir.path(),
        "x.md",
        &ticket_text("X", "todo", "blocked_by: [MISSING]\n"),
    );
    write(
        dir.path(),
        "bad.md",
        "---\nid: B\ntitle: [oops\nstatus: todo\n---\n",
    );
    let (status, summary) = api.post("/v1/rescan", "");
    assert_eq!(status, 200);
    assert_eq!(summary["tickets"], 1);
    assert_eq!(summary["problems"], 1);
    let (_, doctor) = api.get("/v1/doctor");
    assert_eq!(doctor["clean"], false);
    let text = doctor["issues"].to_string();
    assert!(
        text.contains("bad.md") && text.contains("MISSING"),
        "{text}"
    );
}

#[test]
fn config_get_and_change() {
    let dir = tempfile::tempdir().unwrap();
    let api = start(dir.path(), false, SLOW);
    let (_, current) = api.get("/v1/config");
    assert_eq!(current["config"]["id_prefix"], "ST");
    assert!(current["yaml"].is_null());

    let (status, bad) = api.call(
        "PUT",
        "/v1/config",
        Some(&json!({"yaml": "id_prefix: a-b\n"}).to_string()),
    );
    assert_eq!(status, 400, "{bad}");
    assert_eq!(api.call("PUT", "/v1/config", Some("{}")).0, 400);

    let yaml = "id_prefix: WK\n";
    let (status, changed) = api.call(
        "PUT",
        "/v1/config",
        Some(&json!({"yaml": yaml}).to_string()),
    );
    assert_eq!(status, 200, "{changed}");
    assert_eq!(changed["config"]["id_prefix"], "WK");
    assert_eq!(
        std::fs::read_to_string(dir.path().join("simpletickets.yml")).unwrap(),
        yaml
    );
    assert_eq!(api.get("/v1/config").1["yaml"], yaml);
    assert_eq!(
        api.post("/v1/tickets", r#"{"title":"New"}"#).1["id"],
        "WK-1"
    );
}

#[test]
fn event_stream_reports_changes() {
    let dir = tempfile::tempdir().unwrap();
    let api = start(dir.path(), false, SLOW);
    let (_, first) = api.get("/v1/events");
    let start_seq = first["next"].as_u64().unwrap();
    assert!(first["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["type"] == "scan"));

    let addr = api.addr;
    let token = api.token.clone();
    let poller = std::thread::spawn(move || {
        raw(
            addr,
            "GET",
            &format!("/v1/events?since={start_seq}&wait=10"),
            Some(&token),
            None,
            None,
        )
    });
    std::thread::sleep(Duration::from_millis(300));
    api.post("/v1/tickets", r#"{"title":"Event"}"#);
    let (status, body) = poller.join().unwrap();
    assert_eq!(status, 200);
    let types: Vec<&str> = body["events"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["type"].as_str().unwrap())
        .collect();
    assert!(types.contains(&"ticket.created"), "{types:?}");
    let (_, idle) = api.get(&format!("/v1/events?since={}", body["next"]));
    assert!(idle["events"]
        .as_array()
        .unwrap()
        .iter()
        .all(|e| e["type"] != "ticket.created"));
}

#[test]
fn startup_scan_finds_existing_tickets() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "a.md", &ticket_text("A", "todo", ""));
    let api = start(dir.path(), false, SLOW);
    assert_eq!(api.get("/v1/tickets").1.as_array().unwrap().len(), 1);
    assert_eq!(api.get("/v1/health").1["tickets"], 1);
}

#[test]
fn file_watcher_triggers_scan() {
    let dir = tempfile::tempdir().unwrap();
    let api = start(dir.path(), true, SLOW);
    std::thread::sleep(Duration::from_millis(500));
    write(dir.path(), "late.md", &ticket_text("LATE", "todo", ""));
    assert!(
        wait_for(|| api.get("/v1/tickets/LATE").0 == 200),
        "watcher did not pick up the new file"
    );
}

#[test]
fn periodic_scan_finds_new_files() {
    let dir = tempfile::tempdir().unwrap();
    let api = start(dir.path(), false, Duration::from_millis(500));
    write(dir.path(), "late.md", &ticket_text("LATE", "todo", ""));
    assert!(wait_for(|| api.get("/v1/tickets/LATE").0 == 200));
}

#[test]
fn periodic_scan_runs_rules_on_hand_edited_files() {
    let dir = tempfile::tempdir().unwrap();
    let api = start(dir.path(), false, Duration::from_millis(500));
    write(dir.path(), "p.md", &ticket_text("P", "todo", ""));
    write(
        dir.path(),
        "c.md",
        &ticket_text("C", "in-progress", "parent: P\n"),
    );
    assert!(wait_for(
        || api.get("/v1/tickets/P").1["status"] == "in-progress"
    ));
}
