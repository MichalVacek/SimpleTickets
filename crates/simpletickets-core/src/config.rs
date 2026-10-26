//! Configuration, stored as a YAML file in the default tickets folder.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use thiserror::Error;

/// File name of the configuration inside the tickets folder.
pub const CONFIG_FILE: &str = "simpletickets.yml";

/// The four status categories that rules act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Category {
    Todo,
    InProgress,
    Done,
    Cancelled,
}

impl Category {
    pub const ALL: [Category; 4] = [
        Category::Todo,
        Category::InProgress,
        Category::Done,
        Category::Cancelled,
    ];

    /// The category with the given key (`todo`, `in-progress`, `done`, `cancelled`).
    pub fn from_key(key: &str) -> Option<Category> {
        Category::ALL.into_iter().find(|c| c.key() == key)
    }

    pub fn key(self) -> &'static str {
        match self {
            Category::Todo => "todo",
            Category::InProgress => "in-progress",
            Category::Done => "done",
            Category::Cancelled => "cancelled",
        }
    }
}

impl fmt::Display for Category {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.key())
    }
}

/// How a root's Markdown files are recognised as tickets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Default)]
#[serde(rename_all = "kebab-case")]
pub enum RootKind {
    /// Every `.md` file is a ticket.
    #[default]
    Tickets,
    /// Only files carrying the ticket marker are tickets (a code repository).
    Repo,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root {
    pub path: PathBuf,
    pub kind: RootKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
    Info,
    Warn,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The tickets folder: holds this config and the `.simpletickets` state folder.
    pub dir: PathBuf,
    /// Prefix of generated ticket IDs.
    pub id_prefix: String,
    pub roots: Vec<Root>,
    /// Folders watched for new files, in addition to the roots.
    pub auto_scan: Vec<PathBuf>,
    /// Folder holding parent (specification) tickets.
    pub parent_folder: Option<PathBuf>,
    /// Folder holding sub-tickets.
    pub subticket_folder: Option<PathBuf>,
    statuses: BTreeMap<String, Category>,
    /// Log level per tag, applied when a ticket with that tag changes.
    pub tag_logging: BTreeMap<String, LogLevel>,
    /// Whether a cancelled blocker stops blocking.
    pub cancelled_unblocks: bool,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConfigError {
    #[error("cannot read config {path}: {message}")]
    Read { path: PathBuf, message: String },
    #[error("invalid config syntax: {0}")]
    Syntax(String),
    #[error("invalid config: {0}")]
    Invalid(String),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    id_prefix: Option<String>,
    #[serde(default)]
    roots: Vec<RawRoot>,
    #[serde(default)]
    auto_scan: Vec<PathBuf>,
    parent_folder: Option<PathBuf>,
    subticket_folder: Option<PathBuf>,
    statuses: Option<BTreeMap<String, Vec<String>>>,
    #[serde(default)]
    tag_logging: BTreeMap<String, LogLevel>,
    cancelled_unblocks: Option<bool>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRoot {
    path: PathBuf,
    #[serde(default)]
    kind: RootKind,
}

impl Config {
    /// Default configuration for a tickets folder: that folder is the only root.
    pub fn default_for(dir: &Path) -> Config {
        Config {
            dir: dir.to_path_buf(),
            id_prefix: "ST".into(),
            roots: vec![Root {
                path: dir.to_path_buf(),
                kind: RootKind::Tickets,
            }],
            auto_scan: Vec::new(),
            parent_folder: None,
            subticket_folder: None,
            statuses: default_statuses(),
            tag_logging: BTreeMap::new(),
            cancelled_unblocks: true,
        }
    }

    /// Loads `simpletickets.yml` from `dir`, or the default if it does not exist.
    pub fn load_from_dir(dir: &Path) -> Result<Config, ConfigError> {
        let path = dir.join(CONFIG_FILE);
        match std::fs::read_to_string(&path) {
            Ok(text) => Config::parse(&text, dir),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Config::default_for(dir)),
            Err(e) => Err(ConfigError::Read {
                path,
                message: e.to_string(),
            }),
        }
    }

    /// Parses and validates a config; relative paths resolve against `dir`.
    pub fn parse(text: &str, dir: &Path) -> Result<Config, ConfigError> {
        let raw: RawConfig = serde_yaml_ng::from_str(text).map_err(|e| {
            let at = e.location().map_or(String::new(), |l| {
                format!(" (line {}, column {})", l.line(), l.column())
            });
            ConfigError::Syntax(format!("{e}{at}"))
        })?;
        let resolve = |p: &Path| {
            if p.is_absolute() {
                p.to_path_buf()
            } else {
                dir.join(p)
            }
        };

        let id_prefix = raw.id_prefix.unwrap_or_else(|| "ST".into());
        if id_prefix.is_empty() || !id_prefix.chars().all(|c| c.is_ascii_alphanumeric()) {
            return invalid(format!(
                "id_prefix `{id_prefix}` must be non-empty and alphanumeric"
            ));
        }

        let roots: Vec<Root> = if raw.roots.is_empty() {
            vec![Root {
                path: dir.to_path_buf(),
                kind: RootKind::Tickets,
            }]
        } else {
            raw.roots
                .iter()
                .map(|r| Root {
                    path: resolve(&r.path),
                    kind: r.kind,
                })
                .collect()
        };
        for (i, root) in roots.iter().enumerate() {
            if roots[..i].iter().any(|r| r.path == root.path) {
                return invalid(format!("root {} is listed twice", root.path.display()));
            }
        }

        let parent_folder = raw.parent_folder.map(|p| resolve(&p));
        let subticket_folder = raw.subticket_folder.map(|p| resolve(&p));
        if parent_folder.is_some() && parent_folder == subticket_folder {
            return invalid("parent_folder and subticket_folder must differ".into());
        }

        let statuses = match raw.statuses {
            None => default_statuses(),
            Some(map) => build_statuses(map)?,
        };

        for tag in raw.tag_logging.keys() {
            if tag.trim().is_empty() {
                return invalid("tag_logging contains an empty tag name".into());
            }
        }

        Ok(Config {
            dir: dir.to_path_buf(),
            id_prefix,
            roots,
            auto_scan: raw.auto_scan.iter().map(|p| resolve(p)).collect(),
            parent_folder,
            subticket_folder,
            statuses,
            tag_logging: raw.tag_logging,
            cancelled_unblocks: raw.cancelled_unblocks.unwrap_or(true),
        })
    }

    /// The category of a status name, compared case-insensitively.
    pub fn category_of(&self, status: &str) -> Option<Category> {
        self.statuses.get(&status.trim().to_lowercase()).copied()
    }

    /// All configured status names with their categories.
    pub fn statuses(&self) -> &BTreeMap<String, Category> {
        &self.statuses
    }

    /// The configuration as JSON, with paths as strings.
    pub fn to_json(&self) -> serde_json::Value {
        let paths = |ps: &[PathBuf]| -> Vec<String> {
            ps.iter().map(|p| p.display().to_string()).collect()
        };
        let statuses: BTreeMap<&str, Vec<&str>> = Category::ALL
            .into_iter()
            .map(|c| (c.key(), self.statuses_in(c)))
            .collect();
        let tag_logging: BTreeMap<&String, &str> = self
            .tag_logging
            .iter()
            .map(|(t, l)| {
                (
                    t,
                    match l {
                        LogLevel::Info => "info",
                        LogLevel::Warn => "warn",
                    },
                )
            })
            .collect();
        serde_json::json!({
            "id_prefix": self.id_prefix,
            "roots": self.roots.iter().map(|r| serde_json::json!({
                "path": r.path.display().to_string(),
                "kind": match r.kind { RootKind::Tickets => "tickets", RootKind::Repo => "repo" },
            })).collect::<Vec<_>>(),
            "auto_scan": paths(&self.auto_scan),
            "parent_folder": self.parent_folder.as_ref().map(|p| p.display().to_string()),
            "subticket_folder": self.subticket_folder.as_ref().map(|p| p.display().to_string()),
            "statuses": statuses,
            "tag_logging": tag_logging,
            "cancelled_unblocks": self.cancelled_unblocks,
        })
    }

    /// Status names configured for a category, sorted.
    pub fn statuses_in(&self, category: Category) -> Vec<&str> {
        self.statuses
            .iter()
            .filter(|(_, c)| **c == category)
            .map(|(s, _)| s.as_str())
            .collect()
    }
}

/// Replaces the config file in `dir` atomically.
pub fn write_file(dir: &Path, text: &str) -> std::io::Result<()> {
    let path = dir.join(CONFIG_FILE);
    let temp = dir.join(format!(".{CONFIG_FILE}.{}.tmp", std::process::id()));
    let result = std::fs::write(&temp, text).and_then(|()| std::fs::rename(&temp, &path));
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

fn invalid<T>(message: String) -> Result<T, ConfigError> {
    Err(ConfigError::Invalid(message))
}

fn default_statuses() -> BTreeMap<String, Category> {
    [
        ("todo", Category::Todo),
        ("in-progress", Category::InProgress),
        ("done", Category::Done),
        ("cancelled", Category::Cancelled),
    ]
    .into_iter()
    .map(|(s, c)| (s.to_string(), c))
    .collect()
}

fn build_statuses(
    map: BTreeMap<String, Vec<String>>,
) -> Result<BTreeMap<String, Category>, ConfigError> {
    let mut out = BTreeMap::new();
    for (key, names) in &map {
        let Some(category) = Category::ALL.into_iter().find(|c| c.key() == key) else {
            return invalid(format!(
                "unknown status category `{key}` (expected todo, in-progress, done or cancelled)"
            ));
        };
        for name in names {
            let name = name.trim().to_lowercase();
            if name.is_empty() {
                return invalid(format!("category `{key}` contains an empty status name"));
            }
            if let Some(previous) = out.insert(name.clone(), category) {
                return invalid(format!(
                    "status `{name}` is mapped to both `{previous}` and `{category}`"
                ));
            }
        }
    }
    for category in Category::ALL {
        if !out.values().any(|c| *c == category) {
            return invalid(format!("category `{category}` has no statuses"));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Config, ConfigError> {
        Config::parse(text, Path::new("/t"))
    }

    fn invalid_msg(text: &str) -> String {
        match parse(text) {
            Err(ConfigError::Invalid(m)) => m,
            other => panic!("expected invalid config, got {other:?}"),
        }
    }

    #[test]
    fn empty_config_uses_defaults() {
        let c = parse("{}").unwrap();
        assert_eq!(c, Config::default_for(Path::new("/t")));
        assert_eq!(c.category_of("Done"), Some(Category::Done));
        assert_eq!(c.category_of("nope"), None);
        assert!(c.cancelled_unblocks);
    }

    #[test]
    fn full_config() {
        let c = parse(
            "id_prefix: WK\n\
roots:\n  - path: .\n  - path: /code/app\n    kind: repo\n\
auto_scan: [inbox]\n\
parent_folder: specs\n\
subticket_folder: work\n\
statuses:\n  todo: [Backlog, Ready]\n  in-progress: [Doing]\n  done: [Shipped]\n  cancelled: [Dropped]\n\
tag_logging:\n  urgent: warn\n\
cancelled_unblocks: false\n",
        )
        .unwrap();
        assert_eq!(c.id_prefix, "WK");
        assert_eq!(c.roots[0].path, Path::new("/t/."));
        assert_eq!(c.roots[1].kind, RootKind::Repo);
        assert_eq!(c.auto_scan, [PathBuf::from("/t/inbox")]);
        assert_eq!(c.parent_folder, Some(PathBuf::from("/t/specs")));
        assert_eq!(c.category_of("backlog"), Some(Category::Todo));
        assert_eq!(c.category_of("Doing"), Some(Category::InProgress));
        assert_eq!(c.statuses_in(Category::Todo), ["backlog", "ready"]);
        assert_eq!(c.tag_logging["urgent"], LogLevel::Warn);
        assert!(!c.cancelled_unblocks);
    }

    #[test]
    fn rejects_unknown_keys_and_bad_syntax() {
        assert!(matches!(parse("bogus: 1"), Err(ConfigError::Syntax(_))));
        assert!(matches!(parse("roots: [1"), Err(ConfigError::Syntax(_))));
        assert!(matches!(
            parse("roots:\n  - path: a\n    kind: nope"),
            Err(ConfigError::Syntax(_))
        ));
    }

    #[test]
    fn rejects_bad_prefix() {
        assert!(invalid_msg("id_prefix: 'a-b'").contains("id_prefix"));
        assert!(invalid_msg("id_prefix: ''").contains("id_prefix"));
    }

    #[test]
    fn rejects_duplicate_roots_and_same_folders() {
        assert!(invalid_msg("roots: [{path: a}, {path: a}]").contains("twice"));
        assert!(invalid_msg("parent_folder: x\nsubticket_folder: x").contains("differ"));
    }

    #[test]
    fn rejects_bad_status_mapping() {
        let all = "todo: [a]\n  in-progress: [b]\n  done: [c]\n  cancelled: [d]";
        assert!(invalid_msg(&format!("statuses:\n  {all}\n  extra: [e]")).contains("unknown"));
        assert!(invalid_msg(
            "statuses:\n  todo: [a]\n  in-progress: [a]\n  done: [c]\n  cancelled: [d]"
        )
        .contains("both"));
        assert!(
            invalid_msg("statuses:\n  todo: [a]\n  in-progress: [b]\n  done: [c]")
                .contains("`cancelled` has no statuses")
        );
    }

    #[test]
    fn loads_from_directory() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            Config::load_from_dir(dir.path()).unwrap(),
            Config::default_for(dir.path())
        );
        std::fs::write(dir.path().join(CONFIG_FILE), "id_prefix: AB\n").unwrap();
        assert_eq!(Config::load_from_dir(dir.path()).unwrap().id_prefix, "AB");
        std::fs::write(dir.path().join(CONFIG_FILE), "id_prefix: [\n").unwrap();
        assert!(Config::load_from_dir(dir.path()).is_err());
    }
}
