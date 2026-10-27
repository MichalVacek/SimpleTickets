#![allow(dead_code)]
//! An in-process stand-in for the Jira Cloud endpoints the connector uses. It mimics their
//! shapes from the public documentation; it is not Jira, and agreeing with it proves
//! nothing about a live site.

use serde_json::{json, Map, Value};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tiny_http::{Header, Method, Response, Server};

pub const EMAIL: &str = "bot@example.test";
pub const TOKEN: &str = "s3cret-token-value";

#[derive(Clone, Debug)]
pub struct Issue {
    pub key: String,
    pub fields: Map<String, Value>,
    pub status: String,
    pub updated: u32,
}

#[derive(Clone, Debug)]
pub struct Link {
    pub id: String,
    pub kind: String,
    /// The issue on the inward side ("is blocked by").
    pub inward: String,
    /// The issue on the outward side ("blocks").
    pub outward: String,
}

#[derive(Clone, Debug)]
pub struct Recorded {
    pub method: String,
    pub path: String,
    pub body: Value,
}

#[derive(Default)]
pub struct State {
    pub issues: Vec<Issue>,
    pub links: Vec<Link>,
    pub requests: Vec<Recorded>,
    /// Answers to give before behaving: `(status, retry-after)`, used while the request
    /// path contains `fail_on`.
    pub failures: VecDeque<(u16, Option<u64>)>,
    pub fail_on: String,
    /// Statuses an issue can be moved to; the default workflow allows any of them.
    pub statuses: Vec<String>,
    pub page_size: usize,
    pub clock: u32,
    next_id: u32,
}

pub struct Mock {
    pub url: String,
    pub state: Arc<Mutex<State>>,
    server: Arc<Server>,
}

impl Drop for Mock {
    fn drop(&mut self) {
        self.server.unblock();
    }
}

fn updated_stamp(n: u32) -> String {
    format!("2026-10-27T10:{:02}:{:02}.000+0000", n / 60 % 60, n % 60)
}

impl State {
    pub fn issue(&self, key: &str) -> &Issue {
        self.issues
            .iter()
            .find(|i| i.key == key)
            .expect("issue exists")
    }

    pub fn count(&self, method: &str, path_part: &str) -> usize {
        self.requests
            .iter()
            .filter(|r| r.method == method && r.path.contains(path_part))
            .count()
    }

    /// Changes an issue's status the way a person would in Jira.
    pub fn set_status(&mut self, key: &str, status: &str) {
        self.clock += 1;
        let clock = self.clock;
        let issue = self.issues.iter_mut().find(|i| i.key == key).unwrap();
        issue.status = status.into();
        issue.updated = clock;
    }

    fn render(&self, issue: &Issue, with_links: bool) -> Value {
        let mut fields = issue.fields.clone();
        fields.insert("status".into(), json!({"name": issue.status}));
        fields.insert("updated".into(), json!(updated_stamp(issue.updated)));
        if with_links {
            let links: Vec<Value> = self
                .links
                .iter()
                .filter_map(|l| {
                    if l.inward == issue.key {
                        Some(json!({"id": l.id, "type": {"name": l.kind}, "inwardIssue": {"key": l.outward}}))
                    } else if l.outward == issue.key {
                        Some(json!({"id": l.id, "type": {"name": l.kind}, "outwardIssue": {"key": l.inward}}))
                    } else {
                        None
                    }
                })
                .collect();
            fields.insert("issuelinks".into(), json!(links));
        }
        json!({"id": "1", "key": issue.key, "fields": fields})
    }

    fn apply_fields(&mut self, key: &str, fields: &Map<String, Value>) {
        self.clock += 1;
        let clock = self.clock;
        let issue = self.issues.iter_mut().find(|i| i.key == key).unwrap();
        for (k, v) in fields {
            if v.is_null() {
                issue.fields.remove(k);
            } else {
                issue.fields.insert(k.clone(), v.clone());
            }
        }
        issue.updated = clock;
    }
}

fn quoted(text: &str) -> Vec<String> {
    text.split('"')
        .skip(1)
        .step_by(2)
        .map(String::from)
        .collect()
}

