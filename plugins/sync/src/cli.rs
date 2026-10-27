//! Command line of `simpletickets-sync`.

use crate::api::Api;
use crate::config::Config;
use crate::engine::{check_target, Engine, Options, Prefer, Report};
use crate::service;
use crate::state::{self, Lock, Phase, Request, TargetState};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

pub const USAGE: &str = "\
Usage: simpletickets-sync [--dir DIR] [--config FILE] [--url URL] [--token-file FILE] COMMAND

Commands:
  run                     follow the daemon's events and keep the targets in sync
  status [--json] [--check]
                          per target: counts, open conflicts, errors; --check also
                          asks each connector to verify its remote and credentials
  dry-run [--target T] [--json] [ID...]
                          show what a pass would do; changes nothing anywhere
  push-now [--target T] [--force] [--prefer local|remote] [ID...]
                          sync now (queued for the service when it is running);
                          --prefer settles the conflicts of the named tickets
  list-conflicts [--json] [--all]
                          conflicts waiting for a decision (--all adds resolved ones)

The folder defaults to $SIMPLETICKETS_DIR or the current folder; the configuration to
DIR/.simpletickets/sync/config.yml. Credentials come from environment variables only.";

struct Args {
    dir: Option<PathBuf>,
    config: Option<PathBuf>,
    url: Option<String>,
    token_file: Option<PathBuf>,
    command: String,
    flags: BTreeSet<String>,
    values: Vec<(String, String)>,
    positional: Vec<String>,
}

