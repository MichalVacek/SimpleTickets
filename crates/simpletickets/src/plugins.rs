//! The `simpletickets plugins` commands and the checks shared with the daemon.

use crate::api::API_VERSION;
use clap::Subcommand;
use serde_json::{json, Value};
use simpletickets_core::config::Config;
use simpletickets_core::plugin::{self, Found, Manifest};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Subcommand)]
pub enum PluginsCommand {
    /// List the plugins found in the tickets folder and the user folder
    List {
        #[arg(long)]
        json: bool,
    },
    /// Enable a plugin in simpletickets.yml
    Enable { name: String },
    /// Disable a plugin in simpletickets.yml
    Disable { name: String },
    /// Check that the enabled plugins can be started
    Status {
        #[arg(long)]
        json: bool,
    },
}

/// The folder the running executable lives in; plugins installed next to it are found
/// there.
pub fn bin_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
}

fn env_is_set(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty())
}

/// Why a plugin cannot be started on this machine and API version, if it cannot.
pub fn preflight(manifest: &Manifest, plugin_dir: &Path) -> Result<(), String> {
    if !manifest.api_version.contains(API_VERSION) {
        return Err(format!(
            "needs API version {} but this daemon serves {API_VERSION}",
            manifest.api_version
        ));
    }
    let missing: Vec<&str> = manifest
        .env
        .iter()
        .filter(|name| !env_is_set(name))
        .map(String::as_str)
        .collect();
    if !missing.is_empty() {
        return Err(format!(
            "missing environment variables: {}",
            missing.join(", ")
        ));
    }
    if !manifest.command_exists(plugin_dir, bin_dir().as_deref()) {
        return Err(format!("command `{}` not found", manifest.command));
    }
    Ok(())
}

/// `None` when the plugin is fine, else the problem.
fn problem(found: &Found) -> Option<String> {
    match &found.manifest {
        Err(e) => Some(format!("invalid manifest: {e}")),
        Ok(m) => preflight(m, &found.dir).err(),
    }
}

fn describe(found: &Found, enabled: bool) -> Value {
    let base = match &found.manifest {
        Ok(m) => m.to_json(),
        Err(_) => json!({"name": found.name}),
    };
    let mut v = base;
    let map = v.as_object_mut().expect("object");
    map.insert("enabled".into(), json!(enabled));
    map.insert("source".into(), json!(found.source.as_str()));
    map.insert("dir".into(), json!(found.dir.display().to_string()));
    map.insert("valid".into(), json!(found.manifest.is_ok()));
    map.insert("problem".into(), json!(problem(found)));
    v
}

pub fn run(
    dir: &Path,
    config: &Config,
    command: PluginsCommand,
    user_dir: Option<PathBuf>,
) -> Result<ExitCode, String> {
    let found = plugin::discover(&plugin::search_dirs(dir, user_dir));
    let enabled = |name: &str| config.plugins.iter().any(|n| n == name);
    match command {
        PluginsCommand::List { json } => {
            let mut rows: Vec<Value> = found
                .iter()
                .map(|f| describe(f, enabled(&f.name)))
                .collect();
            for name in &config.plugins {
                if !found.iter().any(|f| &f.name == name) {
                    rows.push(json!({"name": name, "enabled": true, "valid": false,
                                     "problem": "enabled but not installed"}));
                }
            }
            if json {
                println!("{}", Value::Array(rows));
                return Ok(ExitCode::SUCCESS);
            }
            if rows.is_empty() {
                println!("no plugins found");
            }
            for row in &rows {
                let text = |k: &str| row[k].as_str().unwrap_or("-").to_string();
                println!(
                    "{:<16} {:<8} {:<12} {:<9} {}",
                    text("name"),
                    text("version"),
                    text("kind"),
                    if row["enabled"] == true {
                        "enabled"
                    } else {
                        "disabled"
                    },
                    text("description"),
                );
            }
            Ok(ExitCode::SUCCESS)
        }
        PluginsCommand::Enable { name } => {
            let entry = found.iter().find(|f| f.name == name).ok_or_else(|| {
                format!("no plugin named `{name}` (see `simpletickets plugins list`)")
            })?;
            if let Err(e) = &entry.manifest {
                return Err(format!("plugin `{name}` has an invalid manifest: {e}"));
            }
            if enabled(&name) {
                println!("{name} is already enabled");
                return Ok(ExitCode::SUCCESS);
            }
            let mut names = config.plugins.clone();
            names.push(name.clone());
            plugin::write_enabled(dir, &names)?;
            println!("{name} enabled; a running daemon starts it on its next config check");
            Ok(ExitCode::SUCCESS)
        }
        PluginsCommand::Disable { name } => {
            if !enabled(&name) {
                println!("{name} is already disabled");
                return Ok(ExitCode::SUCCESS);
            }
            let names: Vec<String> = config
                .plugins
                .iter()
                .filter(|n| **n != name)
                .cloned()
                .collect();
            plugin::write_enabled(dir, &names)?;
            println!("{name} disabled; a running daemon stops it on its next config check");
            Ok(ExitCode::SUCCESS)
        }
        PluginsCommand::Status { json } => {
            let rows: Vec<Value> = found
                .iter()
                .filter(|f| enabled(&f.name))
                .map(|f| describe(f, true))
                .collect();
            let missing: Vec<&String> = config
                .plugins
                .iter()
                .filter(|n| !found.iter().any(|f| &f.name == *n))
                .collect();
            let broken = rows.iter().filter(|r| !r["problem"].is_null()).count() + missing.len();
            if json {
                println!("{}", json!({"plugins": rows, "missing": missing}));
            } else {
                if rows.is_empty() && missing.is_empty() {
                    println!("no plugins enabled");
                }
                for row in &rows {
                    let text = |k: &str| row[k].as_str().unwrap_or("-").to_string();
                    let state = row["problem"]
                        .as_str()
                        .map_or_else(|| "ready".to_string(), |p| format!("not ready: {p}"));
                    println!("{:<16} {:<12} {state}", text("name"), text("kind"));
                }
                for name in missing {
                    println!(
                        "{name:<16} {:<12} not ready: enabled but not installed",
                        "-"
                    );
                }
            }
            Ok(if broken == 0 {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            })
        }
    }
}
