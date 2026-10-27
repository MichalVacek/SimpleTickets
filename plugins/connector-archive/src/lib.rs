//! The archive connector: one file per ticket in a folder. It is the reference
//! implementation of the connector protocol and the test double for the sync plugin.
//!
//! Options: `dir` (required; relative paths are resolved against the tickets folder) and
//! `format` (`json`, the default, or `markdown`). A JSON archive supports `fetch`, so a
//! hand edit of a file's `fields` is seen as a remote change; Markdown files are write-only.

use serde_json::{json, Map, Value};
use simpletickets_connector_protocol::{unsupported, Connector, ErrorCode, ErrorInfo};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct ArchiveConnector {
    dir: Option<PathBuf>,
    markdown: bool,
}

fn internal(context: &str, e: impl std::fmt::Display) -> ErrorInfo {
    ErrorInfo::new(ErrorCode::Internal, format!("{context}: {e}"))
}

/// The file stem for a ticket id: anything outside letters, digits, `-`, `_` and `.` becomes
/// `_`, and a leading dot is replaced, so an id can never leave the folder.
fn stem(id: &str) -> String {
    let cleaned: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .collect();
    match cleaned.strip_prefix('.') {
        Some(rest) => format!("_{rest}"),
        None => cleaned,
    }
}

/// A short stable digest (FNV-1a) used as the version of a file's `fields`.
fn digest(text: &str) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.bytes() {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0100_0000_01b3);
    }
    format!("{hash:016x}")
}

fn write_atomic(path: &Path, text: &str) -> Result<(), ErrorInfo> {
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, text).map_err(|e| internal("cannot write the archive file", e))?;
    fs::rename(&tmp, path).map_err(|e| internal("cannot replace the archive file", e))
}

