//! The HTTP API under `/v1`: JSON over HTTP, token protected, loopback only.

use crate::daemon::{Inner, Shared};
use serde_json::{json, Value};
use simpletickets_core::config::{self, Config};
use simpletickets_core::migration::{self, MigrationError};
use simpletickets_core::ops::{self, NewTicket, OpError, Update};
use simpletickets_core::plugin::{self, Kind};
use simpletickets_core::query::{self, Filter};
use simpletickets_core::search::Query;
use simpletickets_core::templates;
use simpletickets_core::view::{doctor_json, hit_json, ticket_json, tombstones_json};
use simpletickets_core::write::WriteOptions;
use std::io::Read;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// The version of the HTTP API and event contract; the `/v1` routes implement version 1.
pub const API_VERSION: u32 = 1;
/// Response header carrying [`API_VERSION`].
pub const API_VERSION_HEADER: &str = "X-SimpleTickets-Api-Version";

const MAX_BODY: u64 = 1024 * 1024;
const MAX_WAIT_SECONDS: u64 = 30;

/// A parsed HTTP request, independent of the server library.
#[derive(Debug, Clone, Default)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub authorization: Option<String>,
    pub host: Option<String>,
    pub body: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct Reply {
    pub status: u16,
    pub body: Value,
}

fn reply(status: u16, body: Value) -> Reply {
    Reply { status, body }
}

fn error(status: u16, message: impl Into<String>) -> Reply {
    reply(status, json!({"error": message.into()}))
}

impl From<OpError> for Reply {
    fn from(e: OpError) -> Reply {
        let status = match e {
            OpError::NotFound(_) => 404,
            OpError::Invalid(_) => 400,
            OpError::Conflict(_) => 409,
            OpError::Io(_) => 500,
        };
        error(status, e.to_string())
    }
}

/// Accepts requests until the daemon stops.
pub fn serve(server: &tiny_http::Server, shared: &Shared) {
    while !shared.stop.load(Ordering::Relaxed) {
        let Ok(maybe) = server.recv_timeout(Duration::from_millis(250)) else {
            return;
        };
        let Some(mut raw) = maybe else { continue };
        let request = read_request(&mut raw);
        let result = handle(shared, &request);
        let body = serde_json::to_vec(&result.body).unwrap_or_default();
        let header = tiny_http::Header::from_bytes("Content-Type", "application/json")
            .expect("static header");
        let version = tiny_http::Header::from_bytes(API_VERSION_HEADER, API_VERSION.to_string())
            .expect("static header");
        let mut response = tiny_http::Response::from_data(body)
            .with_status_code(result.status)
            .with_header(header)
            .with_header(version);
        if result.status == 401 {
            let challenge =
                tiny_http::Header::from_bytes("WWW-Authenticate", "Bearer").expect("static header");
            response = response.with_header(challenge);
        }
        let _ = raw.respond(response);
    }
}

fn read_request(raw: &mut tiny_http::Request) -> Request {
    let url = raw.url().to_string();
    let (path, query) = match url.split_once('?') {
        Some((p, q)) => (p.to_string(), parse_query(q)),
        None => (url, Vec::new()),
    };
    let header = |name: &'static str| {
        raw.headers()
            .iter()
            .find(|h| h.field.equiv(name))
            .map(|h| h.value.as_str().to_string())
    };
    let authorization = header("Authorization");
    let host = header("Host");
    let mut body = Vec::new();
    let _ = raw.as_reader().take(MAX_BODY + 1).read_to_end(&mut body);
    Request {
        method: raw.method().as_str().to_uppercase(),
        path,
        query,
        authorization,
        host,
        body,
    }
}

