//! The Jira Cloud connector, over the REST API v3.
//!
//! Credentials come from the environment: `JIRA_SITE_URL` (for example
//! `https://example.atlassian.net`), `JIRA_EMAIL` and `JIRA_API_TOKEN`. Options (set under
//! `options:` of the sync target) are listed in `docs/sync.md`.
//!
//! Behaviour against a live Jira is unverified: the code is tested against an in-process
//! mock of the endpoints it uses.

pub mod adf;
pub mod http;

use http::{Client, Retry};
use serde_json::{json, Map, Value};
use simpletickets_connector_protocol::{unsupported, Connector, ErrorCode, ErrorInfo};
use std::collections::BTreeSet;

/// Every issue made for a ticket carries the label `simpletickets-<ticket id>`.
const IDENTITY_PREFIX: &str = "simpletickets-";
const SUMMARY_LIMIT: usize = 255;
/// Keys per `key in (...)` query.
const KEYS_PER_QUERY: usize = 40;
const PAGE_SIZE: usize = 100;

#[derive(Debug, Clone)]
struct Options {
    project: String,
    issue_type: String,
    subtask_type: String,
    close_status: String,
    link_type: String,
    plain_description: bool,
    identity_label: bool,
}

/// Where the connector reads its credentials from; the process environment by default.
type EnvLookup = Box<dyn Fn(&str) -> Option<String> + Send>;

#[derive(Default)]
pub struct JiraConnector {
    client: Option<Client>,
    options: Option<Options>,
    env: Option<EnvLookup>,
}

fn invalid(message: impl Into<String>) -> ErrorInfo {
    ErrorInfo::invalid(message)
}

fn text_option(options: &Value, name: &str, default: &str) -> Result<String, ErrorInfo> {
    match &options[name] {
        Value::Null => Ok(default.to_string()),
        Value::String(s) if !s.trim().is_empty() => Ok(s.trim().to_string()),
        _ => Err(invalid(format!(
            "option `{name}` must be a non-empty string"
        ))),
    }
}

fn number_option(options: &Value, name: &str, default: u64) -> Result<u64, ErrorInfo> {
    match &options[name] {
        Value::Null => Ok(default),
        v => v
            .as_u64()
            .ok_or_else(|| invalid(format!("option `retry.{name}` must be a number"))),
    }
}

/// The site URL must be https; plain http is accepted only for a loopback host, which is
/// how tests reach a local mock.
fn check_site_url(url: &str) -> Result<(), ErrorInfo> {
    let rest = if let Some(rest) = url.strip_prefix("https://") {
        rest
    } else if let Some(rest) = url.strip_prefix("http://") {
        let host = rest.split(['/', ':']).next().unwrap_or_default();
        if !matches!(host, "127.0.0.1" | "localhost" | "[::1]") {
            return Err(invalid(
                "JIRA_SITE_URL must use https (http is allowed only for localhost)",
            ));
        }
        rest
    } else {
        return Err(invalid("JIRA_SITE_URL must start with https://"));
    };
    if rest.trim_matches('/').is_empty() {
        return Err(invalid("JIRA_SITE_URL has no host"));
    }
    Ok(())
}

