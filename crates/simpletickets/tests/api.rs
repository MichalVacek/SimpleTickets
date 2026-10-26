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
        ("GET", "/v1/tombstones"),
        ("DELETE", "/v1/tombstones/ST-1"),
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
fn poll_watcher_triggers_scan() {
    let dir = tempfile::tempdir().unwrap();
    let api = start_with(dir.path(), true, SLOW, |o| {
        o.poll_interval = Some(Duration::from_millis(100));
    });
    std::thread::sleep(Duration::from_millis(500));
    write(dir.path(), "late.md", &ticket_text("LATE", "todo", ""));
    assert!(
        wait_for(|| api.get("/v1/tickets/LATE").0 == 200),
        "polling did not pick up the new file"
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

const OLD_CONFIG: &str = "parent_folder: specs\nsubticket_folder: work\n";
const NEW_CONFIG: &str = "parent_folder: plans\nsubticket_folder: tasks\n\
statuses:\n  todo: [todo]\n  in-progress: [doing]\n  done: [done]\n  cancelled: [cancelled]\n";

fn migration_project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    write(d, "simpletickets.yml", OLD_CONFIG);
    write(d, "specs/P-1.md", &ticket_text("P-1", "todo", ""));
    write(
        d,
        "work/T-1.md",
        &ticket_text("T-1", "in-progress", "parent: P-1\n"),
    );
    write(
        d,
        "work/T-2.md",
        &ticket_text("T-2", "todo", "parent: P-1\nblocked_by: [T-1]\n"),
    );
    dir
}

#[test]
fn config_change_needing_migration_is_refused_then_dry_run_then_applied() {
    let dir = migration_project();
    let d = dir.path();
    let api = start(d, false, SLOW);
    let put = |migrate: bool| {
        api.call(
            "PUT",
            "/v1/config",
            Some(&json!({"yaml": NEW_CONFIG, "migrate": migrate}).to_string()),
        )
    };

    let (status, refused) = put(false);
    assert_eq!(status, 409, "{refused}");
    assert_eq!(refused["plan"]["actions"].as_array().unwrap().len(), 4);
    assert_eq!(
        std::fs::read_to_string(d.join("simpletickets.yml")).unwrap(),
        OLD_CONFIG
    );

    let (status, dry) = api.post("/v1/migration", &json!({"yaml": NEW_CONFIG}).to_string());
    assert_eq!(status, 200, "{dry}");
    assert_eq!(dry["dry_run"], true);
    assert!(d.join("specs/P-1.md").exists(), "dry run moved nothing");
    assert!(api.get("/v1/migration").1["journal"].is_null());

    let (status, done) = api.post(
        "/v1/migration",
        &json!({"yaml": NEW_CONFIG, "dry_run": false}).to_string(),
    );
    assert_eq!(status, 200, "{done}");
    assert_eq!(done["journal"]["state"], "complete");
    assert_eq!(done["journal"]["integrity"]["ok"], true);
    assert!(d.join("plans/P-1.md").exists() && d.join("tasks/T-2.md").exists());
    assert!(!d.join("specs/P-1.md").exists());
    assert!(api.get("/v1/config").1["config"]["parent_folder"]
        .as_str()
        .unwrap()
        .ends_with("plans"));
    assert_eq!(api.get("/v1/tickets/T-1").1["status"], "doing");
    assert_eq!(api.get("/v1/tickets/T-2").1["blocked_by"][0], "T-1");
    assert_eq!(api.get("/v1/doctor").1["clean"], true);
    assert_eq!(api.get("/v1/migration").1["journal"]["state"], "complete");
}

#[test]
fn put_config_with_migrate_flag_applies_the_migration() {
    let dir = migration_project();
    let api = start(dir.path(), false, SLOW);
    let body = json!({"yaml": NEW_CONFIG, "migrate": true}).to_string();
    let (status, applied) = api.call("PUT", "/v1/config", Some(&body));
    assert_eq!(status, 200, "{applied}");
    assert_eq!(applied["journal"]["state"], "complete");
    assert!(dir.path().join("plans/P-1.md").exists());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("simpletickets.yml")).unwrap(),
        NEW_CONFIG
    );
    let events = api.get("/v1/events").1.to_string();
    assert!(events.contains("migration.finished"));
}

