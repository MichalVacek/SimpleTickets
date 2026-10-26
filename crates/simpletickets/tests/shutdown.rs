#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::Duration;

fn run_and_signal(signal: &str) {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("daemon.log");
    let mut child = Command::new(env!("CARGO_BIN_EXE_simpletickets"))
        .arg("--dir")
        .arg(dir.path())
        .args(["daemon", "--port", "0", "--no-update-check", "--log-file"])
        .arg(&log)
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let banner = lines.next().unwrap().unwrap();
    assert!(banner.starts_with("listening on http://"), "{banner}");
    let lock = dir.path().join(".simpletickets/daemon.lock");
    assert!(lock.exists(), "daemon holds the lease while running");

    let sent = Command::new("kill")
        .args([signal, &child.id().to_string()])
        .status()
        .unwrap();
    assert!(sent.success());

    let mut status = None;
    for _ in 0..100 {
        if let Some(s) = child.try_wait().unwrap() {
            status = Some(s);
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let Some(status) = status else {
        let _ = child.kill();
        panic!("daemon did not exit after {signal}");
    };
    assert!(status.success(), "{status:?}");
    assert!(!lock.exists(), "lease is released");
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(text.contains("shutdown requested"), "{text}");
    assert!(text.contains("stopped: lease released"), "{text}");
}

#[test]
fn sigterm_stops_the_daemon_cleanly() {
    run_and_signal("-TERM");
}

#[test]
fn sigint_stops_the_daemon_cleanly() {
    run_and_signal("-INT");
}
