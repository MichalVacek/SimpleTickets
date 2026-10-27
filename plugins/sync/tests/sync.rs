//! The sync engine against a live in-process daemon and the real archive connector.

mod support;

use serde_json::{json, Value};
use simpletickets_connector_protocol::{ErrorCode, ErrorInfo, Transport};
use simpletickets_sync::engine::{Engine, Options, Prefer};
use simpletickets_sync::state::{sync_dir, TargetState};
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use support::{kinds, start, Env};

const BOTH_MANUAL: &str = "    direction: both\n    on_conflict: manual\n";

fn only(ids: &[&str]) -> Option<BTreeSet<String>> {
    Some(ids.iter().map(ToString::to_string).collect())
}

#[test]
fn a_new_ticket_is_pushed_and_nothing_is_written_into_the_ticket_file() {
    let env = start(|_| {});
    let config = env.write_config("");
    let id = env.create_titled("Write report");
    let ticket_file = std::fs::read_to_string(env.ticket(&id)["path"].as_str().unwrap()).unwrap();

    let report = env.full(&config);
    assert_eq!(kinds(&report), [format!("create {id}")]);
    let file = env.archive_file(&format!("{id}.json"));
    assert_eq!(file["ticket"]["title"], "Write report");
    assert_eq!(file["fields"]["status"], "todo");

    let state = env.state();
    assert_eq!(state["tickets"][&id]["ref"], id.as_str());
    assert_eq!(state["tickets"][&id]["phase"], "synced");
    assert!(!state["tickets"][&id]["hash"].as_str().unwrap().is_empty());

    let after = std::fs::read_to_string(env.ticket(&id)["path"].as_str().unwrap()).unwrap();
    assert_eq!(after, ticket_file, "sync state must not touch ticket files");
    assert!(!after.contains("archive"));
}

#[test]
fn a_status_change_updates_the_remote_and_a_restart_pushes_nothing_twice() {
    let env = start(|_| {});
    let config = env.write_config("");
    let id = env.create_titled("Ship it");
    env.full(&config);

    env.patch(&id, json!({"status": "in-progress"}));
    let report = env.pass(
        &config,
        &Options {
            only_ids: only(&[&id]),
            ..Options::default()
        },
    );
    assert_eq!(kinds(&report), [format!("update {id}")]);
    assert_eq!(
        env.archive_file(&format!("{id}.json"))["fields"]["status"],
        "in-progress"
    );

    // A new engine with a new connector process, as after a restart: nothing to do.
    let restarted = env.full(&config);
    assert!(restarted.actions.is_empty(), "{:?}", restarted.actions);
    let files = std::fs::read_dir(&env.archive).unwrap().count();
    assert_eq!(files, 1);
}

#[test]
fn a_missing_state_file_does_not_duplicate_remote_items() {
    let env = start(|_| {});
    let config = env.write_config("");
    let id = env.create_titled("Once");
    env.full(&config);
    std::fs::remove_file(sync_dir(&env.dir).join("archive.json")).unwrap();
    env.full(&config);
    assert_eq!(std::fs::read_dir(&env.archive).unwrap().count(), 1);
    assert!(env.archive.join(format!("{id}.json")).is_file());
}

#[test]
fn parents_and_blockers_are_pushed_first_and_linked_by_reference() {
    let env = start(|_| {});
    let config = env.write_config("");
    let parent = env.create(json!({"title": "Parent", "role": "parent"}));
    let blocker = env.create_titled("Blocker");
    let child = env.create(json!({"title": "Child", "parent": parent, "blocked_by": [blocker]}));
    let report = env.full(&config);
    assert_eq!(report.count("create"), 3, "{:?}", report.actions);
    let links = &env.archive_file(&format!("{child}.json"))["links"];
    assert_eq!(links["parent_ref"], parent.as_str());
    assert_eq!(links["blocked_by_refs"], json!([blocker]));
}

