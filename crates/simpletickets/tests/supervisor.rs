mod common;

use common::*;
use serde_json::Value;
use simpletickets::logger::{Level, Logger};
use simpletickets::supervisor::Policy;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const SLOW: Duration = Duration::from_secs(3600);
const BIN: &str = env!("CARGO_BIN_EXE_simpletickets");

fn fast_policy() -> Policy {
    Policy {
        backoff_initial: Duration::from_millis(10),
        backoff_cap: Duration::from_millis(40),
        max_crashes: 3,
        window: Duration::from_secs(60),
        stop_grace: Duration::from_secs(5),
    }
}

/// Writes a plugin folder with a manifest for the given command.
fn install(tickets: &Path, name: &str, kind: &str, command: &str, args: &[String], extra: &str) {
    let quoted: Vec<String> = args
        .iter()
        .map(|a| format!("'{}'", a.replace('\'', "''")))
        .collect();
    write(
        tickets,
        &format!(".simpletickets/plugins/{name}/plugin.yml"),
        &format!(
            "name: {name}\nversion: 1.0.0\napi_version: 1\nkind: {kind}\n\
             command: '{command}'\nargs: [{}]\n{extra}",
            quoted.join(", ")
        ),
    );
}

fn enable(tickets: &Path, names: &[&str]) {
    write(
        tickets,
        "simpletickets.yml",
        &format!("plugins:\n  enabled: [{}]\n", names.join(", ")),
    );
}

struct Harness {
    api: Api,
    log: PathBuf,
}

fn start_daemon(tickets: &Path, logs: &Path) -> Harness {
    let log = logs.join("daemon.log");
    let log_for_daemon = log.clone();
    let api = start_with(tickets, false, SLOW, move |o| {
        o.user_plugins_dir = None;
        o.plugin_policy = fast_policy();
        o.logger = Arc::new(Logger::file(Level::Info, &log_for_daemon).unwrap());
    });
    Harness { api, log }
}

impl Harness {
    fn plugin(&self, name: &str) -> Value {
        let (status, body) = self.api.get("/v1/plugins");
        assert_eq!(status, 200);
        body.as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == name)
            .cloned()
            .unwrap_or(Value::Null)
    }

    fn wait_state(&self, name: &str, state: &str) -> bool {
        wait_for(|| self.plugin(name)["state"] == state)
    }

    fn events(&self, kind: &str) -> Vec<Value> {
        let page = self.api.get("/v1/events").1;
        page["events"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["type"] == kind)
            .cloned()
            .collect()
    }

    fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }
}

/// A plugin that is a second, idle daemon on its own folder: long-running, prints a line
/// and releases its lease file only when it is stopped through its signal handler.
fn child_daemon_args(child_dir: &Path) -> Vec<String> {
    vec![
        "--dir".into(),
        child_dir.display().to_string(),
        "daemon".into(),
        "--port".into(),
        "0".into(),
        "--no-update-check".into(),
    ]
}

#[test]
fn a_service_is_started_logged_and_stopped_with_the_daemon() {
    let tickets = tempfile::tempdir().unwrap();
    let logs = tempfile::tempdir().unwrap();
    let child_dir = tempfile::tempdir().unwrap();
    install(
        tickets.path(),
        "idle",
        "service",
        BIN,
        &child_daemon_args(child_dir.path()),
        "",
    );
    enable(tickets.path(), &["idle"]);
    let mut h = start_daemon(tickets.path(), logs.path());
    assert!(h.wait_state("idle", "running"), "{}", h.plugin("idle"));
    let info = h.plugin("idle");
    assert!(info["pid"].as_u64().unwrap() > 0);
    assert_eq!(info["restarts"], 0);
    assert!(
        wait_for(|| h
            .log_text()
            .contains("[plugin:idle] listening on http://127.0.0.1:")),
        "{}",
        h.log_text()
    );
    let started = h.events("plugin.started");
    assert_eq!(started.len(), 1);
    assert_eq!(started[0]["data"]["name"], "idle");
    let lock = child_dir.path().join(".simpletickets/daemon.lock");
    assert!(wait_for(|| lock.exists()));

    let log = h.log.clone();
    h.api.running.take().unwrap().stop();
    let text = std::fs::read_to_string(log).unwrap();
    assert!(
        text.contains("plugin.stopped name=idle reason=shutdown"),
        "{text}"
    );
    assert!(
        text.find("plugin.stopped").unwrap() < text.rfind("stopped: lease released").unwrap(),
        "plugins stop before the daemon finishes: {text}"
    );
    if cfg!(unix) {
        assert!(!lock.exists(), "the plugin got a graceful termination");
    }
}

