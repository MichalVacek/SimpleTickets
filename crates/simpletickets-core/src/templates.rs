//! Body templates for new tickets.
//!
//! Built-in templates can be overridden or extended by `*.md` files in the
//! templates folder of the state directory. `{{title}}`, `{{id}}` and
//! `{{date}}` are replaced when a ticket is created.

use crate::state::templates_dir;
use crate::time::today;
use std::fs;
use std::path::Path;

pub const DEFAULT_TEMPLATE: &str = "default";

const BUILTIN: [(&str, &str); 2] = [
    ("default", "# {{title}}\n\n"),
    (
        "spec",
        "# {{title}}\n\n## Goal\n\n## Scope\n\n## Acceptance criteria\n\n",
    ),
];

/// Names of all available templates, sorted.
pub fn list(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = BUILTIN.iter().map(|(n, _)| (*n).to_string()).collect();
    if let Ok(entries) = fs::read_dir(templates_dir(dir)) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "md") {
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    names.push(stem.to_string());
                }
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

/// Renders the named template for a ticket.
pub fn render(dir: &Path, name: &str, title: &str, id: &str) -> Result<String, String> {
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return Err(format!("invalid template name `{name}`"));
    }
    let custom = templates_dir(dir).join(format!("{name}.md"));
    let text = match fs::read_to_string(&custom) {
        Ok(text) => text,
        Err(_) => BUILTIN
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, t)| (*t).to_string())
            .ok_or_else(|| format!("unknown template `{name}`"))?,
    };
    Ok(text
        .replace("{{title}}", title)
        .replace("{{id}}", id)
        .replace("{{date}}", &today()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn built_in_and_custom_templates() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(list(dir.path()), ["default", "spec"]);
        assert_eq!(
            render(dir.path(), "default", "Hello", "ST-1").unwrap(),
            "# Hello\n\n"
        );
        let tdir = templates_dir(dir.path());
        fs::create_dir_all(&tdir).unwrap();
        fs::write(tdir.join("bug.md"), "{{id}}: {{title}} ({{date}})\n").unwrap();
        assert_eq!(list(dir.path()), ["bug", "default", "spec"]);
        let out = render(dir.path(), "bug", "Crash", "ST-2").unwrap();
        assert!(out.starts_with("ST-2: Crash (20"), "{out}");
    }

    #[test]
    fn rejects_unknown_and_unsafe_names() {
        let dir = tempfile::tempdir().unwrap();
        assert!(render(dir.path(), "nope", "t", "i").is_err());
        assert!(render(dir.path(), "../x", "t", "i").is_err());
    }
}
