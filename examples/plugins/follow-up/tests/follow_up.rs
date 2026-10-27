//! Runs the example plugin the way a user would: enabled in the config, started and
//! supervised by a real daemon, reacting to changes made through the API.

use serde_json::{json, Value};
use simpletickets::daemon::{self, Options, Running};
use simpletickets::logger::{Level, Logger};
use simpletickets_follow_up::Client;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

const PLUGIN: &str = env!("CARGO_BIN_EXE_simpletickets-follow-up");

struct Setup {
    running: Option<Running>,
    client: Client,
    dir: std::path::PathBuf,
    /// Declared last so the folder is removed after the daemon has stopped.
    _folder: tempfile::TempDir,
}

impl Setup {
    fn get(&self, path: &str) -> Value {
        self.client.get(path).unwrap().1
    }

    fn tickets(&self) -> Vec<Value> {
        self.get("/v1/tickets").as_array().unwrap().clone()
    }

    fn follow_ups(&self) -> Vec<Value> {
        self.tickets()
            .into_iter()
            .filter(|t| t["title"].as_str().unwrap().starts_with("Follow up: "))
            .collect()
    }

    fn done(&self, id: &str) {
        let (status, _) = self
            .client
            .request(
                "PATCH",
                &format!("/v1/tickets/{id}"),
                &[],
                Some(&json!({"status": "done"})),
            )
            .unwrap();
        assert_eq!(status, 200);
    }

    fn create(&self, title: &str) -> String {
        let (status, ticket) = self
            .client
            .request("POST", "/v1/tickets", &[], Some(&json!({"title": title})))
            .unwrap();
        assert_eq!(status, 201);
        ticket["id"].as_str().unwrap().to_string()
    }

    fn enable_plugin(&self) {
        std::fs::write(
            self.dir.join("simpletickets.yml"),
            "plugins:\n  enabled: [follow-up]\n",
        )
        .unwrap();
        let (status, _) = self
            .client
            .request("POST", "/v1/rescan", &[], None)
            .unwrap();
        assert_eq!(status, 200);
    }

    fn plugin_state(&self) -> String {
        self.get("/v1/plugins")
            .as_array()
            .unwrap()
            .iter()
            .find(|p| p["name"] == "follow-up")
            .map_or(String::new(), |p| p["state"].as_str().unwrap().to_string())
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        if let Some(r) = self.running.take() {
            r.stop();
        }
    }
}

fn wait_for(mut condition: impl FnMut() -> bool) -> bool {
    for _ in 0..150 {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}

/// A daemon on a fresh tickets folder with the example plugin installed in it.
fn setup(logs: &Path, enabled: bool) -> Setup {
    let folder = tempfile::tempdir().unwrap();
    let dir = folder.path().to_path_buf();
    let plugin = dir.join(".simpletickets/plugins/follow-up");
    std::fs::create_dir_all(&plugin).unwrap();
    std::fs::write(
        plugin.join("plugin.yml"),
        format!(
            "name: follow-up\nversion: 0.1.0\napi_version: 1\nkind: service\n\
             command: '{PLUGIN}'\ncapabilities: [events, tickets.write]\n"
        ),
    )
    .unwrap();
    if enabled {
        std::fs::write(
            dir.join("simpletickets.yml"),
            "plugins:\n  enabled: [follow-up]\n",
        )
        .unwrap();
    }
    let mut options = Options::new(dir.clone());
    options.port = 0;
    options.watch = false;
    options.scan_interval = Duration::from_secs(3600);
    options.user_plugins_dir = None;
    options.logger = Arc::new(Logger::file(Level::Info, &logs.join("daemon.log")).unwrap());
    let running = daemon::start(options).expect("daemon starts");
    let token = std::fs::read_to_string(running.token_file()).unwrap();
    let url = format!("http://{}", running.addr);
    Setup {
        client: Client::new(&url, &token, "test-suite").unwrap(),
        running: Some(running),
        dir,
        _folder: folder,
    }
}

#[test]
fn finishing_a_ticket_creates_one_follow_up_as_the_plugin() {
    let logs = tempfile::tempdir().unwrap();
    let s = setup(logs.path(), true);
    assert!(wait_for(|| s.plugin_state() == "running"));
    let id = s.create("Ship release");
    assert_eq!(s.follow_ups().len(), 0);
    s.done(&id);
    assert!(wait_for(|| s.follow_ups().len() == 1));

    let follow_up = s.follow_ups().remove(0);
    assert_eq!(follow_up["title"], "Follow up: Ship release");
    assert_eq!(follow_up["related"], json!([id]));
    assert_eq!(follow_up["tags"], json!(["follow-up"]));
    assert_eq!(follow_up["status"], "todo");

    // The write is attributed to the plugin in the audit log and in the event.
    let audit = s.get("/v1/audit?actor=plugin%3Afollow-up");
    let audit = audit.as_array().unwrap();
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0]["rule"], "api.create");
    assert_eq!(audit[0]["id"], follow_up["id"]);
    let events = s.get("/v1/events");
    let created: Vec<&Value> = events["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "ticket.created" && e["data"]["id"] == follow_up["id"])
        .collect();
    assert_eq!(created.len(), 1);
    assert_eq!(created[0]["data"]["actor"], "plugin:follow-up");
    assert_eq!(created[0]["data"]["source"], "api");

    // Reopening and finishing the ticket again does not create a second follow-up, and
    // finishing the follow-up does not start a chain.
    s.client
        .request(
            "PATCH",
            &format!("/v1/tickets/{id}"),
            &[],
            Some(&json!({"status": "todo"})),
        )
        .unwrap();
    s.done(&id);
    s.done(follow_up["id"].as_str().unwrap());
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(s.tickets().len(), 2);

    let log = std::fs::read_to_string(logs.path().join("daemon.log")).unwrap();
    assert!(
        log.contains("[plugin:follow-up] following events as plugin:follow-up"),
        "{log}"
    );
    assert!(
        log.contains(&format!(
            "[plugin:follow-up] created {} for {id}",
            follow_up["id"].as_str().unwrap()
        )),
        "{log}"
    );
}

#[test]
fn a_plugin_enabled_later_catches_up_from_the_event_backlog_without_duplicates() {
    let logs = tempfile::tempdir().unwrap();
    let s = setup(logs.path(), false);
    let first = s.create("First");
    let second = s.create("Second");
    s.done(&first);
    s.done(&second);
    assert_eq!(s.plugin_state(), "disabled");

    s.enable_plugin();
    assert!(wait_for(|| s.follow_ups().len() == 2));
    let mut titles: Vec<String> = s
        .follow_ups()
        .iter()
        .map(|t| t["title"].as_str().unwrap().to_string())
        .collect();
    titles.sort();
    assert_eq!(titles, ["Follow up: First", "Follow up: Second"]);

    // Restarting the plugin replays the backlog; the stored answers absorb every repeat.
    s.client.request("POST", "/v1/rescan", &[], None).unwrap();
    let before = s.tickets().len();
    std::fs::write(s.dir.join("simpletickets.yml"), "plugins:\n  enabled: []\n").unwrap();
    s.client.request("POST", "/v1/rescan", &[], None).unwrap();
    assert!(wait_for(|| s.plugin_state() == "disabled"));
    s.enable_plugin();
    assert!(wait_for(|| s.plugin_state() == "running"));
    std::thread::sleep(Duration::from_millis(1500));
    assert_eq!(s.tickets().len(), before);
}
