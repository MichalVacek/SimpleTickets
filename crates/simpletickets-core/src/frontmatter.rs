//! Splitting and patching of YAML frontmatter at the text level.
//!
//! The frontmatter is never re-serialized as a whole. Reads go through a YAML
//! parser, writes replace, insert or remove individual top-level keys in the
//! original text, so comments, key order, unknown keys and line endings of
//! everything else stay untouched.

use thiserror::Error;

const DELIMITER: &str = "---";

#[derive(Debug, Error, PartialEq, Eq)]
pub enum FrontmatterError {
    #[error("file does not start with a `---` frontmatter delimiter")]
    Missing,
    #[error("frontmatter is not closed by a `---` line")]
    Unclosed,
    #[error("value for `{0}` must be a single line")]
    MultilineValue(String),
}

/// A value written into a frontmatter key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Scalar(String),
    List(Vec<String>),
}

/// A Markdown file split into frontmatter and body, holding the original text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Document {
    text: String,
    fm_start: usize,
    fm_end: usize,
}

impl Document {
    pub fn parse(text: impl Into<String>) -> Result<Self, FrontmatterError> {
        let text = text.into();
        let (fm_start, fm_end) = locate(&text)?;
        Ok(Self {
            text,
            fm_start,
            fm_end,
        })
    }

    /// The full file text, including delimiters and body.
    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn into_text(self) -> String {
        self.text
    }

    /// The YAML between the delimiters.
    pub fn frontmatter(&self) -> &str {
        &self.text[self.fm_start..self.fm_end]
    }

    /// Everything after the closing delimiter line.
    pub fn body(&self) -> &str {
        let rest = &self.text[self.fm_end..];
        match rest.find('\n') {
            Some(i) => &rest[i + 1..],
            None => "",
        }
    }

    /// One-based file line on which the frontmatter YAML starts.
    pub fn first_frontmatter_line(&self) -> usize {
        2
    }

    /// Sets, replaces or (with `None`) removes a top-level key.
    pub fn set(&mut self, key: &str, value: Option<&Value>) -> Result<(), FrontmatterError> {
        let patched = patch(self.frontmatter(), key, value)?;
        self.text
            .replace_range(self.fm_start..self.fm_end, &patched);
        self.fm_end = self.fm_start + patched.len();
        Ok(())
    }
}

/// Returns the byte range of the frontmatter YAML within `text`.
fn locate(text: &str) -> Result<(usize, usize), FrontmatterError> {
    if text.starts_with('\u{feff}') {
        return Err(FrontmatterError::Missing);
    }
    let mut lines = text.split_inclusive('\n');
    let first = lines.next().ok_or(FrontmatterError::Missing)?;
    if first.trim_end_matches(['\r', '\n']) != DELIMITER {
        return Err(FrontmatterError::Missing);
    }
    let start = first.len();
    let mut offset = start;
    for line in lines {
        if line.trim_end_matches(['\r', '\n']) == DELIMITER {
            return Ok((start, offset));
        }
        offset += line.len();
    }
    Err(FrontmatterError::Unclosed)
}

/// The unquoted key of a top-level `key: value` line, if the line is one.
pub fn top_level_key(line: &str) -> Option<&str> {
    let first = line.chars().next()?;
    if first.is_whitespace() || matches!(first, '#' | '-' | '[' | '{' | '?' | '|' | '>') {
        return None;
    }
    let (key, rest) = if first == '"' || first == '\'' {
        let close = line[1..].find(first)? + 1;
        (&line[1..close], &line[close + 1..])
    } else {
        let colon = line.find(':')?;
        (line[..colon].trim_end(), &line[colon..])
    };
    let rest = rest.trim_start().strip_prefix(':')?;
    let ends_key = rest.is_empty() || rest.starts_with([' ', '\t', '\r', '\n']);
    ends_key.then_some(key)
}

/// One-based line number (relative to `fm`) of a top-level key.
pub fn key_line(fm: &str, key: &str) -> Option<usize> {
    fm.split_inclusive('\n')
        .position(|l| top_level_key(l) == Some(key))
        .map(|i| i + 1)
}

pub fn has_key(fm: &str, key: &str) -> bool {
    key_line(fm, key).is_some()
}

fn is_blank_or_comment(line: &str) -> bool {
    let t = line.trim();
    t.is_empty() || t.starts_with('#')
}

fn render(key: &str, value: &Value, eol: &str) -> Result<String, FrontmatterError> {
    let scalar = |s: &str| -> Result<String, FrontmatterError> {
        if s.contains(['\n', '\r']) {
            return Err(FrontmatterError::MultilineValue(key.to_string()));
        }
        let yaml = serde_yaml_ng::to_string(s).expect("strings serialize");
        Ok(yaml.trim_end().to_string())
    };
    Ok(match value {
        Value::Scalar(s) => format!("{key}: {}{eol}", scalar(s)?),
        Value::List(items) if items.is_empty() => format!("{key}: []{eol}"),
        Value::List(items) => {
            let mut out = format!("{key}:{eol}");
            for item in items {
                out.push_str(&format!("  - {}{eol}", scalar(item)?));
            }
            out
        }
    })
}

