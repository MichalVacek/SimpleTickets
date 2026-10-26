//! The ticket model and its mapping to and from Markdown frontmatter.

use crate::frontmatter::{self, Document, FrontmatterError, Value};
use serde_yaml_ng::{Mapping, Value as Yaml};
use std::fmt;

/// Frontmatter keys owned by the core; every other key is preserved untouched.
pub const OWNED_KEYS: [&str; 9] = [
    "id",
    "title",
    "status",
    "priority",
    "due",
    "tags",
    "parent",
    "blocked_by",
    "related",
];

#[derive(Debug, Clone, PartialEq)]
pub struct Ticket {
    pub id: String,
    pub title: String,
    pub status: String,
    pub priority: Option<String>,
    pub due: Option<String>,
    pub tags: Vec<String>,
    pub parent: Option<String>,
    pub blocked_by: Vec<String>,
    pub related: Vec<String>,
    /// Frontmatter keys the core does not own, in file order.
    pub extra: Mapping,
}

/// A frontmatter problem, with the one-based file line when it is known.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub line: Option<usize>,
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.line {
            Some(line) => write!(f, "line {line}: {}", self.message),
            None => f.write_str(&self.message),
        }
    }
}

impl std::error::Error for ParseError {}

impl From<FrontmatterError> for ParseError {
    fn from(e: FrontmatterError) -> Self {
        let line = match e {
            FrontmatterError::Missing | FrontmatterError::Unclosed => Some(1),
            FrontmatterError::MultilineValue(_) => None,
        };
        ParseError {
            line,
            message: e.to_string(),
        }
    }
}

impl Ticket {
    /// Parses a complete Markdown file.
    pub fn parse(text: &str) -> Result<Ticket, ParseError> {
        let doc = Document::parse(text)?;
        Ticket::from_document(&doc)
    }

    pub fn from_document(doc: &Document) -> Result<Ticket, ParseError> {
        let fm = doc.frontmatter();
        let offset = doc.first_frontmatter_line() - 1;
        let yaml: Yaml = serde_yaml_ng::from_str(fm).map_err(|e| ParseError {
            line: e.location().map(|l| l.line() + offset),
            message: e.to_string(),
        })?;
        let mut map = match yaml {
            Yaml::Null => Mapping::new(),
            Yaml::Mapping(m) => m,
            _ => {
                return Err(ParseError {
                    line: Some(doc.first_frontmatter_line()),
                    message: "frontmatter must be a mapping of keys to values".into(),
                })
            }
        };

        let fields = Fields { fm, offset };
        let id = fields.required_scalar(&mut map, "id")?;
        let title = fields.required_scalar(&mut map, "title")?;
        let status = fields.required_scalar(&mut map, "status")?;
        let priority = fields.optional_scalar(&mut map, "priority")?;
        let due = fields.optional_scalar(&mut map, "due")?;
        let tags = fields.list(&mut map, "tags")?;
        let parent = fields.optional_scalar(&mut map, "parent")?;
        let blocked_by = fields.list(&mut map, "blocked_by")?;
        let related = fields.list(&mut map, "related")?;

        Ok(Ticket {
            id,
            title,
            status,
            priority,
            due,
            tags,
            parent,
            blocked_by,
            related,
            extra: map,
        })
    }

    /// Renders a new ticket file from this ticket and a body.
    pub fn render_new(&self, body: &str) -> Result<String, FrontmatterError> {
        let mut doc = Document::parse("---\n---\n")?;
        for (key, value) in self.owned_values() {
            doc.set(key, value.as_ref())?;
        }
        let mut text = doc.into_text();
        if !self.extra.is_empty() {
            let extra = serde_yaml_ng::to_string(&self.extra).expect("mapping serializes");
            let at = text.len() - "---\n".len();
            text.insert_str(at, &extra);
        }
        text.push_str(body);
        Ok(text)
    }

    /// The owned keys as frontmatter values; `None` means the key is absent.
    pub fn owned_values(&self) -> Vec<(&'static str, Option<Value>)> {
        let scalar = |s: &str| Some(Value::Scalar(s.to_string()));
        let opt = |s: &Option<String>| s.as_deref().and_then(scalar);
        let list = |l: &[String]| (!l.is_empty()).then(|| Value::List(l.to_vec()));
        vec![
            ("id", scalar(&self.id)),
            ("title", scalar(&self.title)),
            ("status", scalar(&self.status)),
            ("priority", opt(&self.priority)),
            ("due", opt(&self.due)),
            ("tags", list(&self.tags)),
            ("parent", opt(&self.parent)),
            ("blocked_by", list(&self.blocked_by)),
            ("related", list(&self.related)),
        ]
    }
}

/// True when the frontmatter carries the keys that mark a file as a ticket.
///
/// Used in repository roots, where ordinary Markdown files must be ignored.
pub fn has_ticket_marker(text: &str) -> bool {
    match Document::parse(text) {
        Ok(doc) => {
            frontmatter::has_key(doc.frontmatter(), "id")
                && frontmatter::has_key(doc.frontmatter(), "status")
        }
        Err(_) => false,
    }
}

struct Fields<'a> {
    fm: &'a str,
    offset: usize,
}

