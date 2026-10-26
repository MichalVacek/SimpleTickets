//! Plugin manifests: parsing, validation and discovery.
//!
//! A plugin is a folder holding a `plugin.yml` that describes how to run it. The folder
//! name is the plugin name. Plugins are looked up in the tickets folder's
//! `.simpletickets/plugins` and in a per-user folder; the config lists which are enabled.

use crate::config::{Config, CONFIG_FILE};
use crate::state::plugins_dir;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// File name of a plugin manifest inside the plugin folder.
pub const MANIFEST_FILE: &str = "plugin.yml";

/// Environment variable that overrides the per-user plugins folder.
pub const USER_DIR_ENV: &str = "SIMPLETICKETS_PLUGINS_DIR";

/// Variables the daemon sets for the plugins it starts; manifests cannot ask for them.
pub const RESERVED_ENV_PREFIX: &str = "SIMPLETICKETS_";

const PLACEHOLDERS: [&str; 4] = ["tickets_dir", "api_url", "token_file", "plugin_dir"];

/// True for a name made of lower-case letters, digits, `-` and `_`, starting with a
/// letter or digit.
pub fn is_valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        && name.len() <= 64
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Started by an external client (an MCP host, say) that talks to it over stdio; the
    /// daemon never runs it.
    StdioClient,
    /// A long-running process the daemon starts, supervises and stops.
    Service,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::StdioClient => "stdio-client",
            Kind::Service => "service",
        }
    }
}

/// How the plugin receives the API token. The daemon always sets
/// `SIMPLETICKETS_TOKEN_FILE`; `Env` also sets `SIMPLETICKETS_TOKEN`. The token is never
/// put on a command line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenDelivery {
    File,
    Env,
}

/// Inclusive range of API versions a plugin works with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApiRange {
    pub min: u32,
    /// `None` for no upper bound.
    pub max: Option<u32>,
}

impl ApiRange {
    /// Parses `N` (exactly N), `N-M` (inclusive) or `N-` (N or newer).
    pub fn parse(text: &str) -> Result<ApiRange, String> {
        let bad = || format!("api_version `{text}` must look like `1`, `1-2` or `1-`");
        let number = |s: &str| s.trim().parse::<u32>().map_err(|_| bad());
        let range = match text.trim().split_once('-') {
            None => {
                let n = number(text)?;
                ApiRange {
                    min: n,
                    max: Some(n),
                }
            }
            Some((lo, "")) => ApiRange {
                min: number(lo)?,
                max: None,
            },
            Some((lo, hi)) => ApiRange {
                min: number(lo)?,
                max: Some(number(hi)?),
            },
        };
        if range.min == 0 || range.max.is_some_and(|m| m < range.min) {
            return Err(bad());
        }
        Ok(range)
    }

    pub fn contains(&self, version: u32) -> bool {
        version >= self.min && self.max.map_or(true, |m| version <= m)
    }
}

impl std::fmt::Display for ApiRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.max {
            Some(m) if m == self.min => write!(f, "{m}"),
            Some(m) => write!(f, "{}-{m}", self.min),
            None => write!(f, "{}-", self.min),
        }
    }
}

/// Optional liveness probe of a service plugin: a GET that must answer `2xx`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Health {
    pub url: String,
    pub interval_seconds: u64,
    pub timeout_seconds: u64,
}

/// How to build the plugin from this repository, read by the installer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Install {
    pub package: String,
    pub binary: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub name: String,
    pub version: String,
    pub description: String,
    pub api_version: ApiRange,
    pub kind: Kind,
    pub command: String,
    pub args: Vec<String>,
    /// Relative to the plugin folder.
    pub working_dir: Option<PathBuf>,
    /// Names of environment variables the plugin needs from the daemon's environment.
    pub env: Vec<String>,
    pub token: TokenDelivery,
    pub health: Option<Health>,
    pub capabilities: Vec<String>,
    pub install: Option<Install>,
}

/// Values substituted for `${name}` in a manifest's `args` and `health.url`.
#[derive(Debug, Clone)]
pub struct Placeholders {
    pub tickets_dir: PathBuf,
    pub api_url: String,
    pub token_file: PathBuf,
    pub plugin_dir: PathBuf,
}