#[test]
fn a_crashing_service_is_restarted_with_backoff_and_then_marked_failed() {
    let tickets = tempfile::tempdir().unwrap();
    let logs = tempfile::tempdir().unwrap();
    install(
        tickets.path(),
        "flaky",
        "service",
        BIN,
        &["--no-such-flag".to_string()],
        "capabilities: [tickets.read]\n",
    );
    enable(tickets.path(), &["flaky"]);
    let h = start_daemon(tickets.path(), logs.path());
    assert!(h.wait_state("flaky", "failed"), "{}", h.plugin("flaky"));
    let info = h.plugin("flaky");
    assert_eq!(info["crashes"], 3);
    assert_eq!(info["restarts"], 2);
    assert_eq!(info["last_exit"], "exit code 2");
    assert_eq!(info["pid"], Value::Null);
    assert_eq!(info["capabilities"][0], "tickets.read");

    assert_eq!(h.events("plugin.started").len(), 3);
    let crashed = h.events("plugin.crashed");
    assert_eq!(crashed.len(), 3);
    assert_eq!(crashed[0]["data"]["will_restart"], true);
    assert_eq!(crashed[1]["data"]["crashes"], 2);
    assert_eq!(crashed[2]["data"]["will_restart"], false);
    let failed = h.events("plugin.failed");
    assert_eq!(failed.len(), 1);
    assert_eq!(failed[0]["data"]["name"], "flaky");
    assert!(failed[0]["data"]["reason"]
        .as_str()
        .unwrap()
        .contains("3 crashes"));
    let text = h.log_text();
    assert!(text.contains("[plugin:flaky] (stderr) "), "{text}");
}

#[test]
fn disabling_stops_a_plugin_and_enabling_starts_it_again() {
    let tickets = tempfile::tempdir().unwrap();
    let logs = tempfile::tempdir().unwrap();
    let child_dir = tempfile::tempdir().unwrap();
    install(
        tickets.path(),
        "idle",
        "service",
        BIN,
        &child_daemon_args(child_dir.path()),
        "",
    );
    enable(tickets.path(), &["idle"]);
    let h = start_daemon(tickets.path(), logs.path());
    assert!(h.wait_state("idle", "running"));

    enable(tickets.path(), &[]);
    assert_eq!(h.api.post("/v1/rescan", "").0, 200);
    assert!(wait_for(|| h.events("plugin.stopped").len() == 1));
    assert!(h.wait_state("idle", "disabled"), "{}", h.plugin("idle"));
    let stopped = h.events("plugin.stopped");
    assert_eq!(stopped.len(), 1);
    assert_eq!(stopped[0]["data"]["reason"], "disabled");
    assert_eq!(h.plugin("idle")["enabled"], false);

    enable(tickets.path(), &["idle"]);
    assert_eq!(h.api.post("/v1/rescan", "").0, 200);
    assert!(wait_for(|| h.events("plugin.started").len() == 2));
}

#[test]
fn a_plugin_that_cannot_start_is_reported_failed_without_crash_loops() {
    let tickets = tempfile::tempdir().unwrap();
    let logs = tempfile::tempdir().unwrap();
    install(
        tickets.path(),
        "needy",
        "service",
        BIN,
        &[],
        "env: [ST_TEST_SURELY_UNSET_VAR]\n",
    );
    install(
        tickets.path(),
        "ghost-cmd",
        "service",
        "no-such-program-xyz",
        &[],
        "",
    );
    enable(tickets.path(), &["needy", "ghost-cmd", "uninstalled"]);
    let h = start_daemon(tickets.path(), logs.path());
    assert!(h.wait_state("needy", "failed"));
    assert!(h.plugin("needy")["error"]
        .as_str()
        .unwrap()
        .contains("ST_TEST_SURELY_UNSET_VAR"));
    assert!(h.wait_state("ghost-cmd", "failed"));
    assert!(h.plugin("ghost-cmd")["error"]
        .as_str()
        .unwrap()
        .contains("not found"));
    assert!(h.wait_state("uninstalled", "failed"));
    assert_eq!(h.events("plugin.failed").len(), 3);
    assert!(h.events("plugin.started").is_empty());
}

#[test]
fn client_started_plugins_are_listed_but_never_run_by_the_daemon() {
    let tickets = tempfile::tempdir().unwrap();
    let logs = tempfile::tempdir().unwrap();
    install(tickets.path(), "mcp", "stdio-client", BIN, &[], "");
    install(tickets.path(), "off", "service", BIN, &[], "");
    enable(tickets.path(), &["mcp"]);
    let h = start_daemon(tickets.path(), logs.path());
    assert!(h.wait_state("mcp", "client"));
    assert!(h.wait_state("off", "disabled"));
    assert!(h.events("plugin.started").is_empty());
    let (status, _) = h.api.call("POST", "/v1/plugins", Some("{}"));
    assert_eq!(status, 405);
    let (status, _) = raw(h.api.addr, "GET", "/v1/plugins", None, None, None);
    assert_eq!(status, 401);
}

