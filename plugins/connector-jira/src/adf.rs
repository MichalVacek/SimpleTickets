//! Rendering a ticket body for Jira's description field, which takes Atlassian Document
//! Format (ADF). Markdown is converted for a deliberately small subset; everything else is
//! kept as plain text so no content is lost. The limits are listed in `docs/sync.md`.

use serde_json::{json, Value};

/// Jira refuses descriptions beyond 32 767 characters; longer bodies are cut here.
pub const MAX_CHARS: usize = 30_000;
const TRUNCATION_NOTE: &str = "[truncated by SimpleTickets]";

fn paragraph(content: Vec<Value>) -> Value {
    if content.is_empty() {
        json!({"type": "paragraph"})
    } else {
        json!({"type": "paragraph", "content": content})
    }
}

fn text_node(text: &str, marks: &[Value]) -> Value {
    if marks.is_empty() {
        json!({"type": "text", "text": text})
    } else {
        json!({"type": "text", "text": text, "marks": marks})
    }
}

fn document(content: Vec<Value>) -> Value {
    json!({"type": "doc", "version": 1, "content": content})
}

fn truncate(body: &str) -> String {
    if body.chars().count() <= MAX_CHARS {
        return body.to_string();
    }
    let cut: String = body.chars().take(MAX_CHARS).collect();
    format!("{cut}\n\n{TRUNCATION_NOTE}")
}

/// The body as plain paragraphs: blank lines separate paragraphs, single line breaks stay
/// as line breaks, and no Markdown is interpreted.
pub fn text_to_adf(body: &str) -> Value {
    let body = truncate(body);
    let mut blocks = Vec::new();
    for chunk in body.replace("\r\n", "\n").split("\n\n") {
        let lines: Vec<&str> = chunk.trim_matches('\n').split('\n').collect();
        if lines.iter().all(|l| l.trim().is_empty()) {
            continue;
        }
        let mut content = Vec::new();
        for (n, line) in lines.iter().enumerate() {
            if n > 0 {
                content.push(json!({"type": "hardBreak"}));
            }
            if !line.is_empty() {
                content.push(text_node(line, &[]));
            }
        }
        blocks.push(paragraph(content));
    }
    document(blocks)
}

fn is_safe_link(url: &str) -> bool {
    ["http://", "https://", "mailto:"]
        .iter()
        .any(|scheme| url.to_ascii_lowercase().starts_with(scheme))
}

/// Finds `needle` in `chars` from `from`, returning its index.
fn find(chars: &[char], from: usize, needle: &[char]) -> Option<usize> {
    if needle.is_empty() || chars.len() < needle.len() {
        return None;
    }
    (from..=chars.len() - needle.len()).find(|&i| chars[i..i + needle.len()] == *needle)
}

fn is_word(c: Option<&char>) -> bool {
    c.is_some_and(|c| c.is_alphanumeric())
}

/// Inline Markdown: `code`, `**bold**`, `*italic*`, `_italic_`, `[text](url)`, and
/// backslash escapes. Anything unmatched stays literal.
pub fn inline(text: &str) -> Vec<Value> {
    inline_marked(&text.chars().collect::<Vec<_>>(), &[])
}

