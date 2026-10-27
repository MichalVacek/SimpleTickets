//! The `simpletickets-sync` binary: its subcommands, and running as a supervised service.

mod support;

use serde_json::{json, Value};
use simpletickets_sync::state::sync_dir;
use std::process::{Command, Output};
use std::time::Duration;
use support::{start, Env};

const SYNC: &str = env!("CARGO_BIN_EXE_simpletickets-sync");

fn sync(env: &Env, args: &[&str]) -> Output {
    Command::new(SYNC)
        .arg("--dir")
        .arg(&env.dir)
        .args(["--url", &env.url, "--token-file"])
        .arg(env.token_file())
        .args(args)
        .output()
        .unwrap()
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn wait_for(mut condition: impl FnMut() -> bool) -> bool {
    for _ in 0..200 {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

#[test]
fn dry_run_lists_the_plan_and_writes_nothing() {
    let env = start(|_| {});
    env.write_config("");
    let id = env.create_titled("Planned");
    let output = sync(&env, &["dry-run", "--json"]);
    assert!(output.status.success(), "{}", text(&output));
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["actions"][0]["kind"], "create");
    assert_eq!(report["actions"][0]["id"], id.as_str());
    assert!(!env.archive.exists());
    assert!(!sync_dir(&env.dir).join("archive.json").exists());
}

#[test]
fn push_now_pushes_and_status_and_conflicts_report_the_result() {
    let env = start(|_| {});
    env.write_config("    direction: both\n    on_conflict: manual\n");
    let id = env.create_titled("Pushed");

    let pushed = sync(&env, &["push-now"]);
    assert!(pushed.status.success(), "{}", text(&pushed));
    assert!(text(&pushed).contains("create"), "{}", text(&pushed));
    assert!(env.archive.join(format!("{id}.json")).is_file());

    let status = sync(&env, &["status", "--json"]);
    assert!(status.status.success(), "{}", text(&status));
    let status: Value = serde_json::from_slice(&status.stdout).unwrap();
    assert_eq!(status["service_running"], false);
    assert_eq!(status["targets"][0]["synced"], 1);
    assert_eq!(status["targets"][0]["conflicts"], 0);

    let none = sync(&env, &["list-conflicts"]);
    assert!(text(&none).contains("no conflicts"));

    env.edit_archive_status(&format!("{id}.json"), "in-progress");
    env.patch(&id, json!({"status": "done"}));
    sync(&env, &["push-now"]);
    let conflicts = sync(&env, &["list-conflicts", "--json"]);
    let rows: Value = serde_json::from_slice(&conflicts.stdout).unwrap();
    assert_eq!(rows[0]["id"], id.as_str());
    assert_eq!(rows[0]["field"], "status");
    assert_eq!(rows[0]["resolution"], Value::Null);

    // The conflict makes `status` exit non-zero so scripts notice it.
    assert_eq!(sync(&env, &["status"]).status.code(), Some(1));

    let settled = sync(&env, &["push-now", "--prefer", "local", &id]);
    assert!(settled.status.success(), "{}", text(&settled));
    assert_eq!(
        env.archive_file(&format!("{id}.json"))["fields"]["status"],
        "done"
    );
    assert!(text(&sync(&env, &["list-conflicts"])).contains("no conflicts"));
    assert!(text(&sync(&env, &["list-conflicts", "--all"])).contains("local-wins"));
}

#[test]
fn bad_input_exits_with_a_usage_error() {
    let env = start(|_| {});
    env.write_config("");
    for args in [
        &["bogus"][..],
        &["status", "--nope"],
        &["status", "--target", "missing"],
    ] {
        let output = sync(&env, args);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {}", text(&output));
    }
    let missing = Command::new(SYNC)
        .args(["--dir", "/nonexistent-sync-dir", "status"])
        .output()
        .unwrap();
    assert_eq!(missing.status.code(), Some(2));
    assert!(text(&missing).contains("cannot read the sync config"));
}

#[test]
fn the_daemon_supervises_the_service_which_follows_events() {
    let env = start(|_| {});
    env.write_config("");
    let plugin = env.dir.join(".simpletickets/plugins/sync");
    std::fs::create_dir_all(&plugin).unwrap();
    std::fs::write(
        plugin.join("plugin.yml"),
        format!(
            "name: sync\nversion: 0.1.0\napi_version: 1\nkind: service\ncommand: '{SYNC}'\nargs: [run]\ncapabilities: [events, tickets.write]\n"
        ),
    )
    .unwrap();
    std::fs::write(
        env.dir.join("simpletickets.yml"),
        "plugins:\n  enabled: [sync]\n",
    )
    .unwrap();
    env.rescan();

    let state = |env: &Env| -> String {
        let (_, list) = env.api.request("GET", "/v1/plugins", &[], None).unwrap();
        list.as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "sync")
            .map_or(String::new(), |p| p["state"].as_str().unwrap().to_string())
    };
    assert!(wait_for(|| state(&env) == "running"));
    assert!(wait_for(|| sync(&env, &["status", "--json"])
        .status
        .success()
        && serde_json::from_slice::<Value>(
            &sync(&env, &["status", "--json"]).stdout
        )
        .is_ok_and(|v| v["service_running"] == true)));

    let id = env.create_titled("Followed");
    let file = format!("{id}.json");
    assert!(
        wait_for(|| env.archive.join(&file).is_file()),
        "pushed from the event stream"
    );
    env.patch(&id, json!({"status": "done"}));
    assert!(wait_for(
        || env.archive_file(&file)["fields"]["status"] == "done"
    ));

    // A request from the command line is queued for the running service.
    let queued = sync(&env, &["push-now", "--force"]);
    assert!(text(&queued).contains("queued"), "{}", text(&queued));
    assert!(wait_for(|| !sync_dir(&env.dir)
        .join("request.json")
        .exists()));
}