#[test]
fn daemon_resumes_an_interrupted_migration_on_startup() {
    use simpletickets_core::config::Config;
    use simpletickets_core::migration::{self, JournalState, RunOptions};
    use simpletickets_core::store::Store;

    let dir = migration_project();
    let d = dir.path().canonicalize().unwrap();
    let store = Store::scan(&Config::load_from_dir(&d).unwrap());
    let journal = migration::begin(&d, &store, NEW_CONFIG).unwrap();
    let partial = migration::run(&d, journal, RunOptions { max_steps: Some(2) }).unwrap();
    assert_eq!(partial.state, JournalState::Applying);
    assert_eq!(
        std::fs::read_to_string(d.join("simpletickets.yml")).unwrap(),
        OLD_CONFIG
    );

    let api = start(&d, false, SLOW);
    assert_eq!(
        std::fs::read_to_string(d.join("simpletickets.yml")).unwrap(),
        NEW_CONFIG
    );
    assert_eq!(api.get("/v1/migration").1["journal"]["state"], "complete");
    assert_eq!(api.get("/v1/tickets/T-1").1["status"], "doing");
    assert!(d.join("plans/P-1.md").exists() && d.join("tasks/T-1.md").exists());
    assert_eq!(api.get("/v1/doctor").1["clean"], true);
    assert_eq!(api.post("/v1/migration/resume", "").0, 404);
}

