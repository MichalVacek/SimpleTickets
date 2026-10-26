//! Filtering and ordering of tickets.

use crate::config::Category;
use crate::id::sequence_number;
use crate::search::{Hit, Query};
use crate::store::{IndexedTicket, Store};
use crate::time::{is_date, today};
use serde_yaml_ng::Value as Yaml;

/// Exact-match filters; every given condition must hold.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filter {
    /// Status name, case-insensitive.
    pub status: Option<String>,
    /// Category of the ticket's own status.
    pub category: Option<Category>,
    pub priority: Option<String>,
    /// Assignee, case-insensitive.
    pub assignee: Option<String>,
    pub tag: Option<String>,
    pub parent: Option<String>,
    /// Case-insensitive substring of the title.
    pub title: Option<String>,
    pub blocked: Option<bool>,
    /// Tickets with a due date on or before this date.
    pub due_by: Option<String>,
    /// Exact match on a free-form frontmatter key.
    pub fields: Vec<(String, String)>,
}

impl Filter {
    /// Builds a filter from `key=value` pairs, as given by the CLI and the API.
    ///
    /// Keys: `status`, `category`, `priority`, `assignee`, `tag`, `parent`, `title`,
    /// `blocked`, `due_by` (a date or `today`) and `field.<key>`.
    pub fn from_pairs(pairs: &[(String, String)]) -> Result<Filter, String> {
        let mut f = Filter::default();
        for (key, value) in pairs {
            let v = value.clone();
            match key.as_str() {
                "status" => f.status = Some(v),
                "category" => {
                    f.category = Some(
                        Category::from_key(value)
                            .ok_or_else(|| format!("unknown category `{value}`"))?,
                    )
                }
                "priority" => f.priority = Some(v),
                "assignee" => f.assignee = Some(v),
                "tag" => f.tag = Some(v),
                "parent" => f.parent = Some(v),
                "title" => f.title = Some(v),
                "blocked" => {
                    f.blocked = Some(match value.as_str() {
                        "true" | "yes" | "1" => true,
                        "false" | "no" | "0" => false,
                        _ => return Err(format!("`blocked` must be true or false, got `{value}`")),
                    })
                }
                "due_by" => {
                    let date = if value == "today" { today() } else { v };
                    if !is_date(&date) {
                        return Err(format!(
                            "`due_by` must be YYYY-MM-DD or today, got `{value}`"
                        ));
                    }
                    f.due_by = Some(date);
                }
                other => match other.strip_prefix("field.") {
                    Some(name) if !name.is_empty() => f.fields.push((name.to_string(), v)),
                    _ => return Err(format!("unknown filter `{other}`")),
                },
            }
        }
        Ok(f)
    }

    pub fn matches(&self, store: &Store, entry: &IndexedTicket) -> bool {
        let t = &entry.ticket;
        if let Some(s) = &self.status {
            if !t.status.eq_ignore_ascii_case(s) {
                return false;
            }
        }
        if let Some(c) = self.category {
            if store.own_category(&t.id) != Some(c) {
                return false;
            }
        }
        if let Some(p) = &self.priority {
            if t.priority.as_deref().map(str::to_lowercase) != Some(p.to_lowercase()) {
                return false;
            }
        }
        if let Some(a) = &self.assignee {
            if t.assignee.as_deref().map(str::to_lowercase) != Some(a.to_lowercase()) {
                return false;
            }
        }
        if let Some(tag) = &self.tag {
            if !t.tags.iter().any(|x| x.eq_ignore_ascii_case(tag)) {
                return false;
            }
        }
        if let Some(p) = &self.parent {
            if t.parent.as_deref() != Some(p.as_str()) {
                return false;
            }
        }
        if let Some(q) = &self.title {
            if !t.title.to_lowercase().contains(&q.to_lowercase()) {
                return false;
            }
        }
        if let Some(b) = self.blocked {
            if store.is_blocked(&t.id) != b {
                return false;
            }
        }
        if let Some(by) = &self.due_by {
            match &t.due {
                Some(due) if is_date(due) && due.as_str() <= by.as_str() => {}
                _ => return false,
            }
        }
        self.fields
            .iter()
            .all(|(key, want)| match t.extra.get(key.as_str()) {
                None if key == "assignee" => t.assignee.as_deref() == Some(want.as_str()),
                Some(Yaml::String(s)) => s == want,
                Some(Yaml::Number(n)) => n.to_string() == *want,
                Some(Yaml::Bool(b)) => b.to_string() == *want,
                _ => false,
            })
    }
}

/// The matching tickets in natural ID order (`ST-2` before `ST-10`).
pub fn list<'a>(store: &'a Store, filter: &Filter) -> Vec<&'a IndexedTicket> {
    let prefix = store.config().id_prefix.as_str();
    let mut out: Vec<&IndexedTicket> = store
        .tickets()
        .filter(|e| filter.matches(store, e))
        .collect();
    out.sort_by_key(|e| {
        (
            sequence_number(prefix, &e.ticket.id).unwrap_or(u64::MAX),
            e.ticket.id.clone(),
        )
    });
    out
}

fn natural_key(prefix: &str, id: &str) -> (u64, String) {
    (
        sequence_number(prefix, id).unwrap_or(u64::MAX),
        id.to_string(),
    )
}

