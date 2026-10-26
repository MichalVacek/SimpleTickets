use serde_json::Value;
use std::path::Path;
use std::process::{Command, Output};

fn st(dir: &Path, user: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_simpletickets"))
        .env("SIMPLETICKETS_PLUGINS_DIR", user)
        .arg("--dir")
        .arg(dir)
        .arg("plugins")
        .args(args)
        .output()
        .unwrap()
}

fn text(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn install(root: &Path, name: &str, manifest: &str) {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("plugin.yml"), manifest).unwrap();
}

/// A service manifest whose command is this test's own binary, so it always exists.
fn manifest(name: &str, extra: &str) -> String {
    format!(
        "name: {name}\nversion: 0.2.0\ndescription: Demo {name}\napi_version: 1\n\
         kind: service\ncommand: '{}'\n{extra}",
        env!("CARGO_BIN_EXE_simpletickets")
    )
}

#[test]
fn lists_enables_and_disables_plugins() {
    let tickets = tempfile::tempdir().unwrap();
    let user = tempfile::tempdir().unwrap();
    let local = tickets.path().join(".simpletickets/plugins");
    install(&local, "alpha", &manifest("alpha", ""));
    install(user.path(), "beta", &manifest("beta", ""));
    install(user.path(), "broken", "name: [");

    let out = st(tickets.path(), user.path(), &["list", "--json"]);
    let rows: Vec<Value> = serde_json::from_slice(&out.stdout).unwrap();
    let names: Vec<&str> = rows.iter().map(|r| r["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["alpha", "beta", "broken"]);
    assert_eq!(rows[0]["source"], "tickets");
    assert_eq!(rows[1]["source"], "user");
    assert_eq!(rows[2]["valid"], false);
    assert!(rows.iter().all(|r| r["enabled"] == false));

    let out = st(tickets.path(), user.path(), &["enable", "alpha"]);
    assert!(out.status.success(), "{}", text(&out));
    let config = std::fs::read_to_string(tickets.path().join("simpletickets.yml")).unwrap();
    assert!(config.contains("- alpha"), "{config}");

    let again = st(tickets.path(), user.path(), &["enable", "alpha"]);
    assert!(text(&again).contains("already enabled"));
    assert!(st(tickets.path(), user.path(), &["enable", "beta"])
        .status
        .success());

    let listed = text(&st(tickets.path(), user.path(), &["list"]));
    let alpha = listed.lines().find(|l| l.starts_with("alpha")).unwrap();
    assert!(
        alpha.contains("service") && alpha.contains("enabled"),
        "{listed}"
    );
    let broken = listed.lines().find(|l| l.starts_with("broken")).unwrap();
    assert!(broken.contains("disabled"));

    assert!(st(tickets.path(), user.path(), &["disable", "alpha"])
        .status
        .success());
    let config = std::fs::read_to_string(tickets.path().join("simpletickets.yml")).unwrap();
    assert!(
        !config.contains("alpha") && config.contains("- beta"),
        "{config}"
    );
    assert!(
        text(&st(tickets.path(), user.path(), &["disable", "alpha"])).contains("already disabled")
    );
}

#[test]
fn refuses_unknown_and_invalid_plugins() {
    let tickets = tempfile::tempdir().unwrap();
    let user = tempfile::tempdir().unwrap();
    install(user.path(), "broken", "name: [");
    let unknown = st(tickets.path(), user.path(), &["enable", "ghost"]);
    assert!(!unknown.status.success());
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("no plugin named"));
    let broken = st(tickets.path(), user.path(), &["enable", "broken"]);
    assert!(!broken.status.success());
    assert!(String::from_utf8_lossy(&broken.stderr).contains("invalid manifest"));
    assert!(!tickets.path().join("simpletickets.yml").exists());
}

#[test]
fn status_reports_what_blocks_an_enabled_plugin() {
    let tickets = tempfile::tempdir().unwrap();
    let user = tempfile::tempdir().unwrap();
    let local = tickets.path().join(".simpletickets/plugins");
    install(&local, "good", &manifest("good", ""));
    install(
        &local,
        "needs-env",
        &manifest("needs-env", "env: [ST_TEST_SURELY_UNSET_VAR]\n"),
    );
    install(
        &local,
        "future",
        &manifest("future", "").replace("api_version: 1", "api_version: 2-"),
    );
    std::fs::write(
        tickets.path().join("simpletickets.yml"),
        "plugins:\n  enabled: [good, needs-env, future, ghost]\n",
    )
    .unwrap();
    let out = st(tickets.path(), user.path(), &["status", "--json"]);
    assert!(!out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let rows = v["plugins"].as_array().unwrap();
    let by = |name: &str| rows.iter().find(|r| r["name"] == name).unwrap();
    assert!(by("good")["problem"].is_null());
    assert!(by("needs-env")["problem"]
        .as_str()
        .unwrap()
        .contains("ST_TEST_SURELY_UNSET_VAR"));
    assert!(by("future")["problem"]
        .as_str()
        .unwrap()
        .contains("API version"));
    assert_eq!(v["missing"][0], "ghost");

    std::fs::write(
        tickets.path().join("simpletickets.yml"),
        "plugins:\n  enabled: [good]\n",
    )
    .unwrap();
    let out = st(tickets.path(), user.path(), &["status"]);
    assert!(out.status.success());
    assert!(text(&out).contains("ready"), "{}", text(&out));
}

#[test]
fn the_mcp_manifest_in_the_repository_is_valid() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/mcp/plugin.yml");
    let m = simpletickets_core::plugin::Manifest::parse(&std::fs::read_to_string(path).unwrap())
        .unwrap();
    assert_eq!(m.name, "mcp");
    assert_eq!(m.kind, simpletickets_core::plugin::Kind::StdioClient);
    assert!(m.api_version.contains(1));
    assert_eq!(m.install.unwrap().binary, "simpletickets-mcp");
}