#[cfg(unix)]
#[test]
fn a_service_gets_connection_info_and_a_clean_environment() {
    let tickets = tempfile::tempdir().unwrap();
    let logs = tempfile::tempdir().unwrap();
    std::env::set_var("ST_TEST_PASSED_VAR", "passed");
    std::env::set_var("ST_TEST_WITHHELD_VAR", "withheld");
    let script = "echo \"url=$SIMPLETICKETS_URL\"; echo \"tokenfile=$SIMPLETICKETS_TOKEN_FILE\"; \
                  echo \"actor=$SIMPLETICKETS_ACTOR\"; echo \"token=$(printenv SIMPLETICKETS_TOKEN || echo unset)\"; \
                  echo \"passed=$(printenv ST_TEST_PASSED_VAR || echo unset)\"; \
                  echo \"withheld=$(printenv ST_TEST_WITHHELD_VAR || echo unset)\"; echo oops >&2; sleep 30";
    install(
        tickets.path(),
        "shell",
        "service",
        "sh",
        &["-c".to_string(), script.to_string()],
        "env: [ST_TEST_PASSED_VAR]\n",
    );
    enable(tickets.path(), &["shell"]);
    let h = start_daemon(tickets.path(), logs.path());
    assert!(
        wait_for(|| h.log_text().contains("[plugin:shell] (stderr) oops")),
        "{}",
        h.log_text()
    );
    let text = h.log_text();
    let url = format!("[plugin:shell] url=http://127.0.0.1:{}", h.api.addr.port());
    assert!(text.contains(&url), "{text}");
    let token_file = tickets
        .path()
        .canonicalize()
        .unwrap()
        .join(".simpletickets/token");
    assert!(
        text.contains(&format!("tokenfile={}", token_file.display())),
        "{text}"
    );
    assert!(text.contains("actor=plugin:shell"), "{text}");
    assert!(
        text.contains("token=unset"),
        "the token is not in the environment: {text}"
    );
    assert!(text.contains("passed=passed"), "{text}");
    assert!(text.contains("withheld=unset"), "{text}");
    let token = std::fs::read_to_string(token_file).unwrap();
    assert!(
        !text.contains(token.trim()),
        "the token never reaches the log"
    );
}

#[cfg(unix)]
#[test]
fn the_token_can_be_requested_in_the_environment() {
    let tickets = tempfile::tempdir().unwrap();
    let logs = tempfile::tempdir().unwrap();
    install(
        tickets.path(),
        "envtoken",
        "service",
        "sh",
        &[
            "-c".to_string(),
            "test -n \"$SIMPLETICKETS_TOKEN\" && echo has-token; sleep 30".to_string(),
        ],
        "token: env\n",
    );
    enable(tickets.path(), &["envtoken"]);
    let h = start_daemon(tickets.path(), logs.path());
    assert!(
        wait_for(|| h.log_text().contains("[plugin:envtoken] has-token")),
        "{}",
        h.log_text()
    );
}

#[test]
fn plugins_status_shows_what_the_running_daemon_reports() {
    let tickets = tempfile::tempdir().unwrap();
    let logs = tempfile::tempdir().unwrap();
    let child_dir = tempfile::tempdir().unwrap();
    install(
        tickets.path(),
        "idle",
        "service",
        BIN,
        &child_daemon_args(child_dir.path()),
        "",
    );
    enable(tickets.path(), &["idle"]);
    let h = start_daemon(tickets.path(), logs.path());
    assert!(h.wait_state("idle", "running"));
    let out = std::process::Command::new(BIN)
        .env("SIMPLETICKETS_PLUGINS_DIR", logs.path())
        .arg("--dir")
        .arg(tickets.path())
        .args(["plugins", "status", "--json", "--url"])
        .arg(format!("http://{}", h.api.addr))
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["daemon"][0]["state"], "running");
    assert!(v["daemon"][0]["pid"].as_u64().unwrap() > 0);
    let text = std::process::Command::new(BIN)
        .env("SIMPLETICKETS_PLUGINS_DIR", logs.path())
        .arg("--dir")
        .arg(tickets.path())
        .args(["plugins", "status", "--url"])
        .arg(format!("http://{}", h.api.addr))
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&text.stdout).contains("daemon: running pid="));
}