/// A Jira label: no spaces allowed, so anything outside letters, digits and `-_.:/` becomes `_`.
fn label(text: &str) -> String {
    text.chars()
        .map(|c| {
            if c.is_alphanumeric() || "-_.:/".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn jql_quote(text: &str) -> String {
    format!("\"{}\"", text.replace('\\', "\\\\").replace('"', "\\\""))
}

fn path_key(key: &str) -> String {
    key.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "-_.".contains(c) {
                c.to_string()
            } else {
                format!("%{:02X}", c as u32)
            }
        })
        .collect()
}

/// Cuts text to `limit` characters, ending with an ellipsis when it was longer.
fn clip(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut cut: String = text.chars().take(limit - 1).collect();
    cut.push('…');
    cut
}

impl JiraConnector {
    /// A connector that reads credentials through `lookup` instead of the environment.
    pub fn with_env(lookup: impl Fn(&str) -> Option<String> + Send + 'static) -> JiraConnector {
        JiraConnector {
            env: Some(Box::new(lookup)),
            ..JiraConnector::default()
        }
    }

    fn env_value(&self, name: &str) -> Result<String, ErrorInfo> {
        let value = match &self.env {
            Some(lookup) => lookup(name),
            None => std::env::var(name).ok(),
        };
        value.filter(|v| !v.trim().is_empty()).ok_or_else(|| {
            ErrorInfo::new(
                ErrorCode::Auth,
                format!("the environment variable {name} is not set"),
            )
        })
    }

    fn client(&self) -> Result<&Client, ErrorInfo> {
        self.client
            .as_ref()
            .ok_or_else(|| invalid("`configure` has not been called"))
    }

    fn options(&self) -> Result<&Options, ErrorInfo> {
        self.options
            .as_ref()
            .ok_or_else(|| invalid("`configure` has not been called"))
    }

    fn identity_label(&self, id: &str) -> String {
        format!("{IDENTITY_PREFIX}{}", label(id))
    }

    fn configure(&mut self, params: &Value) -> Result<Value, ErrorInfo> {
        let o = &params["options"];
        let project = match o["project"].as_str().map(str::trim) {
            Some(p) if !p.is_empty() => p.to_string(),
            _ => return Err(invalid("option `project` (the project key) is required")),
        };
        let retry_defaults = Retry::default();
        let retry = &o["retry"];
        let retry = Retry {
            attempts: number_option(retry, "attempts", u64::from(retry_defaults.attempts))?
                .clamp(1, 20) as u32,
            base_ms: number_option(retry, "base_ms", retry_defaults.base_ms)?,
            max_ms: number_option(retry, "max_ms", retry_defaults.max_ms)?,
        };
        let description = text_option(o, "description", "adf")?;
        if !matches!(description.as_str(), "adf" | "text") {
            return Err(invalid("option `description` must be `adf` or `text`"));
        }
        let options = Options {
            project,
            issue_type: text_option(o, "issue_type", "Task")?,
            subtask_type: text_option(o, "subtask_type", "Sub-task")?,
            close_status: text_option(o, "close_status", "Done")?,
            link_type: text_option(o, "link_type", "Blocks")?,
            plain_description: description == "text",
            identity_label: o["identity_label"].as_bool().unwrap_or(true),
        };
        let site = self.env_value("JIRA_SITE_URL")?;
        check_site_url(site.trim())?;
        let email = self.env_value("JIRA_EMAIL")?;
        let token = self.env_value("JIRA_API_TOKEN")?;
        self.client = Some(Client::new(site.trim(), email.trim(), &token, retry));
        self.options = Some(options);
        Ok(json!({"capabilities": ["close", "fetch", "check"]}))
    }

    fn check(&self) -> Result<Value, ErrorInfo> {
        let client = self.client()?;
        let options = self.options()?;
        let me = client.send("GET", "/rest/api/3/myself", None, true)?;
        let project = client.send(
            "GET",
            &format!("/rest/api/3/project/{}", path_key(&options.project)),
            None,
            true,
        )?;
        Ok(json!({"message": format!(
            "authenticated as {} on {}; project {} ({})",
            me.body["displayName"].as_str().unwrap_or("an unnamed user"),
            client.base(),
            options.project,
            project.body["name"].as_str().unwrap_or("?"),
        )}))
    }

    /// The issue fields a ticket maps to. `parent` is only sent when a parent is known.
    fn issue_fields(
        &self,
        ticket: &Value,
        fields: &Value,
        links: &Value,
        creating: bool,
    ) -> Result<Map<String, Value>, ErrorInfo> {
        let options = self.options()?;
        let id = ticket["id"].as_str().unwrap_or_default();
        let title = fields["title"]
            .as_str()
            .or_else(|| ticket["title"].as_str())
            .unwrap_or("(untitled)");
        let body = ticket["body"].as_str().unwrap_or_default();
        let mut out = Map::new();
        out.insert("summary".into(), json!(clip(title.trim(), SUMMARY_LIMIT)));
        out.insert(
            "description".into(),
            if options.plain_description {
                adf::text_to_adf(body)
            } else {
                adf::markdown_to_adf(body)
            },
        );
        let mut labels: Vec<String> = fields["tags"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(label)
            .collect();
        if options.identity_label {
            labels.push(self.identity_label(id));
        }
        labels.sort();
        labels.dedup();
        out.insert("labels".into(), json!(labels));
        if let Some(priority) = fields["priority"].as_str() {
            out.insert("priority".into(), json!({"name": priority}));
        }
        match &fields["assignee"] {
            Value::String(account) => {
                out.insert("assignee".into(), json!({"accountId": account}));
            }
            Value::Null if !creating && fields.get("assignee").is_some() => {
                out.insert("assignee".into(), Value::Null);
            }
            _ => {}
        }
        match &fields["due"] {
            Value::String(date) => {
                out.insert("duedate".into(), json!(date));
            }
            Value::Null if !creating && fields.get("due").is_some() => {
                out.insert("duedate".into(), Value::Null);
            }
            _ => {}
        }
        if let Some(parent) = links["parent_ref"].as_str() {
            out.insert("parent".into(), json!({"key": parent}));
        }
        Ok(out)
    }

    /// The issue already carrying this ticket's identity label, if any.
    fn find_by_label(&self, label: &str) -> Result<Option<String>, ErrorInfo> {
        let options = self.options()?;
        let jql = format!(
            "project = {} AND labels = {}",
            jql_quote(&options.project),
            jql_quote(label)
        );
        let reply = self.client()?.send(
            "POST",
            "/rest/api/3/search/jql",
            Some(&json!({"jql": jql, "fields": ["summary"], "maxResults": 2})),
            true,
        )?;
        Ok(reply.body["issues"][0]["key"].as_str().map(String::from))
    }

    fn create(
        &self,
        ticket: &Value,
        mut fields: Map<String, Value>,
        links: &Value,
    ) -> Result<String, ErrorInfo> {
        let options = self.options()?;
        fields.insert("project".into(), json!({"key": options.project}));
        let kind = if links["parent_ref"].is_string() {
            &options.subtask_type
        } else {
            &options.issue_type
        };
        fields.insert("issuetype".into(), json!({"name": kind}));
        let reply = self.client()?.send(
            "POST",
            "/rest/api/3/issue",
            Some(&json!({"fields": fields})),
            false,
        )?;
        reply.body["key"].as_str().map(String::from).ok_or_else(|| {
            ErrorInfo::new(
                ErrorCode::Internal,
                format!("Jira created {} without returning its key", ticket["id"]),
            )
        })
    }

    fn issue(&self, key: &str) -> Result<Value, ErrorInfo> {
        let path = format!(
            "/rest/api/3/issue/{}?fields=status,issuelinks",
            path_key(key)
        );
        Ok(self.client()?.send("GET", &path, None, true)?.body)
    }

    /// Moves the issue to the status named `want`. Returns the status it ends up in.
    fn transition(
        &self,
        key: &str,
        current: &str,
        want: &str,
    ) -> Result<Result<String, String>, ErrorInfo> {
        if current.eq_ignore_ascii_case(want) {
            return Ok(Ok(current.to_string()));
        }
        let client = self.client()?;
        let path = format!("/rest/api/3/issue/{}/transitions", path_key(key));
        let reply = client.send("GET", &path, None, true)?;
        let found = reply.body["transitions"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|t| {
                t["to"]["name"]
                    .as_str()
                    .is_some_and(|n| n.eq_ignore_ascii_case(want))
            })
            .or_else(|| {
                reply.body["transitions"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|t| {
                        t["name"]
                            .as_str()
                            .is_some_and(|n| n.eq_ignore_ascii_case(want))
                    })
            });
        let Some(transition) = found else {
            return Ok(Err(format!(
                "no transition from `{current}` to `{want}` is available on {key}"
            )));
        };
        let id = transition["id"].as_str().unwrap_or_default();
        client.send(
            "POST",
            &path,
            Some(&json!({"transition": {"id": id}})),
            true,
        )?;
        let landed = transition["to"]["name"].as_str().unwrap_or(want);
        Ok(Ok(landed.to_string()))
    }

    /// Makes the issue's `Blocks` links match `desired`, touching only links to issues in
    /// `previous` (the ones this connector made earlier). Link problems are warnings.
    fn sync_links(
        &self,
        key: &str,
        issue: &Value,
        links: &Value,
        warnings: &mut Vec<String>,
    ) -> Result<(), ErrorInfo> {
        let options = self.options()?;
        let client = self.client()?;
        let names = |name: &str| -> BTreeSet<String> {
            links[name]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|v| v.as_str().map(String::from))
                .collect()
        };
        let desired = names("blocked_by_refs");
        let previous = names("previous_blocked_by_refs");
        let mut existing: Vec<(String, String)> = Vec::new();
        for link in issue["fields"]["issuelinks"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if link["type"]["name"].as_str() == Some(options.link_type.as_str()) {
                if let (Some(other), Some(id)) =
                    (link["inwardIssue"]["key"].as_str(), link["id"].as_str())
                {
                    existing.push((other.to_string(), id.to_string()));
                }
            }
        }
        let have: BTreeSet<&str> = existing.iter().map(|(k, _)| k.as_str()).collect();
        for blocker in desired.iter().filter(|b| !have.contains(b.as_str())) {
            let body = json!({
                "type": {"name": options.link_type},
                "inwardIssue": {"key": key},
                "outwardIssue": {"key": blocker},
            });
            match client.send("POST", "/rest/api/3/issueLink", Some(&body), true) {
                Ok(_) => {}
                Err(e) if e.retryable || e.code == ErrorCode::Auth => return Err(e),
                Err(e) => warnings.push(format!(
                    "cannot link {key} as blocked by {blocker}: {}",
                    e.message
                )),
            }
        }
        for (other, id) in &existing {
            if previous.contains(other) && !desired.contains(other) {
                match client.send(
                    "DELETE",
                    &format!("/rest/api/3/issueLink/{}", path_key(id)),
                    None,
                    true,
                ) {
                    Ok(_) => {}
                    Err(e) if e.code == ErrorCode::NotFound => {}
                    Err(e) if e.retryable || e.code == ErrorCode::Auth => return Err(e),
                    Err(e) => warnings.push(format!(
                        "cannot remove the link from {key} to {other}: {}",
                        e.message
                    )),
                }
            }
        }
        Ok(())
    }

    fn push(&self, params: &Value) -> Result<Value, ErrorInfo> {
        let options = self.options()?;
        let client = self.client()?;
        let ticket = &params["ticket"];
        let id = ticket["id"]
            .as_str()
            .ok_or_else(|| invalid("`ticket.id` is required"))?;
        let fields = &params["fields"];
        let links = &params["links"];

        let mut key = params["ref"].as_str().map(String::from);
        if key.is_none() && options.identity_label {
            key = self.find_by_label(&self.identity_label(id))?;
        }
        let created = key.is_none();
        let key = match key {
            Some(existing) => {
                let body = json!({"fields": self.issue_fields(ticket, fields, links, false)?});
                client.send(
                    "PUT",
                    &format!("/rest/api/3/issue/{}", path_key(&existing)),
                    Some(&body),
                    true,
                )?;
                existing
            }
            None => self.create(
                ticket,
                self.issue_fields(ticket, fields, links, true)?,
                links,
            )?,
        };

        let mut warnings = Vec::new();
        let issue = self.issue(&key)?;
        let current = issue["fields"]["status"]["name"]
            .as_str()
            .unwrap_or_default()
            .to_string();
        let mut confirmed = fields.as_object().cloned().unwrap_or_default();
        match fields["status"].as_str() {
            Some(want) => match self.transition(&key, &current, want)? {
                Ok(landed) => {
                    confirmed.insert("status".into(), json!(landed));
                }
                Err(warning) => {
                    warnings.push(warning);
                    confirmed.insert("status".into(), json!(current));
                }
            },
            None => {
                confirmed.insert("status".into(), json!(current));
            }
        }
        self.sync_links(&key, &issue, links, &mut warnings)?;
        Ok(json!({
            "ref": key,
            "url": format!("{}/browse/{key}", client.base()),
            "outcome": if created { "created" } else { "updated" },
            "fields": confirmed,
            "warnings": warnings,
        }))
    }

    fn close(&self, params: &Value) -> Result<Value, ErrorInfo> {
        let options = self.options()?;
        let key = params["ref"]
            .as_str()
            .ok_or_else(|| invalid("`ref` is required"))?;
        let issue = self.issue(key)?;
        let current = issue["fields"]["status"]["name"]
            .as_str()
            .unwrap_or_default();
        match self.transition(key, current, &options.close_status)? {
            Ok(_) => Ok(json!({})),
            Err(message) => Err(ErrorInfo::new(ErrorCode::Rejected, message)),
        }
    }

    /// The pullable values of an issue returned by a search.
    fn item(&self, issue: &Value) -> Option<Value> {
        let key = issue["key"].as_str()?;
        let f = &issue["fields"];
        let tags: Vec<&str> = f["labels"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .filter(|l| !l.starts_with(IDENTITY_PREFIX))
            .collect();
        Some(json!({
            "ref": key,
            "version": f["updated"],
            "fields": {
                "status": f["status"]["name"],
                "priority": f["priority"]["name"],
                "assignee": f["assignee"]["accountId"],
                "title": f["summary"],
                "tags": tags,
                "due": f["duedate"],
            },
        }))
    }

    fn search(&self, jql: &str, token: Option<&str>) -> Result<Value, ErrorInfo> {
        let mut body = json!({
            "jql": jql,
            "fields": ["status", "priority", "assignee", "summary", "labels", "duedate", "updated"],
            "maxResults": PAGE_SIZE,
        });
        if let Some(token) = token {
            body["nextPageToken"] = json!(token);
        }
        Ok(self
            .client()?
            .send("POST", "/rest/api/3/search/jql", Some(&body), true)?
            .body)
    }

    /// Changes since the cursor. Without a cursor, the current values of the known items;
    /// with one, everything in the project updated since, filtered to the known items.
    fn fetch(&self, params: &Value) -> Result<Value, ErrorInfo> {
        let options = self.options()?;
        let refs: BTreeSet<String> = params["refs"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| r.as_str().map(String::from))
            .collect();
        let cursor: Value = params["cursor"]
            .as_str()
            .and_then(|c| serde_json::from_str(c).ok())
            .unwrap_or(Value::Null);
        let mut items = Vec::new();
        let mut newest = cursor["since"].as_str().unwrap_or_default().to_string();
        let mut note = |issue: &Value| {
            if let Some(updated) = issue["fields"]["updated"].as_str() {
                if updated > newest.as_str() {
                    newest = updated.to_string();
                }
            }
        };
        let mut next_token = None;
        match cursor["since"].as_str() {
            None => {
                let keys: Vec<&String> = refs.iter().collect();
                for chunk in keys.chunks(KEYS_PER_QUERY) {
                    let list: Vec<String> = chunk.iter().map(|k| jql_quote(k)).collect();
                    let reply = self.search(&format!("key in ({})", list.join(", ")), None)?;
                    for issue in reply["issues"].as_array().into_iter().flatten() {
                        note(issue);
                        items.extend(self.item(issue));
                    }
                }
            }
            Some(since) => {
                // JQL dates have minute precision and read in the caller's time zone, which is
                // also the zone `updated` is rendered in, so its digits can be reused as they are.
                let minute: String = since.chars().take(16).collect::<String>().replace('T', " ");
                let jql = format!(
                    "project = {} AND updated >= {} ORDER BY updated ASC",
                    jql_quote(&options.project),
                    jql_quote(&minute)
                );
                let reply = self.search(&jql, cursor["token"].as_str())?;
                for issue in reply["issues"].as_array().into_iter().flatten() {
                    note(issue);
                    if issue["key"].as_str().is_some_and(|k| refs.contains(k)) {
                        items.extend(self.item(issue));
                    }
                }
                next_token = reply["nextPageToken"].as_str().map(String::from);
            }
        }
        let mut next = json!({"since": newest});
        if let Some(token) = &next_token {
            next["token"] = json!(token);
        }
        let cursor_text = if newest.is_empty() {
            Value::Null
        } else {
            json!(next.to_string())
        };
        Ok(json!({"items": items, "cursor": cursor_text, "more": next_token.is_some()}))
    }
}

impl Connector for JiraConnector {
    fn describe(&self) -> Value {
        json!({
            "name": "jira",
            "version": env!("CARGO_PKG_VERSION"),
            "capabilities": ["close", "fetch", "check"],
        })
    }

    fn handle(&mut self, op: &str, params: &Value) -> Result<Value, ErrorInfo> {
        match op {
            "configure" => self.configure(params),
            "check" => self.check(),
            "push" => self.push(params),
            "close" => self.close(params),
            "fetch" => self.fetch(params),
            other => Err(unsupported(other)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn site_urls_must_be_https_except_for_localhost() {
        assert!(check_site_url("https://example.atlassian.net").is_ok());
        assert!(check_site_url("http://127.0.0.1:8080").is_ok());
        assert!(check_site_url("http://localhost:1").is_ok());
        assert!(check_site_url("http://example.atlassian.net").is_err());
        assert!(check_site_url("example.atlassian.net").is_err());
        assert!(check_site_url("https://").is_err());
    }

    #[test]
    fn labels_have_no_spaces_and_keys_are_escaped() {
        assert_eq!(label("needs review"), "needs_review");
        assert_eq!(label("a-b_c.d:e/f"), "a-b_c.d:e/f");
        assert_eq!(jql_quote(r#"a"b\c"#), r#""a\"b\\c""#);
        assert_eq!(path_key("ST-1"), "ST-1");
        assert_eq!(path_key("a/b c"), "a%2Fb%20c");
    }

    #[test]
    fn long_summaries_are_clipped() {
        assert_eq!(clip("short", 10), "short");
        let clipped = clip(&"x".repeat(400), SUMMARY_LIMIT);
        assert_eq!(clipped.chars().count(), SUMMARY_LIMIT);
        assert!(clipped.ends_with('…'));
    }
}