/// Returns `fm` with one top-level key set, replaced or removed.
pub fn patch(fm: &str, key: &str, value: Option<&Value>) -> Result<String, FrontmatterError> {
    let eol = if fm.contains("\r\n") { "\r\n" } else { "\n" };
    let lines: Vec<&str> = fm.split_inclusive('\n').collect();
    let start = lines.iter().position(|l| top_level_key(l) == Some(key));

    let Some(start) = start else {
        let Some(value) = value else {
            return Ok(fm.to_string());
        };
        let mut out = fm.to_string();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push_str(eol);
        }
        out.push_str(&render(key, value, eol)?);
        return Ok(out);
    };

    let mut end = lines[start + 1..]
        .iter()
        .position(|l| top_level_key(l).is_some())
        .map_or(lines.len(), |i| start + 1 + i);
    while end > start + 1 && is_blank_or_comment(lines[end - 1]) {
        end -= 1;
    }

    let mut out: String = lines[..start].concat();
    if let Some(value) = value {
        out.push_str(&render(key, value, eol)?);
    }
    out.push_str(&lines[end..].concat());
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(s: &str) -> Document {
        Document::parse(s).unwrap()
    }

    #[test]
    fn splits_frontmatter_and_body() {
        let d = doc("---\nid: A-1\n---\n# Title\n\ntext\n");
        assert_eq!(d.frontmatter(), "id: A-1\n");
        assert_eq!(d.body(), "# Title\n\ntext\n");
    }

    #[test]
    fn empty_body_and_empty_frontmatter() {
        let d = doc("---\n---\n");
        assert_eq!(d.frontmatter(), "");
        assert_eq!(d.body(), "");
        assert_eq!(doc("---\na: 1\n---").body(), "");
    }

    #[test]
    fn rejects_missing_or_unclosed() {
        assert_eq!(Document::parse("# hi\n"), Err(FrontmatterError::Missing));
        assert_eq!(Document::parse(""), Err(FrontmatterError::Missing));
        assert_eq!(
            Document::parse("---\na: 1\n"),
            Err(FrontmatterError::Unclosed)
        );
    }

    #[test]
    fn body_may_contain_delimiter_lines() {
        let d = doc("---\na: 1\n---\nx\n---\ny\n");
        assert_eq!(d.body(), "x\n---\ny\n");
    }

    #[test]
    fn detects_top_level_keys() {
        assert_eq!(top_level_key("id: A-1\n"), Some("id"));
        assert_eq!(top_level_key("tags:\n"), Some("tags"));
        assert_eq!(top_level_key("\"quoted key\": 1"), Some("quoted key"));
        assert_eq!(top_level_key("  nested: 1"), None);
        assert_eq!(top_level_key("- item"), None);
        assert_eq!(top_level_key("# comment: x"), None);
        assert_eq!(top_level_key("url: http://x"), Some("url"));
        assert_eq!(top_level_key("not a key"), None);
        assert_eq!(top_level_key("a:b"), None);
    }

    #[test]
    fn replaces_only_the_target_key() {
        let src = "---\n# keep\nid: A-1\nstatus: todo\nx-custom: {a: 1}\n---\nbody\n";
        let mut d = doc(src);
        d.set("status", Some(&Value::Scalar("done".into())))
            .unwrap();
        assert_eq!(
            d.text(),
            "---\n# keep\nid: A-1\nstatus: done\nx-custom: {a: 1}\n---\nbody\n"
        );
    }

    #[test]
    fn replaces_multiline_block_and_keeps_trailing_comment() {
        let src = "---\ntags:\n  - a\n  - b\n# note\nstatus: todo\n---\n";
        let mut d = doc(src);
        d.set("tags", Some(&Value::List(vec!["c".into()]))).unwrap();
        assert_eq!(d.text(), "---\ntags:\n  - c\n# note\nstatus: todo\n---\n");
    }

    #[test]
    fn handles_zero_indented_sequences() {
        let src = "---\ntags:\n- a\n- b\nstatus: todo\n---\n";
        let mut d = doc(src);
        d.set("tags", None).unwrap();
        assert_eq!(d.text(), "---\nstatus: todo\n---\n");
    }

    #[test]
    fn appends_missing_key_and_ignores_missing_removal() {
        let mut d = doc("---\nid: A-1\n---\nbody");
        d.set("due", Some(&Value::Scalar("2026-01-02".into())))
            .unwrap();
        assert_eq!(d.text(), "---\nid: A-1\ndue: 2026-01-02\n---\nbody");
        d.set("nothing", None).unwrap();
        assert_eq!(d.body(), "body");
    }

    #[test]
    fn preserves_crlf() {
        let mut d = doc("---\r\nid: A-1\r\nstatus: todo\r\n---\r\nbody\r\n");
        d.set("status", Some(&Value::Scalar("done".into())))
            .unwrap();
        d.set("tags", Some(&Value::List(vec!["x".into()]))).unwrap();
        assert_eq!(
            d.text(),
            "---\r\nid: A-1\r\nstatus: done\r\ntags:\r\n  - x\r\n---\r\nbody\r\n"
        );
    }

    #[test]
    fn quotes_values_that_need_it() {
        let mut d = doc("---\n---\n");
        d.set("title", Some(&Value::Scalar("a: b #c".into())))
            .unwrap();
        d.set("priority", Some(&Value::Scalar("1".into()))).unwrap();
        let parsed: serde_yaml_ng::Value = serde_yaml_ng::from_str(d.frontmatter()).unwrap();
        assert_eq!(parsed["title"].as_str(), Some("a: b #c"));
        assert_eq!(parsed["priority"].as_str(), Some("1"));
    }

    #[test]
    fn rejects_multiline_values() {
        let mut d = doc("---\n---\n");
        assert_eq!(
            d.set("title", Some(&Value::Scalar("a\nb".into()))),
            Err(FrontmatterError::MultilineValue("title".into()))
        );
    }
}
