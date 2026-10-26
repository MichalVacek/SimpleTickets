mod common;

use common::*;
use simpletickets::update::{Fetcher, Release, UpdateCheck};
use std::sync::Arc;
use std::time::Duration;

const SLOW: Duration = Duration::from_secs(3600);

struct Fake(Result<Release, String>);

impl Fetcher for Fake {
    fn latest(&self) -> Result<Release, String> {
        self.0.clone()
    }
}

fn check(result: Result<Release, String>) -> UpdateCheck {
    UpdateCheck {
        fetcher: Arc::new(Fake(result)),
        interval: SLOW,
    }
}

fn release(tag: &str) -> Release {
    Release {
        tag: tag.into(),
        url: "https://example.test/release".into(),
    }
}

fn update_events(api: &Api) -> Vec<serde_json::Value> {
    let (_, body) = api.get("/v1/events?since=0");
    body["events"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["type"] == "update.available")
        .cloned()
        .collect()
}

#[test]
fn newer_release_is_reported_in_health_and_events() {
    let dir = tempfile::tempdir().unwrap();
    let api = start_with(dir.path(), false, SLOW, |o| {
        o.update_check = Some(check(Ok(release("v99.0.0"))));
    });
    assert!(wait_for(|| !api.get("/v1/health").1["update"].is_null()));
    let health = api.get("/v1/health").1;
    assert_eq!(health["update"]["latest"], "v99.0.0");
    assert_eq!(health["update"]["url"], "https://example.test/release");
    let events = update_events(&api);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["data"]["latest"], "v99.0.0");
}

#[test]
fn same_or_older_release_is_not_reported() {
    let dir = tempfile::tempdir().unwrap();
    let api = start_with(dir.path(), false, SLOW, |o| {
        o.update_check = Some(check(Ok(release("v0.0.1"))));
    });
    std::thread::sleep(Duration::from_millis(500));
    assert!(api.get("/v1/health").1["update"].is_null());
    assert!(update_events(&api).is_empty());
}

#[test]
fn offline_fetch_fails_quietly() {
    let dir = tempfile::tempdir().unwrap();
    let api = start_with(dir.path(), false, SLOW, |o| {
        o.update_check = Some(check(Err("network unreachable".into())));
    });
    std::thread::sleep(Duration::from_millis(500));
    let (status, health) = api.get("/v1/health");
    assert_eq!(status, 200);
    assert!(health["update"].is_null());
    assert!(update_events(&api).is_empty());
}

#[test]
fn disabled_by_default() {
    let dir = tempfile::tempdir().unwrap();
    let api = start(dir.path(), false, SLOW);
    assert!(api.get("/v1/health").1["update"].is_null());
}

#[test]
fn repeated_checks_announce_a_release_once() {
    let dir = tempfile::tempdir().unwrap();
    let api = start_with(dir.path(), false, SLOW, |o| {
        let mut c = check(Ok(release("v99.0.0")));
        c.interval = Duration::from_millis(50);
        o.update_check = Some(c);
    });
    assert!(wait_for(|| !api.get("/v1/health").1["update"].is_null()));
    std::thread::sleep(Duration::from_millis(400));
    assert_eq!(update_events(&api).len(), 1);
}
