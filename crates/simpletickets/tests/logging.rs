mod common;

use common::*;
use simpletickets::logger::{Level, Logger};
use std::sync::Arc;
use std::time::Duration;

fn ticket(id: &str, status: &str, extra: &str) -> String {
    format!("---\nid: {id}\ntitle: Ticket {id}\nstatus: {status}\n{extra}---\nbody\n")
}

/// The rules leave freshly modified files alone, so make the parents look settled.
fn age(path: &std::path::Path) {
    let when = std::time::SystemTime::now() - Duration::from_secs(60);
    let file = std::fs::File::options().write(true).open(path).unwrap();
    file.set_modified(when).unwrap();
}

fn run_with_level(level: Level) -> (String, String) {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "simpletickets.yml",
        "tag_logging:\n  urgent: warn\n",
    );
    write(
        dir.path(),
        "P-1.md",
        &ticket("P-1", "todo", "tags: [urgent]\n"),
    );
    write(
        dir.path(),
        "C-1.md",
        &ticket("C-1", "in-progress", "parent: P-1\n"),
    );
    write(dir.path(), "P-2.md", &ticket("P-2", "todo", ""));
    write(
        dir.path(),
        "C-2.md",
        &ticket("C-2", "in-progress", "parent: P-2\n"),
    );
    for name in ["P-1.md", "P-2.md"] {
        age(&dir.path().join(name));
    }
    let log_path = dir.path().join("logs/daemon.log");
    let logger = Arc::new(Logger::file(level, &log_path).unwrap());
    let api = start_with(dir.path(), false, Duration::from_secs(3600), |o| {
        o.logger = logger;
    });
    let token = api.token.clone();
    drop(api);
    (std::fs::read_to_string(&log_path).unwrap(), token)
}

#[test]
fn rule_applications_log_at_the_tag_level_and_never_leak_the_token() {
    let (text, token) = run_with_level(Level::Info);
    let line = |needle: &str| text.lines().find(|l| l.contains(needle)).unwrap_or("");
    assert!(line("id=P-1").contains(" WARN "), "{text}");
    assert!(line("id=P-2").contains(" INFO "), "{text}");
    assert!(text.contains("starting version="), "{text}");
    assert!(text.contains("listening addr="), "{text}");
    assert!(text.contains("scan reason=startup"), "{text}");
    assert!(text.contains("stopped: lease released"), "{text}");
    assert!(!text.contains(&token), "token leaked into the log");
}

#[test]
fn log_level_filters_untagged_rule_applications() {
    let (text, _) = run_with_level(Level::Warn);
    assert!(text.contains("id=P-1"), "{text}");
    assert!(!text.contains("id=P-2"), "{text}");
    assert!(!text.contains("listening"), "{text}");
}