/// Tickets matching the text `query` and `filter`, best match first; equal
/// scores come in natural ID order.
pub fn search<'a>(
    store: &'a Store,
    query: &Query,
    filter: &Filter,
) -> Vec<(&'a IndexedTicket, Hit)> {
    let prefix = store.config().id_prefix.as_str();
    let mut out: Vec<(&IndexedTicket, Hit)> = store
        .search(query)
        .into_iter()
        .filter_map(|hit| {
            let entry = store.get(&hit.id)?;
            filter.matches(store, entry).then_some((entry, hit))
        })
        .collect();
    out.sort_by(|a, b| {
        b.1.score
            .cmp(&a.1.score)
            .then_with(|| natural_key(prefix, &a.1.id).cmp(&natural_key(prefix, &b.1.id)))
    });
    out
}

/// Tickets that act as parents: they have sub-tickets or sit in the parent folder.
pub fn parents(store: &Store) -> Vec<&IndexedTicket> {
    let folder = store.config().parent_folder.clone();
    list(store, &Filter::default())
        .into_iter()
        .filter(|e| {
            !store.children_of(&e.ticket.id).is_empty()
                || folder
                    .as_ref()
                    .is_some_and(|f| crate::state::is_within(&e.path, f))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::store::testutil::{ticket, write};

    fn store() -> (tempfile::TempDir, Store) {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(
            r,
            "a.md",
            &ticket(
                "ST-10",
                "todo",
                "priority: high\nassignee: Ann\ndue: 2020-01-01\ntags: [Work]\nteam: core\n",
            ),
        );
        write(r, "b.md", &ticket("ST-2", "done", "priority: low\n"));
        write(
            r,
            "c.md",
            &ticket("ST-3", "todo", "blocked_by: [ST-10]\nparent: ST-2\n"),
        );
        let s = Store::scan(&Config::default_for(r));
        (dir, s)
    }

    fn q(pairs: &[(&str, &str)]) -> Vec<String> {
        let (_d, s) = store();
        let pairs: Vec<_> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let f = Filter::from_pairs(&pairs).unwrap();
        list(&s, &f).iter().map(|e| e.ticket.id.clone()).collect()
    }

    #[test]
    fn natural_order_without_filter() {
        assert_eq!(q(&[]), ["ST-2", "ST-3", "ST-10"]);
    }

    #[test]
    fn filters_by_fields() {
        assert_eq!(q(&[("status", "TODO")]), ["ST-3", "ST-10"]);
        assert_eq!(q(&[("category", "done")]), ["ST-2"]);
        assert_eq!(q(&[("priority", "High")]), ["ST-10"]);
        assert_eq!(q(&[("assignee", "ANN")]), ["ST-10"]);
        assert_eq!(q(&[("assignee", "bob")]), Vec::<String>::new());
        assert_eq!(q(&[("tag", "work")]), ["ST-10"]);
        assert_eq!(q(&[("parent", "ST-2")]), ["ST-3"]);
        assert_eq!(q(&[("title", "ticket st-1")]), ["ST-10"]);
        assert_eq!(q(&[("blocked", "true")]), ["ST-3"]);
        assert_eq!(q(&[("blocked", "false")]), ["ST-2", "ST-10"]);
        assert_eq!(q(&[("due_by", "today")]), ["ST-10"]);
        assert_eq!(q(&[("due_by", "2019-12-31")]), Vec::<String>::new());
        assert_eq!(q(&[("field.team", "core")]), ["ST-10"]);
        assert_eq!(q(&[("field.assignee", "Ann")]), ["ST-10"]);
        assert_eq!(q(&[("status", "todo"), ("tag", "work")]), ["ST-10"]);
    }

    #[test]
    fn search_ranks_filters_and_orders_ties_naturally() {
        let dir = tempfile::tempdir().unwrap();
        let r = dir.path();
        write(r, "a.md", &ticket("ST-10", "todo", ""));
        write(r, "b.md", &ticket("ST-2", "todo", ""));
        write(r, "c.md", &ticket("ST-3", "done", ""));
        write(
            r,
            "d.md",
            "---\nid: ST-4\ntitle: Body\nstatus: todo\n---\nTicket ticket\n",
        );
        let s = Store::scan(&Config::default_for(r));
        let ids = |q: &str, pairs: &[(&str, &str)]| -> Vec<String> {
            let pairs: Vec<_> = pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
            let f = Filter::from_pairs(&pairs).unwrap();
            search(&s, &Query::parse(q).unwrap(), &f)
                .iter()
                .map(|(e, _)| e.ticket.id.clone())
                .collect()
        };
        // Titles read `Ticket ST-n`: one title hit each; ST-4 has two body hits.
        assert_eq!(ids("ticket", &[]), ["ST-2", "ST-3", "ST-10", "ST-4"]);
        assert_eq!(
            ids("ticket", &[("status", "todo")]),
            ["ST-2", "ST-10", "ST-4"]
        );
        assert_eq!(ids("\"ticket st-3\"", &[]), ["ST-3"]);
    }

    #[test]
    fn rejects_bad_filters() {
        let bad = |k: &str, v: &str| Filter::from_pairs(&[(k.into(), v.into())]).is_err();
        assert!(bad("category", "x"));
        assert!(bad("blocked", "maybe"));
        assert!(bad("due_by", "soon"));
        assert!(bad("bogus", "1"));
        assert!(bad("field.", "1"));
    }

    #[test]
    fn parents_are_tickets_with_children() {
        let (_d, s) = store();
        let ids: Vec<_> = parents(&s).iter().map(|e| e.ticket.id.clone()).collect();
        assert_eq!(ids, ["ST-2"]);
    }
}