impl Placeholders {
    pub fn expand(&self, text: &str) -> String {
        let mut out = text.to_string();
        for (name, value) in [
            ("tickets_dir", self.tickets_dir.display().to_string()),
            ("api_url", self.api_url.clone()),
            ("token_file", self.token_file.display().to_string()),
            ("plugin_dir", self.plugin_dir.display().to_string()),
        ] {
            out = out.replace(&format!("${{{name}}}"), &value);
        }
        out
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum RawVersion {
    Number(u64),
    Text(String),
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct RawHealth {
    url: String,
    interval_seconds: Option<u64>,
    timeout_seconds: Option<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawInstall {
    package: String,
    binary: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    name: String,
    version: String,
    #[serde(default)]
    description: String,
    api_version: RawVersion,
    kind: String,
    command: String,
    #[serde(default)]
    args: Vec<String>,
    working_dir: Option<PathBuf>,
    #[serde(default)]
    env: Vec<String>,
    token: Option<String>,
    health: Option<RawHealth>,
    #[serde(default)]
    capabilities: Vec<String>,
    install: Option<RawInstall>,
}

fn is_env_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn is_word(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// The `${...}` placeholders in `text` that are not known ones.
fn unknown_placeholder(text: &str) -> Option<String> {
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        let after = &rest[start + 2..];
        let end = after.find('}')?;
        let name = &after[..end];
        if !PLACEHOLDERS.contains(&name) {
            return Some(name.to_string());
        }
        rest = &after[end + 1..];
    }
    None
}

impl Manifest {
    pub fn parse(text: &str) -> Result<Manifest, String> {
        let raw: RawManifest = serde_yaml_ng::from_str(text).map_err(|e| {
            let at = e.location().map_or(String::new(), |l| {
                format!(" (line {}, column {})", l.line(), l.column())
            });
            format!("{e}{at}")
        })?;
        if !is_valid_name(&raw.name) {
            return Err(format!(
                "name `{}` must use lower-case letters, digits, `-` and `_`",
                raw.name
            ));
        }
        if raw.version.trim().is_empty() || raw.version.chars().any(char::is_whitespace) {
            return Err("version must be non-empty text without spaces".into());
        }
        let api_version = ApiRange::parse(&match raw.api_version {
            RawVersion::Number(n) => n.to_string(),
            RawVersion::Text(t) => t,
        })?;
        let kind = match raw.kind.as_str() {
            "stdio-client" => Kind::StdioClient,
            "service" => Kind::Service,
            other => {
                return Err(format!(
                    "kind `{other}` must be `stdio-client` or `service`"
                ))
            }
        };
        if raw.command.trim().is_empty() {
            return Err("command must not be empty".into());
        }
        for arg in &raw.args {
            if let Some(name) = unknown_placeholder(arg) {
                return Err(format!(
                    "args: unknown placeholder `${{{name}}}` (known: {})",
                    PLACEHOLDERS.join(", ")
                ));
            }
            if arg == "--token" || arg.starts_with("--token=") {
                return Err(
                    "args must not carry the token; read it from SIMPLETICKETS_TOKEN_FILE".into(),
                );
            }
        }
        if let Some(dir) = &raw.working_dir {
            use std::path::Component;
            let leaves_folder = dir.components().any(|c| {
                matches!(
                    c,
                    Component::ParentDir | Component::RootDir | Component::Prefix(_)
                )
            });
            if leaves_folder {
                return Err(
                    "working_dir must be relative to the plugin folder, without `..`".into(),
                );
            }
        }
        for name in &raw.env {
            if !is_env_name(name) {
                return Err(format!("env: `{name}` is not an environment variable name"));
            }
            if name.starts_with(RESERVED_ENV_PREFIX) {
                return Err(format!(
                    "env: `{name}` is set by the daemon and cannot be requested"
                ));
            }
        }
        let token = match raw.token.as_deref() {
            None | Some("file") => TokenDelivery::File,
            Some("env") => TokenDelivery::Env,
            Some(other) => return Err(format!("token `{other}` must be `file` or `env`")),
        };
        let health = match raw.health {
            None => None,
            Some(h) => {
                if kind == Kind::StdioClient {
                    return Err("health applies to services only".into());
                }
                if let Some(name) = unknown_placeholder(&h.url) {
                    return Err(format!("health.url: unknown placeholder `${{{name}}}`"));
                }
                let lower = h.url.to_ascii_lowercase();
                if !(lower.starts_with("http://") || lower.starts_with("https://")) {
                    return Err("health.url must be an http:// or https:// URL".into());
                }
                let (interval, timeout) = (
                    h.interval_seconds.unwrap_or(10),
                    h.timeout_seconds.unwrap_or(3),
                );
                if interval == 0 || timeout == 0 {
                    return Err("health intervals must be at least one second".into());
                }
                Some(Health {
                    url: h.url,
                    interval_seconds: interval,
                    timeout_seconds: timeout,
                })
            }
        };
        let mut capabilities: Vec<String> = Vec::new();
        for c in raw.capabilities {
            let ok = !c.is_empty()
                && c.chars().all(|ch| {
                    ch.is_ascii_lowercase() || ch.is_ascii_digit() || matches!(ch, '.' | '-' | ':')
                });
            if !ok {
                return Err(format!(
                    "capability `{c}` must be lower-case words like `tickets.read`"
                ));
            }
            if !capabilities.contains(&c) {
                capabilities.push(c);
            }
        }
        let install = match raw.install {
            None => None,
            Some(i) => {
                if !is_word(&i.package) || !is_word(&i.binary) {
                    return Err("install.package and install.binary must be plain names".into());
                }
                Some(Install {
                    package: i.package,
                    binary: i.binary,
                })
            }
        };
        Ok(Manifest {
            name: raw.name,
            version: raw.version,
            description: raw.description.trim().to_string(),
            api_version,
            kind,
            command: raw.command,
            args: raw.args,
            working_dir: raw.working_dir,
            env: raw.env,
            token,
            health,
            capabilities,
            install,
        })
    }

    /// The program to run: a path with separators is taken as is (relative ones against the
    /// plugin folder); a bare name is looked for in the plugin folder, then in `bin_dir`
    /// (where the daemon itself lives), and otherwise left to the `PATH` search.
    pub fn resolve_command(&self, plugin_dir: &Path, bin_dir: Option<&Path>) -> PathBuf {
        let given = Path::new(&self.command);
        if given.is_absolute() {
            return given.to_path_buf();
        }
        if self.command.contains(['/', '\\']) {
            return plugin_dir.join(given);
        }
        let exe = format!("{}{}", self.command, std::env::consts::EXE_SUFFIX);
        for dir in std::iter::once(plugin_dir).chain(bin_dir) {
            for name in [self.command.as_str(), exe.as_str()] {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    return candidate;
                }
            }
        }
        PathBuf::from(&self.command)
    }

    /// True when [`Manifest::resolve_command`] points at an existing file or a bare name
    /// found on the `PATH`.
    pub fn command_exists(&self, plugin_dir: &Path, bin_dir: Option<&Path>) -> bool {
        let path = self.resolve_command(plugin_dir, bin_dir);
        if path.components().count() > 1 {
            return path.is_file();
        }
        std::env::var_os("PATH").is_some_and(|paths| {
            std::env::split_paths(&paths).any(|dir| {
                dir.join(&path).is_file()
                    || dir
                        .join(format!("{}{}", self.command, std::env::consts::EXE_SUFFIX))
                        .is_file()
            })
        })
    }

    pub fn expanded_args(&self, values: &Placeholders) -> Vec<String> {
        self.args.iter().map(|a| values.expand(a)).collect()
    }

    pub fn to_json(&self) -> Value {
        json!({
            "name": self.name,
            "version": self.version,
            "description": self.description,
            "api_version": self.api_version.to_string(),
            "kind": self.kind.as_str(),
            "command": self.command,
            "args": self.args,
            "working_dir": self.working_dir.as_ref().map(|p| p.display().to_string()),
            "env": self.env,
            "token": match self.token { TokenDelivery::File => "file", TokenDelivery::Env => "env" },
            "health": self.health.as_ref().map(|h| json!({
                "url": h.url,
                "interval_seconds": h.interval_seconds,
                "timeout_seconds": h.timeout_seconds,
            })),
            "capabilities": self.capabilities,
        })
    }
}

/// Where a plugin folder was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    /// `<tickets folder>/.simpletickets/plugins`
    Tickets,
    /// The per-user plugins folder.
    User,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Tickets => "tickets",
            Source::User => "user",
        }
    }
}

