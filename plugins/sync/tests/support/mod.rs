#![allow(dead_code)]
//! A live in-process daemon, the archive connector binary and a sync configuration.

use serde_json::{json, Value};
use simpletickets::daemon::{self, Options, Running};
use simpletickets::logger::{Level, Logger};
use simpletickets_sync::api::Api;
use simpletickets_sync::config::Config;
use simpletickets_sync::engine::{Engine, Options as Pass, Report};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

/// The path of a workspace binary, building it first when `cargo test` has not.
pub fn binary(package: &str) -> PathBuf {
    let exe = std::env::current_exe().unwrap();
    let profile_dir = exe.parent().unwrap().parent().unwrap().to_path_buf();
    let path = profile_dir.join(format!("{package}{}", std::env::consts::EXE_SUFFIX));
    if !path.exists() {
        let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
        let mut command = Command::new(cargo);
        command.args(["build", "-p", package]);
        if profile_dir.file_name().is_some_and(|n| n == "release") {
            command.arg("--release");
        }
        assert!(
            command.status().unwrap().success(),
            "cannot build {package}"
        );
    }
    path
}

pub struct Env {
    pub running: Option<Running>,
    /// Acts as the sync plugin does.
    pub api: Api,
    /// Acts as someone else, whose writes the plugin must react to.
    pub user: Api,
    pub dir: PathBuf,
    pub archive: PathBuf,
    pub url: String,
    /// Declared last so the folders are removed after the daemon has stopped.
    _folder: tempfile::TempDir,
}

impl Drop for Env {
    fn drop(&mut self) {
        if let Some(r) = self.running.take() {
            r.stop();
        }
    }
}

pub fn start(daemon_config: impl FnOnce(&mut Options)) -> Env {
    let folder = tempfile::tempdir().unwrap();
    let dir = folder.path().join("tickets");
    std::fs::create_dir_all(&dir).unwrap();
    let archive = folder.path().join("archive");
    let mut options = Options::new(dir.clone());
    options.port = 0;
    options.watch = false;
    options.scan_interval = Duration::from_secs(3600);
    options.user_plugins_dir = None;
    options.logger =
        Arc::new(Logger::file(Level::Info, &folder.path().join("daemon.log")).unwrap());
    daemon_config(&mut options);
    let running = daemon::start(options).expect("daemon starts");
    let token = std::fs::read_to_string(running.token_file()).unwrap();
    let url = format!("http://{}", running.addr);
    Env {
        api: Api::new(&url, &token, "plugin:sync").unwrap(),
        user: Api::new(&url, &token, "test-user").unwrap(),
        running: Some(running),
        dir,
        archive,
        url,
        _folder: folder,
    }
}

impl Env {
    pub fn token_file(&self) -> PathBuf {
        self.running.as_ref().unwrap().token_file().to_path_buf()
    }

    pub fn config_yaml(&self, extra: &str) -> String {
        format!(
            "targets:\n  archive:\n    connector:\n      command: '{}'\n      timeout_seconds: 30\n    options:\n      dir: '{}'\n    unmapped: pass\n{extra}",
            binary("simpletickets-connector-archive").display(),
            self.archive.display()
        )
    }

    pub fn write_config(&self, extra: &str) -> Config {
        let yaml = self.config_yaml(extra);
        let path = simpletickets_sync::state::config_path(&self.dir);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &yaml).unwrap();
        Config::parse(&yaml).unwrap()
    }

    pub fn create(&self, body: Value) -> String {
        let (status, ticket) = self
            .user
            .request("POST", "/v1/tickets", &[], Some(&body))
            .unwrap();
        assert_eq!(status, 201, "{ticket}");
        ticket["id"].as_str().unwrap().to_string()
    }

    pub fn create_titled(&self, title: &str) -> String {
        self.create(json!({"title": title}))
    }

    pub fn patch(&self, id: &str, body: Value) {
        let (status, reply) = self
            .user
            .request("PATCH", &format!("/v1/tickets/{id}"), &[], Some(&body))
            .unwrap();
        assert_eq!(status, 200, "{reply}");
    }

    pub fn ticket(&self, id: &str) -> Value {
        self.api.get_ticket(id).unwrap().unwrap()
    }

    pub fn rescan(&self) {
        let (status, _) = self.user.request("POST", "/v1/rescan", &[], None).unwrap();
        assert_eq!(status, 200);
    }

    /// One pass with a fresh engine (a fresh connector process), as after a restart.
    pub fn pass(&self, config: &Config, options: &Pass) -> Report {
        Engine::new(&self.api, config, &self.dir, None)
            .run(options)
            .unwrap()
    }

    pub fn full(&self, config: &Config) -> Report {
        self.pass(
            config,
            &Pass {
                pull: true,
                ..Pass::default()
            },
        )
    }

    pub fn archive_file(&self, name: &str) -> Value {
        let text = std::fs::read_to_string(self.archive.join(name)).unwrap();
        serde_json::from_str(&text).unwrap()
    }

    pub fn edit_archive_status(&self, name: &str, status: &str) {
        let mut doc = self.archive_file(name);
        doc["fields"]["status"] = json!(status);
        std::fs::write(
            self.archive.join(name),
            serde_json::to_string_pretty(&doc).unwrap(),
        )
        .unwrap();
    }

    pub fn state(&self) -> Value {
        let path = Path::new(&self.dir).join(".simpletickets/sync/archive.json");
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }
}

pub fn kinds(report: &Report) -> Vec<String> {
    report
        .actions
        .iter()
        .map(|a| format!("{} {}", a.kind, a.id))
        .collect()
}
