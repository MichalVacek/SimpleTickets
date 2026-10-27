//! Runs the real connector binary through the protocol client, as the sync plugin does.

use serde_json::json;
use simpletickets_connector_protocol::{ErrorCode, Process, Spawn, Transport};
use std::time::Duration;

fn spawn() -> Process {
    Process::spawn(&Spawn {
        command: env!("CARGO_BIN_EXE_simpletickets-connector-archive").to_string(),
        args: vec![],
        pass_env: vec![],
        timeout: Duration::from_secs(20),
    })
    .unwrap()
}

#[test]
fn the_binary_speaks_the_protocol_over_stdio() {
    let dir = tempfile::tempdir().unwrap();
    let mut connector = spawn();
    let description = connector.call("describe", json!({})).unwrap();
    assert_eq!(description["name"], "archive");
    assert_eq!(description["protocol"], json!([1]));

    let configured = connector
        .call(
            "configure",
            json!({"options": {"dir": dir.path().to_str().unwrap()}}),
        )
        .unwrap();
    assert_eq!(configured["capabilities"], json!(["close", "fetch"]));

    let pushed = connector
        .call(
            "push",
            json!({"ticket": {"id": "ST-1", "title": "One"}, "fields": {"status": "todo"}, "ref": null}),
        )
        .unwrap();
    assert_eq!(pushed["outcome"], "created");
    assert!(dir.path().join("ST-1.json").is_file());

    let error = connector.call("nonsense", json!({})).unwrap_err();
    assert_eq!(error.code, ErrorCode::Unsupported);
    assert!(connector.is_alive());
    connector.close();
}

#[test]
fn a_connector_that_exits_is_reported_unavailable() {
    let mut connector = spawn();
    connector.call("shutdown", json!({})).unwrap();
    let error = connector.call("describe", json!({})).unwrap_err();
    assert_eq!(error.code, ErrorCode::Unavailable);
    assert!(!connector.is_alive());
}