fn markdown(ticket: &Value, fields: &Value) -> String {
    let mut out = format!("# {}\n\n", ticket["title"].as_str().unwrap_or(""));
    out.push_str(&format!("- id: {}\n", ticket["id"].as_str().unwrap_or("")));
    if let Some(map) = fields.as_object() {
        for (key, value) in map {
            let text = match value {
                Value::Null => String::new(),
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            out.push_str(&format!("- {key}: {text}\n"));
        }
    }
    out.push('\n');
    out.push_str(ticket["body"].as_str().unwrap_or(""));
    out
}

impl ArchiveConnector {
    fn dir(&self) -> Result<&Path, ErrorInfo> {
        self.dir
            .as_deref()
            .ok_or_else(|| ErrorInfo::invalid("`configure` has not been called"))
    }

    fn path(&self, stem: &str) -> Result<PathBuf, ErrorInfo> {
        let ext = if self.markdown { "md" } else { "json" };
        Ok(self.dir()?.join(format!("{stem}.{ext}")))
    }

    fn configure(&mut self, params: &Value) -> Result<Value, ErrorInfo> {
        let options = &params["options"];
        let dir = options["dir"]
            .as_str()
            .filter(|d| !d.is_empty())
            .ok_or_else(|| ErrorInfo::invalid("option `dir` is required"))?;
        let mut path = PathBuf::from(dir);
        if path.is_relative() {
            if let Some(base) = params["tickets_dir"].as_str() {
                path = Path::new(base).join(path);
            }
        }
        self.markdown = match options["format"].as_str().unwrap_or("json") {
            "json" => false,
            "markdown" => true,
            other => {
                return Err(ErrorInfo::invalid(format!(
                    "option `format` must be `json` or `markdown`, not `{other}`"
                )))
            }
        };
        self.dir = Some(path);
        let mut capabilities = vec!["close"];
        if !self.markdown {
            capabilities.push("fetch");
        }
        Ok(json!({"capabilities": capabilities}))
    }

    fn push(&self, params: &Value) -> Result<Value, ErrorInfo> {
        let ticket = &params["ticket"];
        let id = ticket["id"]
            .as_str()
            .ok_or_else(|| ErrorInfo::invalid("`ticket.id` is required"))?;
        let stem = stem(id);
        let path = self.path(&stem)?;
        let creating = params["ref"].is_null();
        if !creating && params["ref"].as_str() != Some(stem.as_str()) {
            return Err(ErrorInfo::new(
                ErrorCode::NotFound,
                format!("`{}` is not the archive reference of {id}", params["ref"]),
            ));
        }
        if !creating && !path.exists() {
            return Err(ErrorInfo::new(
                ErrorCode::NotFound,
                format!("{} no longer exists", path.display()),
            ));
        }
        let existed = path.exists();
        let fields = if params["fields"].is_object() {
            params["fields"].clone()
        } else {
            json!({})
        };
        fs::create_dir_all(self.dir()?).map_err(|e| internal("cannot create the folder", e))?;
        let text = if self.markdown {
            markdown(ticket, &fields)
        } else {
            let document = json!({
                "ref": stem,
                "closed": false,
                "ticket": ticket,
                "fields": fields,
                "links": params["links"],
            });
            serde_json::to_string_pretty(&document).unwrap_or_default() + "\n"
        };
        let unchanged = fs::read_to_string(&path).is_ok_and(|old| old == text);
        if !unchanged {
            write_atomic(&path, &text)?;
        }
        let outcome = match (existed, unchanged) {
            (false, _) => "created",
            (true, true) => "unchanged",
            (true, false) => "updated",
        };
        Ok(json!({
            "ref": stem,
            "url": path.display().to_string(),
            "outcome": outcome,
            "fields": fields,
            "warnings": [],
        }))
    }

    fn close(&self, params: &Value) -> Result<Value, ErrorInfo> {
        let reference = params["ref"]
            .as_str()
            .ok_or_else(|| ErrorInfo::invalid("`ref` is required"))?;
        let path = self.path(&stem(reference))?;
        let text = fs::read_to_string(&path).map_err(|_| {
            ErrorInfo::new(
                ErrorCode::NotFound,
                format!("{} does not exist", path.display()),
            )
        })?;
        if self.markdown {
            if !text.starts_with("[closed]") {
                write_atomic(&path, &format!("[closed]\n{text}"))?;
            }
            return Ok(json!({}));
        }
        let mut document: Value =
            serde_json::from_str(&text).map_err(|e| internal("unreadable archive file", e))?;
        document["closed"] = json!(true);
        let text = serde_json::to_string_pretty(&document).unwrap_or_default() + "\n";
        write_atomic(&path, &text)?;
        Ok(json!({}))
    }

    fn fetch(&self) -> Result<Value, ErrorInfo> {
        if self.markdown {
            return Err(unsupported("fetch"));
        }
        let mut items = Vec::new();
        let entries = match fs::read_dir(self.dir()?) {
            Ok(entries) => entries,
            Err(_) => return Ok(json!({"items": [], "cursor": null, "more": false})),
        };
        let mut paths: Vec<PathBuf> = entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "json"))
            .collect();
        paths.sort();
        for path in paths {
            let Ok(text) = fs::read_to_string(&path) else {
                continue;
            };
            let Ok(document) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            let Some(reference) = document["ref"].as_str() else {
                continue;
            };
            let fields = document["fields"].clone();
            let mut map = Map::new();
            map.insert("ref".into(), json!(reference));
            map.insert("version".into(), json!(digest(&fields.to_string())));
            map.insert("closed".into(), document["closed"].clone());
            map.insert("fields".into(), fields);
            items.push(Value::Object(map));
        }
        Ok(json!({"items": items, "cursor": null, "more": false}))
    }
}

impl Connector for ArchiveConnector {
    fn describe(&self) -> Value {
        json!({
            "name": "archive",
            "version": env!("CARGO_PKG_VERSION"),
            "capabilities": ["close", "fetch", "check"],
        })
    }