#[test]
fn a_remote_only_change_is_written_back_as_the_sync_actor() {
    let env = start(|_| {});
    let config = env.write_config("    direction: both\n");
    let id = env.create_titled("Pulled");
    env.full(&config);

    env.edit_archive_status(&format!("{id}.json"), "in-progress");
    let report = env.full(&config);
    assert!(
        kinds(&report).contains(&format!("pull {id}")),
        "{:?}",
        report.actions
    );
    let ticket = env.ticket(&id);
    assert_eq!(ticket["status"], "in-progress");
    assert!(ticket["body"].as_str().unwrap().contains("Pulled") || ticket["body"].is_string());

    let (_, audit) = env
        .api
        .request("GET", "/v1/audit?actor=plugin%3Async", &[], None)
        .unwrap();
    let entries = audit.as_array().unwrap();
    assert!(entries
        .iter()
        .any(|e| e["id"] == id.as_str() && e["field"] == "status" && e["to"] == "in-progress"));

    // The write-back does not bounce back to the remote as a new push.
    let again = env.full(&config);
    assert!(again.actions.is_empty(), "{:?}", again.actions);
}

#[test]
fn a_write_back_never_changes_the_ticket_body() {
    let env = start(|_| {});
    let config = env.write_config("    direction: both\n");
    let id = env.create(json!({"title": "Body", "body": "keep me exactly\n"}));
    env.full(&config);
    env.edit_archive_status(&format!("{id}.json"), "done");
    env.full(&config);
    assert_eq!(env.ticket(&id)["status"], "done");
    assert_eq!(env.ticket(&id)["body"], "keep me exactly\n");
}

#[test]
fn a_conflict_is_reported_and_left_alone_until_the_operator_decides() {
    let env = start(|_| {});
    let config = env.write_config(BOTH_MANUAL);
    let id = env.create_titled("Contested");
    env.full(&config);

    env.edit_archive_status(&format!("{id}.json"), "in-progress");
    env.patch(&id, json!({"status": "done"}));
    let report = env.full(&config);
    assert_eq!(report.count("conflict"), 1, "{:?}", report.actions);
    assert_eq!(env.ticket(&id)["status"], "done");
    assert_eq!(
        env.archive_file(&format!("{id}.json"))["fields"]["status"],
        "in-progress"
    );
    let state = TargetState::load(&env.dir, "archive").unwrap();
    let open: Vec<_> = state.open_conflicts().collect();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].field.as_deref(), Some("status"));
    assert_eq!(open[0].local, "done");
    assert_eq!(open[0].remote, "in-progress");

    // Still untouched on the next pass, and still reported once.
    let again = env.full(&config);
    assert_eq!(
        env.archive_file(&format!("{id}.json"))["fields"]["status"],
        "in-progress"
    );
    assert_eq!(again.count("create") + again.count("update"), 0);
    assert_eq!(
        TargetState::load(&env.dir, "archive")
            .unwrap()
            .open_conflicts()
            .count(),
        1
    );

    let settled = env.pass(
        &config,
        &Options {
            only_ids: only(&[&id]),
            prefer: Some(Prefer::Local),
            pull: true,
            ..Options::default()
        },
    );
    assert!(
        kinds(&settled).contains(&format!("update {id}")),
        "{:?}",
        settled.actions
    );
    assert_eq!(
        env.archive_file(&format!("{id}.json"))["fields"]["status"],
        "done"
    );
    assert_eq!(
        TargetState::load(&env.dir, "archive")
            .unwrap()
            .open_conflicts()
            .count(),
        0
    );
}

#[test]
fn preferring_the_remote_writes_its_value_into_the_ticket() {
    let env = start(|_| {});
    let config = env.write_config(BOTH_MANUAL);
    let id = env.create_titled("Contested");
    env.full(&config);
    env.edit_archive_status(&format!("{id}.json"), "in-progress");
    env.patch(&id, json!({"status": "done"}));
    env.full(&config);

    env.pass(
        &config,
        &Options {
            only_ids: only(&[&id]),
            prefer: Some(Prefer::Remote),
            pull: true,
            ..Options::default()
        },
    );
    assert_eq!(env.ticket(&id)["status"], "in-progress");
    let state = TargetState::load(&env.dir, "archive").unwrap();
    assert_eq!(state.open_conflicts().count(), 0);
    assert_eq!(
        state.conflicts[0].resolution.as_deref(),
        Some("remote-wins")
    );
}

#[test]
fn local_wins_by_default_and_the_conflict_is_still_recorded() {
    let env = start(|_| {});
    let config = env.write_config("    direction: both\n");
    let id = env.create_titled("Contested");
    env.full(&config);
    env.edit_archive_status(&format!("{id}.json"), "in-progress");
    env.patch(&id, json!({"status": "done"}));
    env.full(&config);
    assert_eq!(
        env.archive_file(&format!("{id}.json"))["fields"]["status"],
        "done"
    );
    assert_eq!(env.ticket(&id)["status"], "done");
    let state = TargetState::load(&env.dir, "archive").unwrap();
    assert_eq!(state.conflicts.len(), 1);
    assert_eq!(state.conflicts[0].resolution.as_deref(), Some("local-wins"));
}