fn inline_marked(chars: &[char], marks: &[Value]) -> Vec<Value> {
    let mut out = Vec::new();
    let mut plain = String::new();
    let flush = |plain: &mut String, out: &mut Vec<Value>| {
        if !plain.is_empty() {
            out.push(text_node(plain, marks));
            plain.clear();
        }
    };
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let with_mark = |mark: Value, inner: &[char], out: &mut Vec<Value>| {
            let mut nested = marks.to_vec();
            nested.push(mark);
            out.extend(inline_marked(inner, &nested));
        };
        if c == '\\' && i + 1 < chars.len() && "\\`*_[]()#".contains(chars[i + 1]) {
            plain.push(chars[i + 1]);
            i += 2;
        } else if c == '`' {
            match find(chars, i + 1, &['`']).filter(|&end| end > i + 1) {
                Some(end) => {
                    flush(&mut plain, &mut out);
                    let code: String = chars[i + 1..end].iter().collect();
                    out.push(json!({"type": "text", "text": code, "marks": [{"type": "code"}]}));
                    i = end + 1;
                }
                None => {
                    plain.push(c);
                    i += 1;
                }
            }
        } else if (c == '*' || c == '_') && chars.get(i + 1) == Some(&c) {
            let delimiter = [c, c];
            let opens = c == '*' || !is_word(i.checked_sub(1).and_then(|p| chars.get(p)));
            // In a closing run such as `***`, the last two characters close the bold span.
            let close = find(chars, i + 2, &delimiter).map(|mut end| {
                while chars.get(end + 2) == Some(&c) {
                    end += 1;
                }
                end
            });
            match close.filter(|&end| opens && end > i + 2) {
                Some(end) if c == '*' || !is_word(chars.get(end + 2)) => {
                    flush(&mut plain, &mut out);
                    with_mark(json!({"type": "strong"}), &chars[i + 2..end], &mut out);
                    i = end + 2;
                }
                _ => {
                    plain.push(c);
                    plain.push(c);
                    i += 2;
                }
            }
        } else if c == '*' || c == '_' {
            let opens = c == '*' || !is_word(i.checked_sub(1).and_then(|p| chars.get(p)));
            let spaced = !chars.get(i + 1).is_some_and(|n| !n.is_whitespace());
            match find(chars, i + 1, &[c]).filter(|&end| opens && !spaced && end > i + 1) {
                Some(end) if c == '*' || !is_word(chars.get(end + 1)) => {
                    flush(&mut plain, &mut out);
                    with_mark(json!({"type": "em"}), &chars[i + 1..end], &mut out);
                    i = end + 1;
                }
                _ => {
                    plain.push(c);
                    i += 1;
                }
            }
        } else if c == '[' {
            let link = find(chars, i + 1, &[']'])
                .filter(|&close| chars.get(close + 1) == Some(&'('))
                .and_then(|close| find(chars, close + 2, &[')']).map(|end| (close, end)))
                .filter(|&(close, _)| close > i + 1);
            match link {
                Some((close, end)) => {
                    let url: String = chars[close + 2..end].iter().collect();
                    if is_safe_link(url.trim()) {
                        flush(&mut plain, &mut out);
                        let mark = json!({"type": "link", "attrs": {"href": url.trim()}});
                        with_mark(mark, &chars[i + 1..close], &mut out);
                        i = end + 1;
                    } else {
                        plain.push(c);
                        i += 1;
                    }
                }
                None => {
                    plain.push(c);
                    i += 1;
                }
            }
        } else {
            plain.push(c);
            i += 1;
        }
    }
    flush(&mut plain, &mut out);
    out
}

fn bullet(line: &str) -> Option<&str> {
    let t = line.trim_start();
    ["- ", "* ", "+ "].iter().find_map(|p| t.strip_prefix(p))
}

fn ordered(line: &str) -> Option<&str> {
    let t = line.trim_start();
    let digits = t.chars().take_while(char::is_ascii_digit).count();
    if digits == 0 || digits > 9 {
        return None;
    }
    let rest = &t[digits..];
    rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") "))
}

fn heading(line: &str) -> Option<(usize, &str)> {
    let level = line.chars().take_while(|&c| c == '#').count();
    let rest = line[level..].strip_prefix(' ')?;
    (1..=6).contains(&level).then_some((level, rest.trim()))
}

fn is_rule(line: &str) -> bool {
    let t = line.trim();
    t.len() >= 3
        && ["-", "*", "_"]
            .iter()
            .any(|m| t.chars().all(|c| m.starts_with(c)))
}

fn starts_block(line: &str) -> bool {
    line.starts_with("```")
        || heading(line).is_some()
        || is_rule(line)
        || bullet(line).is_some()
        || ordered(line).is_some()
        || line.starts_with('>')
}