    fn handle(&mut self, op: &str, params: &Value) -> Result<Value, ErrorInfo> {
        match op {
            "configure" => self.configure(params),
            "push" => self.push(params),
            "close" => self.close(params),
            "fetch" => self.fetch(),
            "check" => {
                let dir = self.dir()?;
                Ok(json!({"message": format!("archive folder {}", dir.display())}))
            }
            other => Err(unsupported(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connector(dir: &Path, format: &str) -> ArchiveConnector {
        let mut c = ArchiveConnector::default();
        c.handle(
            "configure",
            &json!({"options": {"dir": dir.to_str().unwrap(), "format": format}}),
        )
        .unwrap();
        c
    }

    fn push(c: &mut ArchiveConnector, id: &str, reference: Value, status: &str) -> Value {
        c.handle(
            "push",
            &json!({
                "ticket": {"id": id, "title": "T", "body": "text\n"},
                "fields": {"status": status},
                "ref": reference,
            }),
        )
        .unwrap()
    }

    #[test]
    fn stems_cannot_escape_the_folder() {
        assert_eq!(stem("ST-1"), "ST-1");
        assert_eq!(stem("../x/y"), "_._x_y");
        assert_eq!(stem(".hidden"), "_hidden");
    }

    #[test]
    fn push_creates_updates_and_reports_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = connector(dir.path(), "json");
        let created = push(&mut c, "ST-1", Value::Null, "todo");
        assert_eq!(created["outcome"], "created");
        assert_eq!(created["ref"], "ST-1");
        assert!(dir.path().join("ST-1.json").is_file());
        assert_eq!(
            push(&mut c, "ST-1", json!("ST-1"), "todo")["outcome"],
            "unchanged"
        );
        assert_eq!(
            push(&mut c, "ST-1", json!("ST-1"), "done")["outcome"],
            "updated"
        );
        assert_eq!(
            push(&mut c, "ST-1", Value::Null, "done")["outcome"],
            "unchanged"
        );
    }

    #[test]
    fn push_to_a_missing_file_is_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = connector(dir.path(), "json");
        let e = c
            .handle(
                "push",
                &json!({"ticket": {"id": "ST-2"}, "fields": {}, "ref": "ST-2"}),
            )
            .unwrap_err();
        assert_eq!(e.code, ErrorCode::NotFound);
    }

    #[test]
    fn fetch_sees_hand_edits_through_the_version() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = connector(dir.path(), "json");
        push(&mut c, "ST-1", Value::Null, "todo");
        let first = c.handle("fetch", &json!({})).unwrap();
        let item = &first["items"][0];
        assert_eq!(item["fields"]["status"], "todo");
        let path = dir.path().join("ST-1.json");
        let edited = fs::read_to_string(&path)
            .unwrap()
            .replace("\"todo\"", "\"doing\"");
        fs::write(&path, edited).unwrap();
        let second = c.handle("fetch", &json!({})).unwrap();
        assert_eq!(second["items"][0]["fields"]["status"], "doing");
        assert_ne!(second["items"][0]["version"], item["version"]);
    }

    #[test]
    fn close_marks_the_file_and_never_deletes_it() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = connector(dir.path(), "json");
        push(&mut c, "ST-1", Value::Null, "todo");
        c.handle("close", &json!({"ref": "ST-1"})).unwrap();
        let text = fs::read_to_string(dir.path().join("ST-1.json")).unwrap();
        assert!(text.contains("\"closed\": true"));
        let e = c.handle("close", &json!({"ref": "ST-9"})).unwrap_err();
        assert_eq!(e.code, ErrorCode::NotFound);
    }

    #[test]
    fn markdown_files_are_write_only() {
        let dir = tempfile::tempdir().unwrap();
        let mut c = connector(dir.path(), "markdown");
        push(&mut c, "ST-1", Value::Null, "todo");
        let text = fs::read_to_string(dir.path().join("ST-1.md")).unwrap();
        assert!(text.starts_with("# T\n"));
        assert!(text.contains("- status: todo"));
        assert!(text.ends_with("text\n"));
        assert_eq!(
            c.handle("fetch", &json!({})).unwrap_err().code,
            ErrorCode::Unsupported
        );
    }

    #[test]
    fn configure_validates_options() {
        let mut c = ArchiveConnector::default();
        assert!(c.handle("configure", &json!({"options": {}})).is_err());
        assert!(c
            .handle(
                "configure",
                &json!({"options": {"dir": "x", "format": "xml"}})
            )
            .is_err());
        assert!(c.handle("push", &json!({"ticket": {"id": "A"}})).is_err());
    }
}