#[test]
fn a_remote_item_that_vanished_is_reported_and_recreated_under_local_wins() {
    let env = start(|_| {});
    let config = env.write_config("");
    let id = env.create_titled("Lost");
    env.full(&config);
    std::fs::remove_file(env.archive.join(format!("{id}.json"))).unwrap();
    env.patch(&id, json!({"title": "Lost again"}));
    let report = env.full(&config);
    assert!(report.count("conflict") == 1, "{:?}", report.actions);
    assert_eq!(
        env.archive_file(&format!("{id}.json"))["ticket"]["title"],
        "Lost again"
    );
}

#[test]
fn dry_run_changes_nothing_anywhere() {
    let env = start(|_| {});
    let config = env.write_config("    direction: both\n");
    let id = env.create_titled("Planned");
    let before = std::fs::read_to_string(env.ticket(&id)["path"].as_str().unwrap()).unwrap();
    let audit_before = env.api.request("GET", "/v1/audit", &[], None).unwrap().1;

    let report = env.pass(
        &config,
        &Options {
            dry_run: true,
            pull: true,
            ..Options::default()
        },
    );
    assert_eq!(kinds(&report), [format!("create {id}")]);
    assert!(!env.archive.exists());
    assert!(!sync_dir(&env.dir).join("archive.json").exists());
    assert_eq!(
        std::fs::read_to_string(env.ticket(&id)["path"].as_str().unwrap()).unwrap(),
        before
    );
    assert_eq!(
        env.api.request("GET", "/v1/audit", &[], None).unwrap().1,
        audit_before
    );

    // After a real pass, a dry run of a remote edit plans the write-back and does not make it.
    env.full(&config);
    env.edit_archive_status(&format!("{id}.json"), "in-progress");
    let state_before = std::fs::read_to_string(sync_dir(&env.dir).join("archive.json")).unwrap();
    let plan = env.pass(
        &config,
        &Options {
            dry_run: true,
            pull: true,
            ..Options::default()
        },
    );
    assert_eq!(kinds(&plan), [format!("pull {id}")]);
    assert_eq!(env.ticket(&id)["status"], "todo");
    assert_eq!(
        std::fs::read_to_string(sync_dir(&env.dir).join("archive.json")).unwrap(),
        state_before
    );
}

#[test]
fn a_deleted_ticket_closes_its_remote_item_and_never_deletes_it() {
    let env = start(|_| {});
    let config = env.write_config("");
    let id = env.create_titled("Doomed");
    env.full(&config);
    let path = env.ticket(&id)["path"].as_str().unwrap().to_string();
    std::fs::remove_file(path).unwrap();
    env.rescan();

    let report = env.full(&config);
    assert_eq!(kinds(&report), [format!("close {id}")]);
    let file = env.archive_file(&format!("{id}.json"));
    assert_eq!(file["closed"], true);
    assert_eq!(env.state()["tickets"][&id]["phase"], "closed");
    assert!(env.full(&config).actions.is_empty());
}

#[test]
fn the_ignore_policy_leaves_the_remote_item_alone() {
    let env = start(|_| {});
    let config = env.write_config("    on_delete: ignore\n");
    let id = env.create_titled("Doomed");
    env.full(&config);
    std::fs::remove_file(env.ticket(&id)["path"].as_str().unwrap()).unwrap();
    env.rescan();
    env.full(&config);
    assert_eq!(env.archive_file(&format!("{id}.json"))["closed"], false);
    assert_eq!(env.state()["tickets"][&id]["phase"], "deleted");
}

#[test]
fn the_filter_keeps_other_tickets_out() {
    let env = start(|_| {});
    let config = env.write_config("    filter: {tags: [sync]}\n");
    let wanted = env.create(json!({"title": "In", "tags": ["sync"]}));
    env.create_titled("Out");
    let report = env.full(&config);
    assert_eq!(kinds(&report), [format!("create {wanted}")]);
}