/// A plugin folder and what its manifest said.
#[derive(Debug, Clone)]
pub struct Found {
    /// The folder name.
    pub name: String,
    pub dir: PathBuf,
    pub source: Source,
    pub manifest: Result<Manifest, String>,
}

/// The per-user plugins folder: `$SIMPLETICKETS_PLUGINS_DIR`, else
/// `$XDG_CONFIG_HOME/simpletickets/plugins` or `~/.config/simpletickets/plugins`, and on
/// Windows `%APPDATA%\simpletickets\plugins`.
pub fn user_plugins_dir() -> Option<PathBuf> {
    let var = |name: &str| std::env::var_os(name).filter(|v| !v.is_empty());
    if let Some(dir) = var(USER_DIR_ENV) {
        return Some(PathBuf::from(dir));
    }
    let base = if cfg!(windows) {
        var("APPDATA").map(PathBuf::from)
    } else {
        var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .or_else(|| var("HOME").map(|h| PathBuf::from(h).join(".config")))
    };
    base.map(|b| b.join("simpletickets").join("plugins"))
}

/// The folders searched, in priority order: the tickets folder's, then `user`'s.
pub fn search_dirs(tickets_dir: &Path, user: Option<PathBuf>) -> Vec<(Source, PathBuf)> {
    let mut dirs = vec![(Source::Tickets, plugins_dir(tickets_dir))];
    dirs.extend(user.map(|u| (Source::User, u)));
    dirs
}