impl Fields<'_> {
    fn error(&self, key: &str, message: String) -> ParseError {
        ParseError {
            line: frontmatter::key_line(self.fm, key).map(|l| l + self.offset),
            message,
        }
    }

    fn scalar(&self, key: &str, value: Yaml) -> Result<String, ParseError> {
        match value {
            Yaml::String(s) => Ok(s),
            Yaml::Number(n) => Ok(n.to_string()),
            _ => Err(self.error(key, format!("`{key}` must be a text or number value"))),
        }
    }

    fn required_scalar(&self, map: &mut Mapping, key: &str) -> Result<String, ParseError> {
        match map.remove(key) {
            None | Some(Yaml::Null) => Err(ParseError {
                line: None,
                message: format!("missing required key `{key}`"),
            }),
            Some(v) => {
                let s = self.scalar(key, v)?;
                if s.trim().is_empty() {
                    return Err(self.error(key, format!("`{key}` must not be empty")));
                }
                Ok(s)
            }
        }
    }

    fn optional_scalar(&self, map: &mut Mapping, key: &str) -> Result<Option<String>, ParseError> {
        match map.remove(key) {
            None | Some(Yaml::Null) => Ok(None),
            Some(v) => self.scalar(key, v).map(Some),
        }
    }

    fn list(&self, map: &mut Mapping, key: &str) -> Result<Vec<String>, ParseError> {
        match map.remove(key) {
            None | Some(Yaml::Null) => Ok(Vec::new()),
            Some(Yaml::Sequence(items)) => items.into_iter().map(|v| self.scalar(key, v)).collect(),
            Some(_) => Err(self.error(key, format!("`{key}` must be a list"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = "---\n\
id: ST-1\n\
title: Write report\n\
status: todo\n\
priority: high\n\
due: 2026-11-05\n\
tags: [work, report]\n\
parent: ST-0\n\
blocked_by:\n  - ST-2\n\
related: [ST-3]\n\
custom: {a: 1}\n\
# trailing comment\n\
---\n\
# Heading\n\nBody text\n";

    #[test]
    fn parses_all_fields() {
        let t = Ticket::parse(FULL).unwrap();
        assert_eq!(t.id, "ST-1");
        assert_eq!(t.title, "Write report");
        assert_eq!(t.status, "todo");
        assert_eq!(t.priority.as_deref(), Some("high"));
        assert_eq!(t.due.as_deref(), Some("2026-11-05"));
        assert_eq!(t.tags, ["work", "report"]);
        assert_eq!(t.parent.as_deref(), Some("ST-0"));
        assert_eq!(t.blocked_by, ["ST-2"]);
        assert_eq!(t.related, ["ST-3"]);
        assert_eq!(t.extra.len(), 1);
        assert!(t.extra.contains_key("custom"));
    }

    #[test]
    fn minimal_ticket_and_numeric_scalars() {
        let t = Ticket::parse("---\nid: 7\ntitle: T\nstatus: todo\npriority: 2\n---\n").unwrap();
        assert_eq!(t.id, "7");
        assert_eq!(t.priority.as_deref(), Some("2"));
        assert!(t.tags.is_empty());
    }

    #[test]
    fn reports_missing_keys() {
        let e = Ticket::parse("---\nid: A\nstatus: todo\n---\n").unwrap_err();
        assert!(e.message.contains("title"), "{e}");
    }

    #[test]
    fn reports_yaml_error_with_file_line() {
        let e = Ticket::parse("---\nid: A\ntitle: [oops\nstatus: todo\n---\n").unwrap_err();
        assert!(e.line.is_some());
        assert!(e.line.unwrap() >= 3, "{e}");
    }

    #[test]
    fn reports_type_error_at_key_line() {
        let e = Ticket::parse("---\nid: A\ntitle: T\nstatus: todo\ntags: nope\n---\n").unwrap_err();
        assert_eq!(e.line, Some(5));
    }

    #[test]
    fn reports_duplicate_keys() {
        let e = Ticket::parse("---\nid: A\nid: B\ntitle: T\nstatus: todo\n---\n").unwrap_err();
        assert!(e.line.is_some(), "{e}");
    }

    #[test]
    fn rejects_non_mapping_and_missing_frontmatter() {
        assert!(Ticket::parse("---\n- a\n---\n").is_err());
        assert!(Ticket::parse("no frontmatter").is_err());
    }

    #[test]
    fn patching_owned_key_preserves_everything_else() {
        let mut doc = Document::parse(FULL).unwrap();
        doc.set("status", Some(&Value::Scalar("done".into())))
            .unwrap();
        assert_eq!(doc.text(), FULL.replace("status: todo", "status: done"));
        assert_eq!(doc.body(), "# Heading\n\nBody text\n");
        let t = Ticket::from_document(&doc).unwrap();
        assert_eq!(t.status, "done");
        assert_eq!(t.extra.len(), 1);
    }

    #[test]
    fn render_new_round_trips() {
        let mut t = Ticket::parse(FULL).unwrap();
        t.tags = vec!["a: b".into()];
        let text = t.render_new("\nbody\n").unwrap();
        assert_eq!(Ticket::parse(&text).unwrap(), t);
        assert!(text.ends_with("---\n\nbody\n"));
    }

    #[test]
    fn render_new_minimal_has_no_empty_lists() {
        let t = Ticket::parse("---\nid: A\ntitle: T\nstatus: todo\n---\n").unwrap();
        assert_eq!(
            t.render_new("").unwrap(),
            "---\nid: A\ntitle: T\nstatus: todo\n---\n"
        );
    }

    #[test]
    fn marker_detection() {
        assert!(has_ticket_marker("---\nid: A\nstatus: todo\n---\n"));
        assert!(!has_ticket_marker("---\ntitle: Notes\n---\n"));
        assert!(!has_ticket_marker("# Just markdown\n"));
        assert!(!has_ticket_marker("---\nid: A\n---\n"));
    }
}