/// Markdown to ADF: headings, paragraphs, bullet and numbered lists (one level), block
/// quotes, fenced code, rules, and the inline forms of [`inline`].
pub fn markdown_to_adf(body: &str) -> Value {
    let body = truncate(body).replace("\r\n", "\n");
    let lines: Vec<&str> = body.split('\n').collect();
    let mut blocks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let line = lines[i];
        if line.trim().is_empty() {
            i += 1;
        } else if let Some(fence) = line.strip_prefix("```") {
            let language = fence.trim();
            let mut code = Vec::new();
            i += 1;
            while i < lines.len() && !lines[i].starts_with("```") {
                code.push(lines[i]);
                i += 1;
            }
            i += 1;
            let mut node = json!({"type": "codeBlock"});
            if !language.is_empty() {
                node["attrs"] = json!({"language": language});
            }
            let text = code.join("\n");
            if !text.is_empty() {
                node["content"] = json!([{"type": "text", "text": text}]);
            }
            blocks.push(node);
        } else if let Some((level, text)) = heading(line) {
            blocks.push(
                json!({"type": "heading", "attrs": {"level": level}, "content": inline(text)}),
            );
            i += 1;
        } else if is_rule(line) {
            blocks.push(json!({"type": "rule"}));
            i += 1;
        } else if bullet(line).is_some() || ordered(line).is_some() {
            let is_bullet = bullet(line).is_some();
            let mut items = Vec::new();
            while i < lines.len() {
                let item = if is_bullet {
                    bullet(lines[i])
                } else {
                    ordered(lines[i])
                };
                let Some(text) = item else { break };
                items.push(json!({"type": "listItem", "content": [paragraph(inline(text))]}));
                i += 1;
            }
            let kind = if is_bullet {
                "bulletList"
            } else {
                "orderedList"
            };
            blocks.push(json!({"type": kind, "content": items}));
        } else if line.starts_with('>') {
            let mut quoted = Vec::new();
            while i < lines.len() && lines[i].starts_with('>') {
                quoted.push(lines[i][1..].trim());
                i += 1;
            }
            let text = quoted.join(" ");
            blocks.push(json!({"type": "blockquote", "content": [paragraph(inline(text.trim()))]}));
        } else {
            let mut para = Vec::new();
            while i < lines.len()
                && !lines[i].trim().is_empty()
                && (para.is_empty() || !starts_block(lines[i]))
            {
                para.push(lines[i].trim());
                i += 1;
            }
            blocks.push(paragraph(inline(&para.join(" "))));
        }
    }
    document(blocks)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(doc: &Value) -> Vec<&str> {
        doc["content"]
            .as_array()
            .unwrap()
            .iter()
            .map(|b| b["type"].as_str().unwrap())
            .collect()
    }

    #[test]
    fn converts_the_supported_blocks() {
        let md = "# Title\n\nSome *text* here\nsecond line\n\n- one\n- two\n\n1. first\n2) second\n\n> quoted\n> more\n\n---\n\n```rust\nlet x = 1;\n```\n";
        let doc = markdown_to_adf(md);
        assert_eq!(doc["type"], "doc");
        assert_eq!(doc["version"], 1);
        assert_eq!(
            kinds(&doc),
            [
                "heading",
                "paragraph",
                "bulletList",
                "orderedList",
                "blockquote",
                "rule",
                "codeBlock"
            ]
        );
        assert_eq!(doc["content"][0]["attrs"]["level"], 1);
        // Soft line breaks join into one paragraph.
        let para = &doc["content"][1]["content"];
        assert_eq!(para[2]["text"], " here second line");
        assert_eq!(doc["content"][2]["content"].as_array().unwrap().len(), 2);
        assert_eq!(doc["content"][6]["attrs"]["language"], "rust");
        assert_eq!(doc["content"][6]["content"][0]["text"], "let x = 1;");
    }

    #[test]
    fn converts_inline_marks_and_links() {
        let nodes = inline("a **bold *and em*** `code` [site](https://x.test) b");
        let texts: Vec<&str> = nodes.iter().map(|n| n["text"].as_str().unwrap()).collect();
        assert_eq!(
            texts,
            ["a ", "bold ", "and em", " ", "code", " ", "site", " b"]
        );
        assert_eq!(nodes[1]["marks"][0]["type"], "strong");
        assert_eq!(nodes[2]["marks"][1]["type"], "em");
        assert_eq!(nodes[4]["marks"][0]["type"], "code");
        assert_eq!(nodes[6]["marks"][0]["attrs"]["href"], "https://x.test");
    }

    #[test]
    fn unsafe_links_unmatched_marks_and_snake_case_stay_literal() {
        let joined = |s: &str| {
            inline(s)
                .iter()
                .map(|n| n["text"].as_str().unwrap().to_string())
                .collect::<String>()
        };
        assert_eq!(
            joined("[x](javascript:alert(1))"),
            "[x](javascript:alert(1))"
        );
        assert_eq!(joined("a * b and **c"), "a * b and **c");
        assert_eq!(joined("snake_case_name stays"), "snake_case_name stays");
        assert_eq!(joined(r"escaped \*stars\*"), "escaped *stars*");
        assert!(inline("a snake_case word")
            .iter()
            .all(|n| n.get("marks").is_none()));
    }

    #[test]
    fn unsupported_markdown_is_kept_as_text() {
        let doc = markdown_to_adf("| a | b |\n|---|---|\n| 1 | 2 |\n");
        assert_eq!(kinds(&doc), ["paragraph"]);
        let text = doc["content"][0]["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("| 1 | 2 |"));
    }

    #[test]
    fn plain_text_mode_does_not_interpret_markdown() {
        let doc = text_to_adf("# not a heading\nline two\n\nsecond *para*");
        assert_eq!(kinds(&doc), ["paragraph", "paragraph"]);
        assert_eq!(doc["content"][0]["content"][1]["type"], "hardBreak");
        assert_eq!(doc["content"][1]["content"][0]["text"], "second *para*");
    }

    #[test]
    fn empty_bodies_give_an_empty_document_and_long_ones_are_cut() {
        assert_eq!(markdown_to_adf("")["content"], json!([]));
        assert_eq!(text_to_adf("\n\n")["content"], json!([]));
        let long = "x".repeat(MAX_CHARS + 50);
        let doc = markdown_to_adf(&long);
        let text = serde_json::to_string(&doc).unwrap();
        assert!(text.contains(TRUNCATION_NOTE));
        assert!(text.len() < MAX_CHARS + 500);
    }
}