fn handle(state: &mut State, method: &str, url: &str, body: &Value) -> (u16, Value, Option<u64>) {
    let (path, _query) = url.split_once('?').unwrap_or((url, ""));
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    let not_found = || {
        (
            404,
            json!({"errorMessages": ["Issue does not exist or you do not have permission to see it."], "errors": {}}),
            None,
        )
    };
    match (method, parts.as_slice()) {
        ("GET", ["rest", "api", "3", "myself"]) => (200, json!({"displayName": "Mock User"}), None),
        ("GET", ["rest", "api", "3", "project", key]) => {
            (200, json!({"key": key, "name": "Mock Project"}), None)
        }
        ("POST", ["rest", "api", "3", "issue"]) => {
            let fields = body["fields"].as_object().cloned().unwrap_or_default();
            let project = fields["project"]["key"].as_str().unwrap_or("");
            if project.is_empty() || fields["issuetype"]["name"].as_str().is_none() {
                return (
                    400,
                    json!({"errorMessages": [], "errors": {"project": "project is required"}}),
                    None,
                );
            }
            if fields.get("summary").and_then(Value::as_str).is_none() {
                return (
                    400,
                    json!({"errorMessages": [], "errors": {"summary": "You must specify a summary of the issue."}}),
                    None,
                );
            }
            state.next_id += 1;
            let key = format!("{project}-{}", state.next_id);
            state.clock += 1;
            let mut stored = fields;
            stored.remove("project");
            state.issues.push(Issue {
                key: key.clone(),
                fields: stored,
                status: "To Do".into(),
                updated: state.clock,
            });
            (
                201,
                json!({"id": state.next_id.to_string(), "key": key, "self": "x"}),
                None,
            )
        }
        ("PUT", ["rest", "api", "3", "issue", key]) => {
            if !state.issues.iter().any(|i| i.key == *key) {
                return not_found();
            }
            let fields = body["fields"].as_object().cloned().unwrap_or_default();
            if fields
                .get("summary")
                .is_some_and(|s| s.as_str().map_or(true, str::is_empty))
            {
                return (
                    400,
                    json!({"errorMessages": [], "errors": {"summary": "bad"}}),
                    None,
                );
            }
            state.apply_fields(key, &fields);
            (204, Value::Null, None)
        }
        ("GET", ["rest", "api", "3", "issue", key]) => {
            match state.issues.iter().find(|i| i.key == *key) {
                Some(issue) => (200, state.render(issue, true), None),
                None => not_found(),
            }
        }
        ("GET", ["rest", "api", "3", "issue", key, "transitions"]) => {
            let Some(issue) = state.issues.iter().find(|i| i.key == *key) else {
                return not_found();
            };
            let list: Vec<Value> = state
                .statuses
                .iter()
                .enumerate()
                .filter(|(_, s)| **s != issue.status)
                .map(|(n, s)| json!({"id": (n + 11).to_string(), "name": format!("Move to {s}"), "to": {"name": s}}))
                .collect();
            (200, json!({"transitions": list}), None)
        }
        ("POST", ["rest", "api", "3", "issue", key, "transitions"]) => {
            let id: usize = body["transition"]["id"]
                .as_str()
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let Some(status) = state.statuses.get(id.wrapping_sub(11)).cloned() else {
                return (
                    400,
                    json!({"errorMessages": ["Invalid transition"], "errors": {}}),
                    None,
                );
            };
            if !state.issues.iter().any(|i| i.key == *key) {
                return not_found();
            }
            state.set_status(key, &status);
            (204, Value::Null, None)
        }
        ("POST", ["rest", "api", "3", "issueLink"]) => {
            let kind = body["type"]["name"].as_str().unwrap_or("").to_string();
            let inward = body["inwardIssue"]["key"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let outward = body["outwardIssue"]["key"]
                .as_str()
                .unwrap_or("")
                .to_string();
            for key in [&inward, &outward] {
                if !state.issues.iter().any(|i| i.key == *key) {
                    return (
                        404,
                        json!({"errorMessages": [format!("Issue {key} does not exist")], "errors": {}}),
                        None,
                    );
                }
            }
            let id = format!("L{}", state.links.len() + 100);
            state.links.push(Link {
                id,
                kind,
                inward,
                outward,
            });
            (201, Value::Null, None)
        }
        ("DELETE", ["rest", "api", "3", "issueLink", id]) => {
            let before = state.links.len();
            state.links.retain(|l| l.id != *id);
            if state.links.len() == before {
                return not_found();
            }
            (204, Value::Null, None)
        }
        ("POST", ["rest", "api", "3", "search", "jql"]) => {
            let jql = body["jql"].as_str().unwrap_or("");
            let values = quoted(jql);
            let matching: Vec<Issue> = state
                .issues
                .iter()
                .filter(|issue| {
                    if jql.contains("labels = ") {
                        let wanted = values.last().cloned().unwrap_or_default();
                        issue.fields["labels"]
                            .as_array()
                            .is_some_and(|l| l.iter().any(|v| v == wanted.as_str()))
                    } else if jql.contains("key in (") {
                        values.contains(&issue.key)
                    } else if jql.contains("updated >= ") {
                        let since = values.last().cloned().unwrap_or_default().replace(' ', "T");
                        updated_stamp(issue.updated).as_str() >= since.as_str()
                    } else {
                        true
                    }
                })
                .cloned()
                .collect();
            let page = if state.page_size == 0 {
                100
            } else {
                state.page_size
            };
            let start: usize = body["nextPageToken"]
                .as_str()
                .and_then(|t| t.parse().ok())
                .unwrap_or(0);
            let slice: Vec<Value> = matching
                .iter()
                .skip(start)
                .take(page)
                .map(|i| state.render(i, false))
                .collect();
            let mut reply = json!({"issues": slice, "isLast": start + page >= matching.len()});
            if start + page < matching.len() {
                reply["nextPageToken"] = json!((start + page).to_string());
            }
            (200, reply, None)
        }
        _ => (
            404,
            json!({"errorMessages": [format!("no mock route for {method} {path}")], "errors": {}}),
            None,
        ),
    }
}

pub fn start() -> Mock {
    let server = Arc::new(Server::http("127.0.0.1:0").unwrap());
    let url = format!("http://{}", server.server_addr().to_ip().unwrap());
    let state = Arc::new(Mutex::new(State {
        statuses: vec!["To Do".into(), "In Progress".into(), "Done".into()],
        ..State::default()
    }));
    let expected = format!(
        "Basic {}",
        simpletickets_connector_jira::http::base64(format!("{EMAIL}:{TOKEN}").as_bytes())
    );
    let (worker, shared) = (Arc::clone(&server), Arc::clone(&state));
    std::thread::spawn(move || {
        for mut request in worker.incoming_requests() {
            let method = match request.method() {
                Method::Get => "GET",
                Method::Post => "POST",
                Method::Put => "PUT",
                Method::Delete => "DELETE",
                _ => "OTHER",
            }
            .to_string();
            let url = request.url().to_string();
            let mut text = String::new();
            let _ = request.as_reader().read_to_string(&mut text);
            let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
            let authorized = request
                .headers()
                .iter()
                .any(|h| h.field.equiv("Authorization") && h.value.as_str() == expected);
            let (status, reply, retry_after) = {
                let mut state = shared.lock().unwrap();
                state.requests.push(Recorded {
                    method: method.clone(),
                    path: url.clone(),
                    body: body.clone(),
                });
                if !authorized {
                    (
                        401,
                        json!({"errorMessages": ["Client must be authenticated to access this resource."]}),
                        None,
                    )
                } else if !state.failures.is_empty() && url.contains(&state.fail_on) {
                    let (status, retry_after) = state.failures.pop_front().unwrap();
                    (
                        status,
                        json!({"errorMessages": ["injected failure"]}),
                        retry_after,
                    )
                } else {
                    handle(&mut state, &method, &url, &body)
                }
            };
            let text = if reply.is_null() {
                String::new()
            } else {
                reply.to_string()
            };
            let mut response = Response::from_string(text)
                .with_status_code(status)
                .with_header(Header::from_bytes("Content-Type", "application/json").unwrap());
            if let Some(seconds) = retry_after {
                response = response
                    .with_header(Header::from_bytes("Retry-After", seconds.to_string()).unwrap());
            }
            let _ = request.respond(response);
        }
    });
    Mock { url, state, server }
}
