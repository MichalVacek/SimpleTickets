//! The sync plugin's own configuration: targets, how each reaches its connector, and how
//! ticket values map to the remote's. It lives in `.simpletickets/sync/config.yml` and
//! never holds a secret: connectors read credentials from environment variables.

use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Seconds between full reconciles, which also catch edits the event stream missed.
    #[serde(default = "default_reconcile")]
    pub reconcile_interval_seconds: u64,
    /// Seconds between polls of the remotes for changes, for targets that pull.
    #[serde(default = "default_pull")]
    pub pull_interval_seconds: u64,
    #[serde(default)]
    pub targets: BTreeMap<String, Target>,
}

fn default_reconcile() -> u64 {
    300
}

fn default_pull() -> u64 {
    60
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    pub connector: ConnectorSpec,
    /// Passed to the connector's `configure`; opaque to the sync plugin.
    #[serde(default)]
    pub options: Value,
    #[serde(default)]
    pub direction: Direction,
    #[serde(default)]
    pub on_conflict: ConflictPolicy,
    #[serde(default)]
    pub on_delete: DeletePolicy,
    #[serde(default)]
    pub unmapped: Unmapped,
    #[serde(default)]
    pub filter: Filter,
    #[serde(default)]
    pub map: Mapping,
    #[serde(default)]
    pub pull: Pull,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectorSpec {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    /// Names of environment variables handed to the connector (credentials, mostly).
    #[serde(default)]
    pub env: Vec<String>,
    #[serde(default = "default_timeout")]
    pub timeout_seconds: u64,
}

fn default_timeout() -> u64 {
    120
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Direction {
    /// Local changes go to the remote; the remote is never read.
    #[default]
    Push,
    /// Also read remote changes of the pulled fields back through the API.
    Both,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ConflictPolicy {
    #[default]
    LocalWins,
    RemoteWins,
    /// Report and change nothing on either side until the operator decides.
    Manual,
}

impl ConflictPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            ConflictPolicy::LocalWins => "local-wins",
            ConflictPolicy::RemoteWins => "remote-wins",
            ConflictPolicy::Manual => "manual",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeletePolicy {
    /// Ask the connector to close the remote item. Nothing is ever deleted remotely.
    #[default]
    Close,
    /// Leave the remote item as it is.
    Ignore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Unmapped {
    /// A value with no mapping is not sent, so the remote keeps whatever it has.
    #[default]
    Skip,
    /// A value with no mapping is sent as it is.
    Pass,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Filter {
    /// Only tickets with at least one of these tags.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Only tickets whose category is one of these (`todo`, `in-progress`, ...).
    #[serde(default)]
    pub categories: Vec<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Mapping {
    /// Local status name to remote status.
    #[serde(default)]
    pub status: BTreeMap<String, String>,
    /// Status category to remote status, for statuses not listed in `status`.
    #[serde(default)]
    pub category: BTreeMap<String, String>,
    #[serde(default)]
    pub priority: BTreeMap<String, String>,
    /// Local assignee to the remote user identifier.
    #[serde(default)]
    pub assignee: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pull {
    /// Fields read back from the remote: any of `status`, `priority`, `assignee`,
    /// `title`, `tags`, `due`.
    #[serde(default = "default_pull_fields")]
    pub fields: Vec<String>,
    /// Remote status to local status, overriding the inverse of `map.status`.
    #[serde(default)]
    pub status: BTreeMap<String, String>,
    #[serde(default)]
    pub priority: BTreeMap<String, String>,
    #[serde(default)]
    pub assignee: BTreeMap<String, String>,
}

fn default_pull_fields() -> Vec<String> {
    vec!["status".into()]
}

impl Default for Pull {
    fn default() -> Pull {
        Pull {
            fields: default_pull_fields(),
            status: BTreeMap::new(),
            priority: BTreeMap::new(),
            assignee: BTreeMap::new(),
        }
    }
}

/// The fields a target may pull.
pub const PULLABLE: &[&str] = &["status", "priority", "assignee", "title", "tags", "due"];

impl Config {
    pub fn parse(text: &str) -> Result<Config, String> {
        let config: Config = serde_yaml_ng::from_str(text).map_err(|e| e.to_string())?;
        config.validate()?;
        Ok(config)
    }

    pub fn load(path: &Path) -> Result<Config, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read the sync config {}: {e}", path.display()))?;
        Config::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    fn validate(&self) -> Result<(), String> {
        if self.reconcile_interval_seconds == 0 || self.pull_interval_seconds == 0 {
            return Err("the intervals must be at least 1 second".into());
        }
        for (name, target) in &self.targets {
            let valid = !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
            if !valid {
                return Err(format!(
                    "target name `{name}` must use lower-case letters, digits, `-` and `_`"
                ));
            }
            if target.connector.command.trim().is_empty() {
                return Err(format!("target `{name}`: connector.command is empty"));
            }
            if target.connector.timeout_seconds == 0 {
                return Err(format!("target `{name}`: connector.timeout_seconds is 0"));
            }
            if let Some(bad) = target
                .pull
                .fields
                .iter()
                .find(|f| !PULLABLE.contains(&f.as_str()))
            {
                return Err(format!(
                    "target `{name}`: cannot pull `{bad}` (pullable: {})",
                    PULLABLE.join(", ")
                ));
            }
            if let Some(bad) =
                target.connector.env.iter().find(|v| {
                    v.is_empty() || !v.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                })
            {
                return Err(format!(
                    "target `{name}`: `{bad}` is not an environment variable name; list names, never values"
                ));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = "
reconcile_interval_seconds: 120
targets:
  jira:
    connector:
      command: simpletickets-connector-jira
      env: [JIRA_SITE_URL, JIRA_EMAIL, JIRA_API_TOKEN]
    options: {project: ST}
    direction: both
    on_conflict: remote-wins
    on_delete: ignore
    unmapped: pass
    filter: {tags: [sync]}
    map:
      status: {todo: To Do}
      category: {done: Done}
    pull:
      fields: [status, priority]
";

    #[test]
    fn parses_a_full_config_and_applies_defaults() {
        let config = Config::parse(FULL).unwrap();
        assert_eq!(config.reconcile_interval_seconds, 120);
        assert_eq!(config.pull_interval_seconds, 60);
        let jira = &config.targets["jira"];
        assert_eq!(jira.direction, Direction::Both);
        assert_eq!(jira.on_conflict, ConflictPolicy::RemoteWins);
        assert_eq!(jira.on_delete, DeletePolicy::Ignore);
        assert_eq!(jira.unmapped, Unmapped::Pass);
        assert_eq!(jira.options["project"], "ST");
        assert_eq!(jira.connector.timeout_seconds, 120);

        let minimal = Config::parse("targets:\n  a:\n    connector: {command: x}\n").unwrap();
        let a = &minimal.targets["a"];
        assert_eq!(a.direction, Direction::Push);
        assert_eq!(a.on_conflict, ConflictPolicy::LocalWins);
        assert_eq!(a.on_delete, DeletePolicy::Close);
        assert_eq!(a.unmapped, Unmapped::Skip);
        assert_eq!(a.pull.fields, ["status"]);
    }

    #[test]
    fn rejects_mistakes_instead_of_ignoring_them() {
        assert!(Config::parse("targts: {}").is_err());
        assert!(Config::parse("targets:\n  A:\n    connector: {command: x}\n").is_err());
        assert!(Config::parse("targets:\n  a:\n    connector: {command: ''}\n").is_err());
        assert!(Config::parse(
            "targets:\n  a:\n    connector: {command: x}\n    pull: {fields: [body]}\n"
        )
        .is_err());
        assert!(
            Config::parse("targets:\n  a:\n    connector: {command: x, env: ['TOKEN=abc']}\n")
                .is_err()
        );
        assert!(Config::parse("reconcile_interval_seconds: 0\n").is_err());
    }
}