#[test]
fn a_permanent_refusal_is_recorded_and_not_retried_until_the_ticket_changes() {
    let env = start(|_| {});
    let blocker = env.archive.parent().unwrap().join("not-a-folder");
    std::fs::write(&blocker, "file").unwrap();
    let mut yaml = env.config_yaml("");
    yaml = yaml.replace(
        &env.archive.display().to_string(),
        &blocker.display().to_string(),
    );
    let config = simpletickets_sync::config::Config::parse(&yaml).unwrap();
    let id = env.create_titled("Refused");
    let first = env.full(&config);
    assert_eq!(first.count("error"), 1, "{:?}", first.actions);
    let entry = &env.state()["tickets"][&id];
    assert_eq!(entry["phase"], "error");
    assert!(entry["failed_hash"].is_string());
    let second = env.full(&config);
    assert!(second.actions.is_empty(), "{:?}", second.actions);
    env.patch(&id, json!({"title": "Refused, edited"}));
    assert_eq!(env.full(&config).count("error"), 1);
}

/// A connector that answers from a script, for failure handling.
struct Scripted {
    pushes: Arc<Mutex<u32>>,
    failure: Arc<Mutex<Option<ErrorInfo>>>,
}

impl Transport for Scripted {
    fn call(&mut self, op: &str, _params: Value) -> Result<Value, ErrorInfo> {
        match op {
            "describe" => Ok(json!({"name": "scripted", "protocol": [1]})),
            "configure" => Ok(json!({"capabilities": ["close"]})),
            "push" => {
                *self.pushes.lock().unwrap() += 1;
                match self.failure.lock().unwrap().clone() {
                    Some(e) => Err(e),
                    None => Ok(json!({"ref": "R-1", "outcome": "created"})),
                }
            }
            other => Err(ErrorInfo::new(ErrorCode::Unsupported, other)),
        }
    }

    fn is_alive(&mut self) -> bool {
        true
    }
}

type Handles = (Scripted, Arc<Mutex<u32>>, Arc<Mutex<Option<ErrorInfo>>>);

fn scripted(env: &Env, failure: Option<ErrorInfo>) -> Handles {
    let _ = env;
    let pushes = Arc::new(Mutex::new(0));
    let failure = Arc::new(Mutex::new(failure));
    (
        Scripted {
            pushes: Arc::clone(&pushes),
            failure: Arc::clone(&failure),
        },
        pushes,
        failure,
    )
}

#[test]
fn transient_failures_back_off_and_are_retried_with_force() {
    let env = start(|_| {});
    let config = env.write_config("");
    let id = env.create_titled("Flaky");
    let (connector, pushes, failure) = scripted(
        &env,
        Some(ErrorInfo::new(ErrorCode::RateLimited, "slow down").with_retry_after(60_000)),
    );
    let mut engine = Engine::new(&env.api, &config, &env.dir, None);
    engine.use_connector("archive", Box::new(connector));

    let first = engine.run(&Options::default()).unwrap();
    assert_eq!(first.count("error"), 1);
    let entry = &env.state()["tickets"][&id];
    assert_eq!(entry["attempts"], 1);
    assert!(entry["next_attempt"].as_u64().unwrap() > simpletickets_sync::state::now_unix() + 30);

    engine.run(&Options::default()).unwrap();
    assert_eq!(*pushes.lock().unwrap(), 1, "waits for the retry time");

    *failure.lock().unwrap() = None;
    let forced = engine
        .run(&Options {
            force: true,
            ..Options::default()
        })
        .unwrap();
    assert_eq!(*pushes.lock().unwrap(), 2);
    assert_eq!(forced.count("create"), 1);
    let entry = &env.state()["tickets"][&id];
    assert_eq!(entry["attempts"], 0);
    assert_eq!(entry["ref"], "R-1");
}

#[test]
fn refused_credentials_stop_the_target_for_the_pass() {
    let env = start(|_| {});
    let config = env.write_config("");
    env.create_titled("One");
    env.create_titled("Two");
    let (connector, pushes, _) = scripted(&env, Some(ErrorInfo::new(ErrorCode::Auth, "bad token")));
    let mut engine = Engine::new(&env.api, &config, &env.dir, None);
    engine.use_connector("archive", Box::new(connector));
    let report = engine.run(&Options::default()).unwrap();
    assert_eq!(*pushes.lock().unwrap(), 1, "stops after the first refusal");
    assert_eq!(report.target_errors.len(), 1);
    assert!(env.state()["last_error"]
        .as_str()
        .unwrap()
        .contains("credentials refused"));
}
