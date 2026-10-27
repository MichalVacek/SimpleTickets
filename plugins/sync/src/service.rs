//! The long-running service: a full reconcile at start, then the event stream for prompt
//! pushes, a periodic reconcile for whatever the stream missed, and a periodic pull.

use crate::api::{Api, POLL_WAIT};
use crate::config::{Config, Direction};
use crate::engine::{Engine, Options, Prefer, Report};
use crate::state::{self, Lock};
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const EVENT_TYPES: [&str; 4] = [
    "ticket.created",
    "ticket.updated",
    "ticket.status_changed",
    "ticket.deleted",
];

/// The ticket ids named by the events that can change what a remote should hold. Events
/// written by this plugin itself are skipped: its write-backs are already recorded.
pub fn affected_ids(events: &[serde_json::Value], own_actor: &str) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    for event in events {
        let kind = event["type"].as_str().unwrap_or_default();
        let data = &event["data"];
        if kind == "ticket.id_reassigned" {
            ids.extend(
                ["old_id", "new_id"]
                    .iter()
                    .filter_map(|k| data[*k].as_str().map(String::from)),
            );
        } else if EVENT_TYPES.contains(&kind) && data["actor"].as_str() != Some(own_actor) {
            ids.extend(data["id"].as_str().map(String::from));
        }
    }
    ids
}

fn print_report(label: &str, report: &Report) {
    for (target, message) in &report.target_errors {
        eprintln!("sync: {target}: {message}");
    }
    for a in &report.actions {
        match a.kind.as_str() {
            "error" | "warn" | "conflict" => {
                eprintln!("sync: {} {} {}: {}", a.target, a.id, a.kind, a.detail)
            }
            _ => println!("{} {} {}: {}", a.target, a.id, a.kind, a.detail),
        }
    }
    if !report.actions.is_empty() || !report.target_errors.is_empty() {
        println!("{label}: {} change(s)", report.actions.len());
    }
}

/// Runs until `stop` is set. `Err` only for conditions that make running pointless.
pub fn run(
    api: &Api,
    config: &Config,
    tickets_dir: &Path,
    stop: &AtomicBool,
    supervised: bool,
) -> Result<(), String> {
    let lock = if supervised {
        Lock::take_over(tickets_dir)?
    } else {
        Lock::acquire(tickets_dir)?
    };
    let mut engine = Engine::new(api, config, tickets_dir, Some(&lock));
    let reconcile_every = Duration::from_secs(config.reconcile_interval_seconds);
    let pull_every = Duration::from_secs(config.pull_interval_seconds);
    let pulls = config
        .targets
        .values()
        .any(|t| t.direction == Direction::Both);
    println!(
        "syncing {} target(s) as {}",
        config.targets.len(),
        api.actor
    );

    let full = Options {
        pull: true,
        ..Options::default()
    };
    // The cursor is taken before the first reconcile, so changes made during it are seen.
    let mut cursor: Option<(u64, String)> = None;
    while cursor.is_none() && !stop.load(Ordering::Relaxed) {
        match api.events(None, 0) {
            Ok(page) => cursor = Some((page.next, page.stream)),
            Err(e) => {
                eprintln!("sync: waiting for the daemon: {e}");
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    }
    let mut last_full = Instant::now();
    let mut last_pull = Instant::now();
    match engine.run(&full) {
        Ok(report) => print_report("reconcile", &report),
        Err(e) => eprintln!("sync: reconcile failed: {e}"),
    }

    while !stop.load(Ordering::Relaxed) {
        lock.touch();
        let mut options: Option<Options> = None;
        if let Some(request) = state::take_request(tickets_dir) {
            options = Some(Options {
                force: request.force,
                only_ids: (!request.ids.is_empty()).then(|| request.ids.into_iter().collect()),
                only_target: request.target,
                pull: true,
                prefer: match request.prefer.as_deref() {
                    Some("local") => Some(Prefer::Local),
                    Some("remote") => Some(Prefer::Remote),
                    _ => None,
                },
                ..Options::default()
            });
        }
        let since = cursor.as_ref().map(|(n, s)| (*n, s.as_str()));
        match api.events(since, POLL_WAIT) {
            Ok(page) => {
                if page.gap {
                    // Events were lost: take the new cursor first, then reload everything.
                    options = Some(full.clone());
                } else if options.is_none() {
                    let ids = affected_ids(&page.events, &api.actor);
                    if !ids.is_empty() {
                        options = Some(Options {
                            only_ids: Some(ids),
                            ..Options::default()
                        });
                    }
                }
                cursor = Some((page.next, page.stream));
            }
            Err(e) => {
                eprintln!("sync: events: {e}");
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        }
        if last_full.elapsed() >= reconcile_every {
            options = Some(full.clone());
        } else if options.is_none() && pulls && last_pull.elapsed() >= pull_every {
            options = Some(Options {
                pull: true,
                pull_only: true,
                ..Options::default()
            });
        }
        let Some(options) = options else { continue };
        if options.only_ids.is_none() && !options.pull_only {
            last_full = Instant::now();
        }
        if options.pull {
            last_pull = Instant::now();
        }
        match engine.run(&options) {
            Ok(report) => print_report("pass", &report),
            Err(e) => eprintln!("sync: pass failed: {e}"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn collects_the_tickets_named_by_relevant_events() {
        let events = vec![
            json!({"type": "ticket.created", "data": {"id": "ST-1"}}),
            json!({"type": "ticket.status_changed", "data": {"id": "ST-2", "actor": "someone"}}),
            json!({"type": "ticket.updated", "data": {"id": "ST-3", "actor": "plugin:sync"}}),
            json!({"type": "ticket.deleted", "data": {"id": "ST-4"}}),
            json!({"type": "ticket.moved", "data": {"id": "ST-5"}}),
            json!({"type": "scan", "data": {}}),
            json!({"type": "ticket.id_reassigned", "data": {"old_id": "ST-6", "new_id": "ST-7", "actor": "x"}}),
        ];
        let ids: Vec<String> = affected_ids(&events, "plugin:sync").into_iter().collect();
        assert_eq!(ids, ["ST-1", "ST-2", "ST-4", "ST-6", "ST-7"]);
    }
}