pub fn parse_query(q: &str) -> Vec<(String, String)> {
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            (percent_decode(k), percent_decode(v))
        })
        .collect()
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if bytes.len() >= i + 3 => {
                match std::str::from_utf8(&bytes[i + 1..i + 3])
                    .ok()
                    .and_then(|h| u8::from_str_radix(h, 16).ok())
                {
                    Some(b) => {
                        out.push(b);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn is_local_host(host: &str) -> bool {
    let name = if let Some(rest) = host.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        host.split(':').next().unwrap_or("")
    };
    matches!(name, "127.0.0.1" | "localhost" | "::1")
}

/// Authenticates and routes one request.
pub fn handle(shared: &Shared, req: &Request) -> Reply {
    if req.host.as_deref().is_some_and(|h| !is_local_host(h)) {
        return error(403, "requests must use a localhost host name");
    }
    let token = req
        .authorization
        .as_deref()
        .and_then(|a| a.strip_prefix("Bearer "))
        .unwrap_or("");
    if !shared.verify_token(token) {
        return error(401, "missing or invalid token");
    }
    if req.body.len() as u64 > MAX_BODY {
        return error(413, "request body too large");
    }
    let segments: Vec<&str> = req.path.trim_matches('/').split('/').collect();
    let method = req.method.as_str();
    match (segments.as_slice(), method) {
        (["v1", "health"], "GET") => {
            let inner = shared.lock();
            let update = shared
                .available_update()
                .map(|r| json!({"latest": r.tag, "url": r.url}));
            let offline: Vec<Value> = inner
                .store
                .offline_roots()
                .iter()
                .map(|(path, message)| {
                    json!({"root": path.display().to_string(), "message": message})
                })
                .collect();
            reply(
                200,
                json!({
                    "api_version": API_VERSION,
                    "version": simpletickets_core::VERSION,
                    "status": if offline.is_empty() { "ok" } else { "degraded" },
                    "tickets": inner.store.tickets().count(),
                    "frozen": inner.store.frozen_ids().len(),
                    "rules_enabled": shared.rules_enabled(),
                    "offline_roots": offline,
                    "update": update,
                }),
            )
        }
        (["v1", "parents"], "GET") => {
            let inner = shared.lock();
            let items = query::parents(&inner.store)
                .into_iter()
                .map(|e| ticket_json(&inner.store, e, false))
                .collect();
            reply(200, Value::Array(items))
        }
        (["v1", "tickets"], "GET") => list_tickets(shared, req),
        (["v1", "search"], "GET") => search_tickets(shared, req),
        (["v1", "tickets"], "POST") => create_ticket(shared, req),
        (["v1", "tickets", id], "GET") => {
            let inner = shared.lock();
            match inner.store.get(id) {
                Some(e) => reply(200, ticket_json(&inner.store, e, true)),
                None => error(404, format!("no ticket {id}")),
            }
        }
        (["v1", "tickets", id], "PATCH" | "PUT") => update_ticket(shared, req, id),
        (["v1", "tickets", id, "reassign-id"], "POST") => reassign_id(shared, req, id),
        (["v1", "rescan"], "POST") => reply(200, shared.rescan("api").0),
        (["v1", "doctor"], "GET") => {
            let inner = shared.lock();
            reply(200, doctor_json(&inner.store))
        }
        (["v1", "tombstones"], "GET") => {
            let inner = shared.lock();
            reply(200, tombstones_json(&inner.store))
        }
        (["v1", "tombstones", id], "DELETE") => forget_tombstone(shared, id),
        (["v1", "config"], "GET") => get_config(shared),
        (["v1", "config"], "PUT") => put_config(shared, req),
        (["v1", "migration"], "GET") => match migration::load(&shared.dir) {
            Ok(journal) => reply(200, json!({"journal": journal})),
            Err(e) => migration_error(&e),
        },
        (["v1", "migration"], "POST") => start_migration(shared, req),
        (["v1", "migration", "resume"], "POST") => {
            let mut inner = shared.lock();
            match shared.resume_migration(&mut inner) {
                Ok(Some(journal)) => reply(200, json!({"journal": journal})),
                Ok(None) => error(404, "no unfinished migration"),
                Err(e) => migration_error(&e),
            }
        }
        (["v1", "events"], "GET") => events(shared, req),
        (["v1", "audit"], "GET") => {
            let limit = num_param(req, "limit").unwrap_or(100) as usize;
            reply(200, Value::Array(shared.audit.tail(limit)))
        }
        (["v1", "plugins"], "GET") => list_plugins(shared),
        (["v1", "templates"], "GET") => reply(200, json!(templates::list(&shared.dir))),
        (
            ["v1", "health" | "parents" | "tickets" | "search" | "rescan" | "doctor" | "config"
            | "migration" | "events" | "audit" | "templates" | "tombstones" | "plugins"]
            | ["v1", "migration", "resume"]
            | ["v1", "tickets" | "tombstones", _]
            | ["v1", "tickets", _, "reassign-id"],
            _,
        ) => error(405, "method not allowed"),
        _ => error(404, format!("no such route: {} {}", req.method, req.path)),
    }
}

/// Every plugin found, with its manifest data and, for enabled services, what the
/// supervisor knows.
fn list_plugins(shared: &Shared) -> Reply {
    let enabled = shared.lock().config.plugins.clone();
    let found = plugin::discover(&shared.plugin_dirs);
    let runtime = shared
        .plugins
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let mut rows: Vec<Value> = Vec::new();
    for f in &found {
        let is_enabled = enabled.contains(&f.name);
        let mut row = match &f.manifest {
            Ok(m) => m.to_json(),
            Err(_) => json!({"name": f.name}),
        };
        let map = row.as_object_mut().expect("object");
        map.insert("enabled".into(), json!(is_enabled));
        map.insert("source".into(), json!(f.source.as_str()));
        map.insert("dir".into(), json!(f.dir.display().to_string()));
        map.insert("valid".into(), json!(f.manifest.is_ok()));
        map.insert("problem".into(), json!(f.manifest.as_ref().err()));
        let live = runtime.get(&f.name);
        let state = match (&f.manifest, is_enabled, live) {
            (_, false, _) => "disabled",
            (Ok(m), true, None) if m.kind == Kind::StdioClient => "client",
            (_, true, None) => "pending",
            (_, true, Some(rt)) => rt.state.as_str(),
        };
        map.insert("state".into(), json!(state));
        let live = live.map_or_else(
            || crate::supervisor::Runtime::idle().to_json(),
            |rt| rt.to_json(),
        );
        for (k, v) in live.as_object().expect("object") {
            if k != "state" {
                map.insert(k.clone(), v.clone());
            }
        }
        rows.push(row);
    }
    for name in enabled
        .iter()
        .filter(|n| !found.iter().any(|f| &f.name == *n))
    {
        let mut row = json!({"name": name, "enabled": true, "valid": false,
                             "problem": "enabled but not installed", "state": "failed"});
        let map = row.as_object_mut().expect("object");
        for (k, v) in crate::supervisor::Runtime::idle()
            .to_json()
            .as_object()
            .expect("object")
        {
            if k != "state" {
                map.insert(k.clone(), v.clone());
            }
        }
        rows.push(row);
    }
    reply(200, Value::Array(rows))
}

fn num_param(req: &Request, key: &str) -> Option<u64> {
    req.query
        .iter()
        .find(|(k, _)| k == key)
        .and_then(|(_, v)| v.parse().ok())
}

fn parse_body<T: serde::de::DeserializeOwned>(req: &Request) -> Result<T, Reply> {
    serde_json::from_slice(&req.body).map_err(|e| error(400, format!("invalid JSON body: {e}")))
}

/// Lists tickets; with `q` the list is the text matches, best first, each with
/// its score and snippet.
fn list_tickets(shared: &Shared, req: &Request) -> Reply {
    if req.query.iter().any(|(k, _)| k == "q") {
        return search_tickets(shared, req);
    }
    let limit = num_param(req, "limit");
    let pairs: Vec<(String, String)> = req
        .query
        .iter()
        .filter(|(k, _)| k != "limit")
        .cloned()
        .collect();
    let filter = match Filter::from_pairs(&pairs) {
        Ok(f) => f,
        Err(e) => return error(400, e),
    };
    let inner = shared.lock();
    let items = query::list(&inner.store, &filter)
        .into_iter()
        .take(limit.map_or(usize::MAX, |l| l as usize))
        .map(|e| ticket_json(&inner.store, e, false))
        .collect();
    reply(200, Value::Array(items))
}

/// Full-text search over titles and bodies; the other query parameters are the
/// list filters.
fn search_tickets(shared: &Shared, req: &Request) -> Reply {
    let limit = num_param(req, "limit");
    let mut text = None;
    let mut pairs: Vec<(String, String)> = Vec::new();
    for (k, v) in &req.query {
        match k.as_str() {
            "limit" => {}
            "q" => text = Some(v.as_str()),
            _ => pairs.push((k.clone(), v.clone())),
        }
    }
    let Some(text) = text else {
        return error(400, "missing query parameter `q`");
    };
    let query = match Query::parse(text) {
        Ok(q) => q,
        Err(e) => return error(400, e),
    };
    let filter = match Filter::from_pairs(&pairs) {
        Ok(f) => f,
        Err(e) => return error(400, e),
    };
    let inner = shared.lock();
    let items = query::search(&inner.store, &query, &filter)
        .into_iter()
        .take(limit.map_or(usize::MAX, |l| l as usize))
        .map(|(e, hit)| hit_json(&inner.store, e, &hit))
        .collect();
    reply(200, Value::Array(items))
}

fn explicit_write() -> WriteOptions {
    WriteOptions {
        min_age: Duration::ZERO,
    }
}

fn create_ticket(shared: &Shared, req: &Request) -> Reply {
    let new: NewTicket = match parse_body(req) {
        Ok(n) => n,
        Err(r) => return r,
    };
    let mut inner = shared.lock();
    let created = match ops::create(&inner.store, &shared.dir, new) {
        Ok(c) => c,
        Err(e) => return e.into(),
    };
    after_write(shared, &mut inner, "create", &created.id, 201)
}

fn update_ticket(shared: &Shared, req: &Request, id: &str) -> Reply {
    let update: Update = match parse_body(req) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let mut inner = shared.lock();
    match ops::update(&inner.store, id, &update, explicit_write()) {
        Ok(_) => after_write(shared, &mut inner, "update", id, 200),
        Err(e) => e.into(),
    }
}

/// Rescans (running the rules) and returns the ticket as it is now.
fn after_write(shared: &Shared, inner: &mut Inner, reason: &str, id: &str, status: u16) -> Reply {
    shared.rescan_locked(inner, reason);
    match inner.store.get(id) {
        Some(e) => reply(status, ticket_json(&inner.store, e, true)),
        None => error(500, format!("ticket {id} not found after write")),
    }
}

/// Gives a conflicted copy of ticket `id` a new ID. The body may name the copy
/// with `path`; it can be left out when there is exactly one copy.
fn reassign_id(shared: &Shared, req: &Request, id: &str) -> Reply {
    #[derive(serde::Deserialize, Default)]
    #[serde(deny_unknown_fields)]
    struct Body {
        path: Option<String>,
    }
    let body: Body = if req.body.iter().all(u8::is_ascii_whitespace) {
        Body::default()
    } else {
        match parse_body(req) {
            Ok(b) => b,
            Err(r) => return r,
        }
    };
    let mut inner = shared.lock();
    let path = body.path.as_deref().map(std::path::Path::new);
    match ops::reassign_id(&inner.store, id, path, explicit_write()) {
        Ok(done) => {
            shared.events.publish(
                "ticket.id_reassigned",
                json!({"old_id": done.old_id, "new_id": done.new_id,
                       "path": done.path.display().to_string()}),
            );
            let mut reply = after_write(shared, &mut inner, "reassign-id", &done.new_id, 200);
            if let Value::Object(map) = &mut reply.body {
                map.insert("old_id".into(), json!(done.old_id));
            }
            reply
        }
        Err(e) => e.into(),
    }
}

fn forget_tombstone(shared: &Shared, id: &str) -> Reply {
    let mut inner = shared.lock();
    if let Err(e) = ops::forget_tombstone(&inner.store, id) {
        return e.into();
    }
    shared
        .events
        .publish("tombstone.forgotten", json!({"id": id}));
    shared.rescan_locked(&mut inner, "forget");
    reply(200, json!({"forgotten": id}))
}

fn get_config(shared: &Shared) -> Reply {
    let inner = shared.lock();
    let yaml = std::fs::read_to_string(shared.dir.join(config::CONFIG_FILE)).ok();
    reply(200, json!({"yaml": yaml, "config": inner.config.to_json()}))
}

fn migration_error(e: &MigrationError) -> Reply {
    let status = match e {
        MigrationError::Busy => 409,
        MigrationError::Config(_) => 400,
        MigrationError::Journal { .. } => 500,
    };
    error(status, e.to_string())
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigBody {
    yaml: String,
    /// Apply the migration the change requires instead of refusing.
    #[serde(default)]
    migrate: bool,
}

/// Replaces the config. A change that needs a migration is refused with the
/// plan unless `migrate` is set, in which case the migration applies it.
fn put_config(shared: &Shared, req: &Request) -> Reply {
    let body: ConfigBody = match parse_body(req) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let new = match Config::parse(&body.yaml, &shared.dir) {
        Ok(c) => c,
        Err(e) => return error(400, e.to_string()),
    };
    let mut inner = shared.lock();
    match migration::load(&shared.dir) {
        Ok(Some(j)) if !j.is_complete() => return migration_error(&MigrationError::Busy),
        Err(e) => return migration_error(&e),
        _ => {}
    }
    let plan = migration::plan(&inner.store, &new);
    if plan.is_empty() {
        if let Err(e) = config::write_file(&shared.dir, &body.yaml) {
            return error(500, format!("cannot write config: {e}"));
        }
        shared.rescan_locked(&mut inner, "config");
        return reply(
            200,
            json!({"config": inner.config.to_json(), "warnings": plan.warnings}),
        );
    }
    if !body.migrate {
        return reply(
            409,
            json!({"error": "the change requires a migration; resend with \"migrate\": true", "plan": plan}),
        );
    }
    match shared.migrate(&mut inner, &body.yaml) {
        Ok(journal) => reply(
            200,
            json!({"config": inner.config.to_json(), "journal": journal}),
        ),
        Err(e) => migration_error(&e),
    }
}

/// Dry-runs (the default) or applies a migration to the config in `yaml`
/// without going through `PUT /v1/config`.
fn start_migration(shared: &Shared, req: &Request) -> Reply {
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Body {
        yaml: String,
        dry_run: Option<bool>,
    }
    let body: Body = match parse_body(req) {
        Ok(b) => b,
        Err(r) => return r,
    };
    let new = match Config::parse(&body.yaml, &shared.dir) {
        Ok(c) => c,
        Err(e) => return error(400, e.to_string()),
    };
    let mut inner = shared.lock();
    if body.dry_run.unwrap_or(true) {
        let plan = migration::plan(&inner.store, &new);
        return reply(200, json!({"dry_run": true, "plan": plan}));
    }
    match shared.migrate(&mut inner, &body.yaml) {
        Ok(journal) => reply(200, json!({"dry_run": false, "journal": journal})),
        Err(e) => migration_error(&e),
    }
}

fn events(shared: &Shared, req: &Request) -> Reply {
    let since = num_param(req, "since").unwrap_or(0);
    let wait = num_param(req, "wait").unwrap_or(0).min(MAX_WAIT_SECONDS);
    let stream = req
        .query
        .iter()
        .find(|(k, _)| k == "stream")
        .map(|(_, v)| v.as_str());
    let page = shared
        .events
        .since(since, stream, Duration::from_secs(wait), &shared.stop);
    reply(
        200,
        json!({
            "events": page.events,
            "next": page.next,
            "oldest": page.oldest,
            "latest": page.latest,
            "gap": page.gap.is_some(),
            "gap_reason": page.gap.map(|g| g.key()),
            "stream": page.stream,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_queries() {
        let q = parse_query("status=in-progress&title=a%20b+c&field.x=1%2B1&flag");
        assert_eq!(q[0], ("status".into(), "in-progress".into()));
        assert_eq!(q[1].1, "a b c");
        assert_eq!(q[2].1, "1+1");
        assert_eq!(q[3], ("flag".into(), String::new()));
        assert_eq!(percent_decode("100%"), "100%");
        assert_eq!(percent_decode("%zz"), "%zz");
    }

    #[test]
    fn host_check() {
        assert!(is_local_host("127.0.0.1:7878"));
        assert!(is_local_host("localhost"));
        assert!(is_local_host("[::1]:80"));
        assert!(!is_local_host("evil.example:7878"));
        assert!(!is_local_host("127.0.0.1.evil.example"));
    }
}