/// Reads every `<name>/plugin.yml` below the given folders. A name found in more than one
/// folder resolves to the first. The result is sorted by name.
pub fn discover(dirs: &[(Source, PathBuf)]) -> Vec<Found> {
    let mut found: BTreeMap<String, Found> = BTreeMap::new();
    for (source, root) in dirs {
        let Ok(entries) = fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let dir = entry.path();
            let manifest_path = dir.join(MANIFEST_FILE);
            let Some(name) = dir.file_name().and_then(|n| n.to_str()).map(String::from) else {
                continue;
            };
            if !manifest_path.is_file() || found.contains_key(&name) {
                continue;
            }
            let manifest = fs::read_to_string(&manifest_path)
                .map_err(|e| format!("cannot read {}: {e}", manifest_path.display()))
                .and_then(|text| Manifest::parse(&text))
                .and_then(|m| {
                    if m.name == name {
                        Ok(m)
                    } else {
                        Err(format!(
                            "name `{}` does not match the folder name `{name}`",
                            m.name
                        ))
                    }
                });
            found.insert(
                name.clone(),
                Found {
                    name,
                    dir,
                    source: *source,
                    manifest,
                },
            );
        }
    }
    found.into_values().collect()
}

/// `text` with the `plugins:` block replaced by one listing `enabled` (or appended when
/// there was none); everything else, comments included, stays as written.
pub fn config_text_with_enabled(text: &str, enabled: &[String]) -> String {
    let mut block = String::from("plugins:\n");
    if enabled.is_empty() {
        block.push_str("  enabled: []\n");
    } else {
        block.push_str("  enabled:\n");
        for name in enabled {
            block.push_str(&format!("    - {name}\n"));
        }
    }
    let lines: Vec<&str> = text.lines().collect();
    let Some(start) = lines.iter().position(|l| l.starts_with("plugins:")) else {
        let mut out = text.to_string();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&block);
        return out;
    };
    let mut end = start + 1;
    let mut last = start;
    while end < lines.len() {
        let line = lines[end];
        if line.starts_with([' ', '\t']) {
            last = end;
        } else if !line.trim().is_empty() {
            break;
        }
        end += 1;
    }
    let mut out = String::new();
    for line in &lines[..start] {
        out.push_str(line);
        out.push('\n');
    }
    out.push_str(&block);
    for line in &lines[last + 1..] {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Rewrites the config file in `dir` so that exactly `enabled` plugins are enabled.
pub fn write_enabled(dir: &Path, enabled: &[String]) -> Result<(), String> {
    let path = dir.join(CONFIG_FILE);
    let current = match fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(format!("{}: {e}", path.display())),
    };
    let updated = config_text_with_enabled(&current, enabled);
    let parsed = Config::parse(&updated, dir).map_err(|e| e.to_string())?;
    if parsed.plugins != enabled {
        return Err(format!(
            "cannot edit the `plugins` section of {}; change it by hand",
            path.display()
        ));
    }
    crate::config::write_file(dir, &updated).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MCP: &str = "name: mcp\nversion: 0.1.0\ndescription: MCP server\napi_version: 1\n\
                       kind: stdio-client\ncommand: simpletickets-mcp\n";

    #[test]
    fn parses_a_full_manifest() {
        let m = Manifest::parse(
            "name: flow\nversion: 1.2.3\ndescription: Automates\napi_version: \"1-2\"\n\
             kind: service\ncommand: flow-plugin\nargs: [\"--dir\", \"${tickets_dir}\"]\n\
             working_dir: run\nenv: [FLOW_KEY]\ntoken: env\n\
             health:\n  url: http://127.0.0.1:9100/health\n  interval_seconds: 5\n\
             capabilities: [tickets.read, tickets.write, tickets.read]\n\
             install:\n  package: flow-crate\n  binary: flow-plugin\n",
        )
        .unwrap();
        assert_eq!(m.kind, Kind::Service);
        assert_eq!(
            m.api_version,
            ApiRange {
                min: 1,
                max: Some(2)
            }
        );
        assert_eq!(m.token, TokenDelivery::Env);
        assert_eq!(m.capabilities, ["tickets.read", "tickets.write"]);
        let health = m.health.unwrap();
        assert_eq!((health.interval_seconds, health.timeout_seconds), (5, 3));
        assert_eq!(m.install.unwrap().package, "flow-crate");
        assert_eq!(m.working_dir, Some(PathBuf::from("run")));
    }

    #[test]
    fn minimal_manifest_has_defaults() {
        let m = Manifest::parse(MCP).unwrap();
        assert_eq!(m.token, TokenDelivery::File);
        assert!(m.args.is_empty() && m.env.is_empty() && m.health.is_none());
        assert_eq!(
            m.api_version,
            ApiRange {
                min: 1,
                max: Some(1)
            }
        );
    }

    #[test]
    fn rejects_invalid_manifests() {
        let bad = |extra: &str| Manifest::parse(&format!("{MCP}{extra}")).unwrap_err();
        assert!(bad("args: [\"--token\", \"x\"]\n").contains("token"));
        assert!(bad("args: [\"--token=abc\"]\n").contains("token"));
        assert!(bad("args: [\"${token}\"]\n").contains("placeholder"));
        assert!(bad("env: [SIMPLETICKETS_TOKEN]\n").contains("daemon"));
        assert!(bad("env: [\"1X\"]\n").contains("environment"));
        assert!(bad("working_dir: ../x\n").contains("working_dir"));
        assert!(bad("working_dir: /abs\n").contains("working_dir"));
        assert!(bad("token: argv\n").contains("token"));
        assert!(bad("health:\n  url: http://x/\n").contains("services only"));
        assert!(bad("capabilities: [\"Bad Cap\"]\n").contains("capability"));
        assert!(bad("surprise: 1\n").contains("surprise"));
        let other = |text: &str| Manifest::parse(text).unwrap_err();
        assert!(
            other("name: Bad\nversion: 1\napi_version: 1\nkind: service\ncommand: x\n")
                .contains("name")
        );
        assert!(
            other("name: a\nversion: 1\napi_version: 1\nkind: daemon\ncommand: x\n")
                .contains("kind")
        );
        assert!(
            other("name: a\nversion: 1\napi_version: 0\nkind: service\ncommand: x\n")
                .contains("api_version")
        );
        assert!(
            other("name: a\nversion: 1\napi_version: 1\nkind: service\ncommand: \" \"\n")
                .contains("command")
        );
        let svc = "name: a\nversion: 1\napi_version: 1\nkind: service\ncommand: x\n";
        assert!(other(&format!("{svc}health:\n  url: ftp://x\n")).contains("http"));
        assert!(other(&format!(
            "{svc}health:\n  url: http://x\n  interval_seconds: 0\n"
        ))
        .contains("second"));
    }

    #[test]
    fn api_ranges() {
        let r = ApiRange::parse("1-2").unwrap();
        assert!(r.contains(1) && r.contains(2) && !r.contains(3) && !r.contains(0));
        assert!(ApiRange::parse("2-").unwrap().contains(9));
        assert!(!ApiRange::parse("2").unwrap().contains(1));
        assert_eq!(ApiRange::parse("1-2").unwrap().to_string(), "1-2");
        assert_eq!(ApiRange::parse("3").unwrap().to_string(), "3");
        assert_eq!(ApiRange::parse("3-").unwrap().to_string(), "3-");
        for bad in ["", "x", "2-1", "0", "1-x", "-2"] {
            assert!(ApiRange::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn expands_placeholders() {
        let m = Manifest::parse(&format!(
            "{MCP}args: [\"--url\", \"${{api_url}}\", \"--dir=${{tickets_dir}}\"]\n"
        ))
        .unwrap();
        let values = Placeholders {
            tickets_dir: PathBuf::from("/t"),
            api_url: "http://127.0.0.1:1".into(),
            token_file: PathBuf::from("/t/token"),
            plugin_dir: PathBuf::from("/p"),
        };
        assert_eq!(
            m.expanded_args(&values),
            ["--url", "http://127.0.0.1:1", "--dir=/t"]
        );
    }

    #[test]
    fn resolves_commands() {
        let dir = tempfile::tempdir().unwrap();
        let (plugin, bin) = (dir.path().join("p"), dir.path().join("b"));
        fs::create_dir_all(&plugin).unwrap();
        fs::create_dir_all(&bin).unwrap();
        let manifest = |command: &str| {
            Manifest::parse(&format!(
                "name: a\nversion: 1\napi_version: 1\nkind: service\ncommand: \"{command}\"\n"
            ))
            .unwrap()
        };
        let m = manifest("tool");
        assert_eq!(
            m.resolve_command(&plugin, Some(&bin)),
            PathBuf::from("tool")
        );
        fs::write(bin.join("tool"), "").unwrap();
        assert_eq!(m.resolve_command(&plugin, Some(&bin)), bin.join("tool"));
        assert!(m.command_exists(&plugin, Some(&bin)));
        fs::write(plugin.join("tool"), "").unwrap();
        assert_eq!(m.resolve_command(&plugin, Some(&bin)), plugin.join("tool"));
        assert_eq!(
            manifest("bin/run").resolve_command(&plugin, None),
            plugin.join("bin/run")
        );
        assert!(!manifest("bin/run").command_exists(&plugin, None));
    }

    fn plugin_folder(root: &Path, name: &str, text: &str) {
        fs::create_dir_all(root.join(name)).unwrap();
        fs::write(root.join(name).join(MANIFEST_FILE), text).unwrap();
    }

    #[test]
    fn discovers_in_priority_order_and_reports_errors() {
        let dir = tempfile::tempdir().unwrap();
        let (tickets, user) = (dir.path().join("tickets"), dir.path().join("user"));
        let local = plugins_dir(&tickets);
        plugin_folder(&local, "mcp", MCP);
        plugin_folder(&user, "mcp", &MCP.replace("0.1.0", "9.9.9"));
        plugin_folder(&user, "other", &MCP.replace("name: mcp", "name: other"));
        plugin_folder(&user, "broken", "name: [");
        plugin_folder(&user, "renamed", MCP);
        fs::create_dir_all(user.join("no-manifest")).unwrap();
        let found = discover(&search_dirs(&tickets, Some(user)));
        let names: Vec<&str> = found.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["broken", "mcp", "other", "renamed"]);
        let mcp = &found[1];
        assert_eq!(mcp.source, Source::Tickets);
        assert_eq!(mcp.manifest.as_ref().unwrap().version, "0.1.0");
        assert!(found[0].manifest.is_err());
        assert!(found[3]
            .manifest
            .as_ref()
            .unwrap_err()
            .contains("does not match"));
        assert_eq!(found[2].source, Source::User);
    }

    #[test]
    fn rewrites_the_plugins_block_only() {
        let names = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let text = "id_prefix: ST # keep\nplugins:\n  enabled: [a]\n  # note\nwebhooks: []\n";
        let out = config_text_with_enabled(text, &names(&["a", "b"]));
        assert_eq!(
            out,
            "id_prefix: ST # keep\nplugins:\n  enabled:\n    - a\n    - b\nwebhooks: []\n"
        );
        assert_eq!(
            config_text_with_enabled("id_prefix: ST", &names(&["x"])),
            "id_prefix: ST\nplugins:\n  enabled:\n    - x\n"
        );
        assert_eq!(
            config_text_with_enabled("plugins: {enabled: [a]}\n", &[]),
            "plugins:\n  enabled: []\n"
        );
    }

    #[test]
    fn writes_enabled_plugins_to_the_config() {
        let dir = tempfile::tempdir().unwrap();
        write_enabled(dir.path(), &["mcp".to_string()]).unwrap();
        let config = Config::load_from_dir(dir.path()).unwrap();
        assert_eq!(config.plugins, ["mcp"]);
        write_enabled(dir.path(), &[]).unwrap();
        assert!(Config::load_from_dir(dir.path())
            .unwrap()
            .plugins
            .is_empty());
        assert!(write_enabled(dir.path(), &["Bad Name".to_string()]).is_err());
    }
}