impl Args {
    fn value(&self, name: &str) -> Option<&str> {
        self.values
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

fn parse(args: &[String]) -> Result<Args, String> {
    let mut out = Args {
        dir: None,
        config: None,
        url: None,
        token_file: None,
        command: String::new(),
        flags: BTreeSet::new(),
        values: Vec::new(),
        positional: Vec::new(),
    };
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        let mut take = |name: &str| {
            iter.next()
                .cloned()
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match arg.as_str() {
            "--dir" => out.dir = Some(take("--dir")?.into()),
            "--config" => out.config = Some(take("--config")?.into()),
            "--url" => out.url = Some(take("--url")?),
            "--token-file" => out.token_file = Some(take("--token-file")?.into()),
            "--target" | "--prefer" => {
                let value = take(arg)?;
                out.values.push((arg.clone(), value));
            }
            "--json" | "--check" | "--force" | "--all" => {
                out.flags.insert(arg.clone());
            }
            "-h" | "--help" => {
                out.flags.insert("--help".into());
            }
            flag if flag.starts_with("--") => return Err(format!("unknown option {flag}")),
            word if out.command.is_empty() => out.command = word.to_string(),
            word => out.positional.push(word.to_string()),
        }
    }
    Ok(out)
}

fn fail(message: &str) -> ExitCode {
    eprintln!("simpletickets-sync: {message}");
    ExitCode::from(2)
}

pub fn main(args: Vec<String>) -> ExitCode {
    let args = match parse(&args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("simpletickets-sync: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    if args.flags.contains("--help") || args.command.is_empty() {
        println!("{USAGE}");
        return if args.command.is_empty() && !args.flags.contains("--help") {
            ExitCode::from(2)
        } else {
            ExitCode::SUCCESS
        };
    }
    let dir = args
        .dir
        .clone()
        .or_else(|| std::env::var_os("SIMPLETICKETS_DIR").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("."));
    let config_file = args
        .config
        .clone()
        .unwrap_or_else(|| state::config_path(&dir));
    let config = match Config::load(&config_file) {
        Ok(c) => c,
        Err(e) => return fail(&e),
    };
    if let Some(name) = args.value("--target") {
        if !config.targets.contains_key(name) {
            return fail(&format!("no target named `{name}` in the configuration"));
        }
    }
    let result = match args.command.as_str() {
        "run" => run(&args, &dir, &config),
        "status" => status(&args, &dir, &config),
        "dry-run" => dry_run(&args, &dir, &config),
        "push-now" => push_now(&args, &dir, &config),
        "list-conflicts" => list_conflicts(&args, &dir, &config),
        other => Err(format!("unknown command `{other}`\n\n{USAGE}")),
    };
    match result {
        Ok(code) => code,
        Err(e) => fail(&e),
    }
}

fn connect(args: &Args, dir: &std::path::Path) -> Result<Api, String> {
    Api::connect(args.url.as_deref(), args.token_file.as_deref(), dir)
}

fn run(args: &Args, dir: &std::path::Path, config: &Config) -> Result<ExitCode, String> {
    if config.targets.is_empty() {
        return Err("the configuration lists no targets".into());
    }
    let api = connect(args, dir)?;
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    ctrlc::set_handler(move || flag.store(true, Ordering::Relaxed))
        .map_err(|e| format!("cannot install the signal handler: {e}"))?;
    let supervised = std::env::var("SIMPLETICKETS_PLUGIN_NAME").as_deref() == Ok("sync");
    service::run(&api, config, dir, &stop, supervised)?;
    Ok(ExitCode::SUCCESS)
}

fn options(args: &Args) -> Result<Options, String> {
    let prefer = match args.value("--prefer") {
        None => None,
        Some("local") => Some(Prefer::Local),
        Some("remote") => Some(Prefer::Remote),
        Some(other) => {
            return Err(format!(
                "--prefer must be `local` or `remote`, not `{other}`"
            ))
        }
    };
    Ok(Options {
        force: args.flags.contains("--force"),
        only_ids: (!args.positional.is_empty()).then(|| args.positional.iter().cloned().collect()),
        only_target: args.value("--target").map(String::from),
        pull: true,
        prefer,
        ..Options::default()
    })
}

fn report_json(report: &Report) -> Value {
    json!({
        "actions": report.actions.iter().map(|a| json!({
            "target": a.target, "id": a.id, "kind": a.kind, "detail": a.detail,
        })).collect::<Vec<_>>(),
        "target_errors": report.target_errors.iter()
            .map(|(t, m)| json!({"target": t, "message": m})).collect::<Vec<_>>(),
    })
}

fn print_report(report: &Report, json_output: bool) {
    if json_output {
        println!(
            "{}",
            serde_json::to_string_pretty(&report_json(report)).unwrap_or_default()
        );
        return;
    }
    for (target, message) in &report.target_errors {
        println!("{target}: ERROR {message}");
    }
    for a in &report.actions {
        println!("{:<10} {:<8} {:<9} {}", a.target, a.id, a.kind, a.detail);
    }
    if report.actions.is_empty() && report.target_errors.is_empty() {
        println!("nothing to do");
    }
}

fn dry_run(args: &Args, dir: &std::path::Path, config: &Config) -> Result<ExitCode, String> {
    let api = connect(args, dir)?;
    let mut options = options(args)?;
    options.dry_run = true;
    options.prefer = None;
    let report = Engine::new(&api, config, dir, None).run(&options)?;
    print_report(&report, args.flags.contains("--json"));
    Ok(ExitCode::SUCCESS)
}

fn push_now(args: &Args, dir: &std::path::Path, config: &Config) -> Result<ExitCode, String> {
    let options = options(args)?;
    if Lock::is_held(dir) {
        state::write_request(
            dir,
            &Request {
                ids: args.positional.clone(),
                target: options.only_target.clone(),
                force: options.force,
                prefer: args.value("--prefer").map(String::from),
            },
        )?;
        println!("the sync service is running; the request is queued and runs within seconds");
        return Ok(ExitCode::SUCCESS);
    }
    let api = connect(args, dir)?;
    let lock = Lock::acquire(dir)?;
    let report = Engine::new(&api, config, dir, Some(&lock)).run(&options)?;
    print_report(&report, args.flags.contains("--json"));
    Ok(if report.has_errors() {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

fn status(args: &Args, dir: &std::path::Path, config: &Config) -> Result<ExitCode, String> {
    let json_output = args.flags.contains("--json");
    let mut targets = Vec::new();
    let mut trouble = false;
    for (name, target) in &config.targets {
        let state = TargetState::load(dir, name)?;
        let count = |phase: Phase| state.tickets.values().filter(|t| t.phase == phase).count();
        let errors: Vec<Value> = state
            .tickets
            .iter()
            .filter(|(_, t)| t.error.is_some() && t.phase != Phase::Synced)
            .map(|(id, t)| json!({"id": id, "error": t.error, "attempts": t.attempts}))
            .collect();
        let warnings: Vec<Value> = state
            .tickets
            .iter()
            .filter(|(_, t)| !t.warnings.is_empty())
            .map(|(id, t)| json!({"id": id, "warnings": t.warnings}))
            .collect();
        let check = if args.flags.contains("--check") {
            Some(check_target(name, target, dir))
        } else {
            None
        };
        let open = state.open_conflicts().count();
        trouble |= open > 0
            || !errors.is_empty()
            || state.last_error.is_some()
            || check.as_ref().is_some_and(Result::is_err);
        targets.push(json!({
            "target": name,
            "connector": target.connector.command,
            "synced": count(Phase::Synced),
            "errors": errors,
            "warnings": warnings,
            "conflicts": open,
            "closed": count(Phase::Closed) + count(Phase::Deleted),
            "remote_missing": count(Phase::RemoteMissing),
            "last_pass": state.last_pass,
            "last_error": state.last_error,
            "check": check.map(|c| match c {
                Ok(message) => json!({"ok": true, "message": message}),
                Err(message) => json!({"ok": false, "message": message}),
            }),
        }));
    }
    let running = Lock::is_held(dir);
    if json_output {
        let out = json!({"service_running": running, "targets": targets});
        println!("{}", serde_json::to_string_pretty(&out).unwrap_or_default());
    } else {
        println!(
            "service: {}",
            if running { "running" } else { "not running" }
        );
        for t in &targets {
            println!(
                "{}: {} synced, {} conflict(s), {} error(s), {} closed, last pass {}",
                t["target"].as_str().unwrap_or(""),
                t["synced"],
                t["conflicts"],
                t["errors"].as_array().map_or(0, Vec::len),
                t["closed"],
                t["last_pass"].as_str().unwrap_or("never"),
            );
            if let Some(e) = t["last_error"].as_str() {
                println!("  target error: {e}");
            }
            for e in t["errors"].as_array().into_iter().flatten() {
                println!(
                    "  {}: {}",
                    e["id"].as_str().unwrap_or(""),
                    e["error"].as_str().unwrap_or("")
                );
            }
            for w in t["warnings"].as_array().into_iter().flatten() {
                println!(
                    "  {} warns: {}",
                    w["id"].as_str().unwrap_or(""),
                    w["warnings"]
                );
            }
            if t["remote_missing"].as_u64().unwrap_or(0) > 0 {
                println!("  {} ticket(s) lost their remote item", t["remote_missing"]);
            }
            if let Some(c) = t["check"].as_object() {
                let word = if c["ok"] == true {
                    "check ok"
                } else {
                    "check FAILED"
                };
                println!("  {word}: {}", c["message"].as_str().unwrap_or(""));
            }
        }
    }
    Ok(if trouble {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

fn list_conflicts(args: &Args, dir: &std::path::Path, config: &Config) -> Result<ExitCode, String> {
    let all = args.flags.contains("--all");
    let mut rows = Vec::new();
    for name in config.targets.keys() {
        let state = TargetState::load(dir, name)?;
        for c in state
            .conflicts
            .iter()
            .filter(|c| all || c.resolution.is_none())
        {
            rows.push(json!({
                "target": name, "id": c.id, "kind": c.kind, "field": c.field,
                "local": c.local, "remote": c.remote,
                "detected_at": c.detected_at, "resolution": c.resolution,
            }));
        }
    }
    if args.flags.contains("--json") {
        println!(
            "{}",
            serde_json::to_string_pretty(&rows).unwrap_or_default()
        );
    } else if rows.is_empty() {
        println!("no conflicts");
    } else {
        for r in &rows {
            let what = match r["field"].as_str() {
                Some(field) => format!("{field}: local {} vs remote {}", r["local"], r["remote"]),
                None => "the remote item no longer exists".to_string(),
            };
            let outcome = r["resolution"].as_str().unwrap_or("needs a decision");
            println!(
                "{} {} {} ({}): {what} [{outcome}]",
                r["target"].as_str().unwrap_or(""),
                r["id"].as_str().unwrap_or(""),
                r["kind"].as_str().unwrap_or(""),
                r["detected_at"].as_str().unwrap_or(""),
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn parses_globals_flags_values_and_ids_in_any_order() {
        let a = parse(&args(&[
            "push-now", "ST-1", "--dir", "/t", "--prefer", "local", "--force", "ST-2",
        ]))
        .unwrap();
        assert_eq!(a.command, "push-now");
        assert_eq!(a.positional, ["ST-1", "ST-2"]);
        assert_eq!(a.dir, Some(PathBuf::from("/t")));
        assert_eq!(a.value("--prefer"), Some("local"));
        assert!(a.flags.contains("--force"));
    }

    #[test]
    fn rejects_unknown_options_and_missing_values() {
        assert!(parse(&args(&["status", "--bogus"])).is_err());
        assert!(parse(&args(&["status", "--dir"])).is_err());
        assert!(options(&parse(&args(&["push-now", "--prefer", "x"])).unwrap()).is_err());
    }
}