#[test]
fn migration_endpoints_validate_input() {
    let dir = migration_project();
    let api = start(dir.path(), false, SLOW);
    assert_eq!(
        api.post("/v1/migration", r#"{"yaml":"id_prefix: a-b\n"}"#)
            .0,
        400
    );
    assert_eq!(api.post("/v1/migration", "{}").0, 400);
    assert_eq!(api.call("DELETE", "/v1/migration", None).0, 405);
    let same = json!({"yaml": OLD_CONFIG, "dry_run": true}).to_string();
    let (_, plan) = api.post("/v1/migration", &same);
    assert!(plan["plan"]["actions"].as_array().unwrap().is_empty());
}

#[test]
fn hand_edited_config_suggests_a_migration_event() {
    let dir = migration_project();
    let api = start(dir.path(), false, Duration::from_millis(500));
    std::fs::write(dir.path().join("simpletickets.yml"), NEW_CONFIG).unwrap();
    let seen = wait_for(|| {
        api.get("/v1/events")
            .1
            .to_string()
            .contains("migration.suggested")
    });
    assert!(seen);
    assert!(
        dir.path().join("specs/P-1.md").exists(),
        "nothing moves without an explicit apply"
    );
}

#[test]
fn deleted_blocker_stays_open_until_its_tombstone_is_forgotten() {
    let dir = tempfile::tempdir().unwrap();
    write(dir.path(), "a.md", &ticket_text("A", "done", ""));
    write(
        dir.path(),
        "b.md",
        &ticket_text("B", "todo", "blocked_by: [A]\n"),
    );
    let api = start(dir.path(), false, SLOW);
    assert_eq!(api.get("/v1/tickets/B").1["blocked"], false);

    std::fs::remove_file(dir.path().join("a.md")).unwrap();
    api.post("/v1/rescan", "");
    let (_, b) = api.get("/v1/tickets/B");
    assert_eq!(b["blocked"], true);
    assert_eq!(b["open_blockers"][0], "A");

    let (_, doctor) = api.get("/v1/doctor");
    assert_eq!(doctor["clean"], false);
    assert!(doctor["issues"][0].as_str().unwrap().contains("tombstone"));
    let (_, tombs) = api.get("/v1/tombstones");
    assert_eq!(tombs[0]["id"], "A");
    assert_eq!(tombs[0]["referenced_by"][0], "B");

    assert_eq!(api.call("DELETE", "/v1/tombstones/B", None).0, 400);
    assert_eq!(api.call("DELETE", "/v1/tombstones/NOPE", None).0, 404);
    assert_eq!(api.call("DELETE", "/v1/tombstones/A", None).0, 200);
    assert_eq!(api.get("/v1/tickets/B").1["blocked"], false);
    assert_eq!(api.get("/v1/tombstones").1.as_array().unwrap().len(), 0);
    let (_, events) = api.get("/v1/events");
    assert!(events["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["type"] == "tombstone.forgotten"));
}

#[test]
fn offline_root_freezes_its_tickets_and_recovers() {
    let dir = tempfile::tempdir().unwrap();
    let ext_parent = tempfile::tempdir().unwrap();
    let ext = ext_parent.path().join("vault");
    write(&ext, "x.md", &ticket_text("X", "in-progress", ""));
    write(
        dir.path(),
        "w.md",
        &ticket_text("W", "todo", "blocked_by: [X]\n"),
    );
    write(
        dir.path(),
        "simpletickets.yml",
        &format!(
            "roots:\n  - path: {}\n  - path: {}\n",
            dir.path().display(),
            ext.display()
        ),
    );
    let api = start(dir.path(), false, SLOW);
    assert_eq!(api.get("/v1/health").1["status"], "ok");
    assert_eq!(api.get("/v1/tickets/W").1["blocked"], true);

    let away = ext_parent.path().join("away");
    std::fs::rename(&ext, &away).unwrap();
    api.post("/v1/rescan", "");
    let (_, health) = api.get("/v1/health");
    assert_eq!(health["status"], "degraded");
    assert_eq!(health["frozen"], 1);
    assert_eq!(health["offline_roots"].as_array().unwrap().len(), 1);
    let (_, doctor) = api.get("/v1/doctor");
    assert!(doctor["issues"][0].as_str().unwrap().contains("offline"));
    let (_, x) = api.get("/v1/tickets/X");
    assert_eq!(x["frozen"], true);
    assert_eq!(x["status"], "in-progress");
    assert_eq!(api.get("/v1/tickets/W").1["blocked"], true);
    let (status, _) = api.patch("/v1/tickets/X", r#"{"status":"done"}"#);
    assert_eq!(status, 409);

    std::fs::rename(&away, &ext).unwrap();
    api.post("/v1/rescan", "");
    assert_eq!(api.get("/v1/health").1["status"], "ok");
    assert_eq!(api.get("/v1/tickets/X").1["frozen"], false);
    assert_eq!(api.get("/v1/doctor").1["clean"], true);
    let (status, _) = api.patch("/v1/tickets/X", r#"{"status":"done"}"#);
    assert_eq!(status, 200);
    assert_eq!(api.get("/v1/tickets/W").1["blocked"], false);

    let (_, events) = api.get("/v1/events");
    let kinds: Vec<&str> = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|e| e["type"].as_str())
        .filter(|t| t.starts_with("root."))
        .collect();
    assert_eq!(kinds, ["root.offline", "root.online"]);
}

#[test]
fn offline_root_recovers_by_itself_on_the_periodic_scan() {
    let dir = tempfile::tempdir().unwrap();
    let ext_parent = tempfile::tempdir().unwrap();
    let ext = ext_parent.path().join("vault");
    write(&ext, "x.md", &ticket_text("X", "todo", ""));
    write(
        dir.path(),
        "simpletickets.yml",
        &format!(
            "roots:\n  - path: {}\n  - path: {}\n",
            dir.path().display(),
            ext.display()
        ),
    );
    let away = ext_parent.path().join("away");
    std::fs::rename(&ext, &away).unwrap();
    let api = start(dir.path(), false, Duration::from_millis(200));
    assert!(wait_for(|| api.get("/v1/health").1["status"] == "degraded"));
    std::fs::rename(&away, &ext).unwrap();
    assert!(wait_for(|| api.get("/v1/health").1["status"] == "ok"));
    assert_eq!(api.get("/v1/tickets/X").1["frozen"], false);
}
