mod common;

use common::*;
use serde_json::{json, Value};
use simpletickets::webhooks::{
    sign, Backoff, DELIVERY_HEADER, EVENT_HEADER, SIGNATURE_HEADER, TIMESTAMP_HEADER,
};
use std::collections::{HashMap, VecDeque};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const SLOW: Duration = Duration::from_secs(3600);
const SECRET_VAR: &str = "ST_TEST_WEBHOOK_SECRET";
const SECRET: &str = "s3cret-value";
const UNSET_VAR: &str = "ST_TEST_WEBHOOK_SECRET_UNSET";

static ENV: Once = Once::new();

fn set_env() {
    ENV.call_once(|| {
        std::env::set_var(SECRET_VAR, SECRET);
        std::env::remove_var(UNSET_VAR);
    });
}

fn fast() -> Backoff {
    Backoff {
        attempts: 4,
        base: Duration::from_millis(20),
        cap: Duration::from_millis(80),
        timeout: Duration::from_secs(2),
    }
}

#[derive(Clone)]
struct Hit {
    headers: HashMap<String, String>,
    body: String,
}

impl Hit {
    fn event(&self) -> Value {
        serde_json::from_str(&self.body).unwrap()
    }
    fn kind(&self) -> String {
        self.event()["type"].as_str().unwrap().to_string()
    }
}

/// A local HTTP server that records requests and answers with scripted statuses.
struct Receiver {
    url: String,
    hits: Arc<Mutex<Vec<Hit>>>,
    script: Arc<Mutex<VecDeque<u16>>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Receiver {
    fn start(statuses: &[u16]) -> Receiver {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let url = format!("http://{}/hook", server.server_addr().to_ip().unwrap());
        let hits = Arc::new(Mutex::new(Vec::new()));
        let script = Arc::new(Mutex::new(VecDeque::from(statuses.to_vec())));
        let stop = Arc::new(AtomicBool::new(false));
        let (h, sc, st) = (hits.clone(), script.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            while !st.load(Ordering::Relaxed) {
                let Ok(Some(mut request)) = server.recv_timeout(Duration::from_millis(50)) else {
                    continue;
                };
                let headers = request
                    .headers()
                    .iter()
                    .map(|x| {
                        (
                            x.field.as_str().as_str().to_lowercase(),
                            x.value.as_str().to_string(),
                        )
                    })
                    .collect();
                let mut body = String::new();
                std::io::Read::read_to_string(request.as_reader(), &mut body).unwrap();
                h.lock().unwrap().push(Hit { headers, body });
                let status = sc.lock().unwrap().pop_front().unwrap_or(200);
                let _ = request.respond(tiny_http::Response::empty(status));
            }
        });
        Receiver {
            url,
            hits,
            script,
            stop,
            thread: Some(thread),
        }
    }

    fn hits(&self) -> Vec<Hit> {
        self.hits.lock().unwrap().clone()
    }

    fn wait_for(&self, count: usize) -> Vec<Hit> {
        let until = Instant::now() + Duration::from_secs(15);
        while Instant::now() < until {
            let hits = self.hits();
            if hits.len() >= count {
                return hits;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("expected {count} deliveries, got {}", self.hits().len());
    }
}

impl Drop for Receiver {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        let _ = &self.script;
    }
}

fn config(hooks: &[(&str, &[&str], Option<&str>)]) -> String {
    let mut yaml = String::from("webhooks:\n");
    for (url, events, secret) in hooks {
        yaml.push_str(&format!("  - url: {url}\n"));
        let globs: Vec<String> = events.iter().map(|e| format!("\"{e}\"")).collect();
        yaml.push_str(&format!("    events: [{}]\n", globs.join(", ")));
        if let Some(s) = secret {
            yaml.push_str(&format!("    secret_env: {s}\n"));
        }
    }
    yaml
}

fn start_hooked(dir: &std::path::Path, yaml: &str) -> Api {
    set_env();
    write(dir, "simpletickets.yml", yaml);
    start_with(dir, false, SLOW, |o| o.webhook_backoff = fast())
}

#[test]
fn matching_events_are_posted_signed() {
    let receiver = Receiver::start(&[]);
    let dir = tempfile::tempdir().unwrap();
    let yaml = config(&[(
        &receiver.url,
        &["ticket.created", "ticket.status_changed"],
        Some(SECRET_VAR),
    )]);
    let api = start_hooked(dir.path(), &yaml);
    let (_, created) = api.post("/v1/tickets", r#"{"title":"Hooked"}"#);
    let id = created["id"].as_str().unwrap();
    api.patch(
        &format!("/v1/tickets/{id}"),
        r#"{"status":"in-progress","priority":"high"}"#,
    );

    let hits = receiver.wait_for(2);
    std::thread::sleep(Duration::from_millis(300));
    let hits_after = receiver.hits();
    assert_eq!(hits_after.len(), hits.len(), "only subscribed types arrive");
    let kinds: Vec<String> = hits_after.iter().map(Hit::kind).collect();
    assert_eq!(kinds, ["ticket.created", "ticket.status_changed"]);

    for hit in &hits_after {
        let event = hit.event();
        assert_eq!(event["data"]["id"], id);
        assert!(event["seq"].is_u64() && event["time"].is_string());
        assert_eq!(hit.headers["content-type"], "application/json");
        assert_eq!(hit.headers["x-simpletickets-api-version"], "1");
        assert_eq!(hit.headers[&EVENT_HEADER.to_lowercase()], hit.kind());
        assert!(
            hit.headers[&DELIVERY_HEADER.to_lowercase()].ends_with(&format!("-{}", event["seq"]))
        );
        let timestamp = &hit.headers[&TIMESTAMP_HEADER.to_lowercase()];
        let expected = sign(SECRET.as_bytes(), timestamp, hit.body.as_bytes());
        assert_eq!(hit.headers[&SIGNATURE_HEADER.to_lowercase()], expected);
        assert!(!hit.body.contains(SECRET));
    }
    let seqs: Vec<u64> = hits_after
        .iter()
        .map(|h| h.event()["seq"].as_u64().unwrap())
        .collect();
    assert!(seqs[0] < seqs[1]);
}

#[test]
fn a_webhook_without_secret_is_unsigned_and_gets_everything_it_asks_for() {
    let receiver = Receiver::start(&[]);
    let dir = tempfile::tempdir().unwrap();
    let api = start_hooked(dir.path(), &config(&[(&receiver.url, &["*"], None)]));
    api.post("/v1/tickets", r#"{"title":"All"}"#);
    let hits = receiver.wait_for(3);
    assert!(hits
        .iter()
        .all(|h| !h.headers.contains_key(&SIGNATURE_HEADER.to_lowercase())));
    let kinds: Vec<String> = hits.iter().map(Hit::kind).collect();
    assert!(kinds.contains(&"ticket.created".to_string()), "{kinds:?}");
    assert!(kinds.contains(&"scan".to_string()), "{kinds:?}");
}

#[test]
fn failed_deliveries_are_retried_with_the_same_payload() {
    let receiver = Receiver::start(&[500, 503, 200]);
    let dir = tempfile::tempdir().unwrap();
    let api = start_hooked(
        dir.path(),
        &config(&[(&receiver.url, &["ticket.created"], Some(SECRET_VAR))]),
    );
    api.post("/v1/tickets", r#"{"title":"Retry"}"#);
    let hits = receiver.wait_for(3);
    std::thread::sleep(Duration::from_millis(400));
    let hits_after = receiver.hits();
    assert_eq!(hits_after.len(), 3, "a delivered event is not sent again");
    assert!(hits.iter().all(|h| h.body == hits[0].body));
    let ids: Vec<&String> = hits
        .iter()
        .map(|h| &h.headers[&DELIVERY_HEADER.to_lowercase()])
        .collect();
    assert!(ids.iter().all(|i| *i == ids[0]));
    for hit in &hits {
        let ts = &hit.headers[&TIMESTAMP_HEADER.to_lowercase()];
        assert_eq!(
            hit.headers[&SIGNATURE_HEADER.to_lowercase()],
            sign(SECRET.as_bytes(), ts, hit.body.as_bytes())
        );
    }
}

#[test]
fn an_event_is_dropped_after_the_last_attempt() {
    let receiver = Receiver::start(&[500; 10]);
    let dir = tempfile::tempdir().unwrap();
    let api = start_hooked(
        dir.path(),
        &config(&[(&receiver.url, &["ticket.created"], None)]),
    );
    api.post("/v1/tickets", r#"{"title":"Doomed"}"#);
    receiver.wait_for(4);
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(receiver.hits().len(), 4, "attempts are capped");
    assert_eq!(api.get("/v1/health").0, 200);
}

#[test]
fn an_unreachable_receiver_never_blocks_the_api_or_other_webhooks() {
    let dead = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        format!("http://{}/hook", listener.local_addr().unwrap())
    };
    let live = Receiver::start(&[]);
    let dir = tempfile::tempdir().unwrap();
    let yaml = config(&[
        (&dead, &["ticket.*"], None),
        (&live.url, &["ticket.*"], None),
    ]);
    let api = start_hooked(dir.path(), &yaml);
    let started = Instant::now();
    for i in 0..5 {
        let (status, _) = api.post("/v1/tickets", &format!(r#"{{"title":"T{i}"}}"#));
        assert_eq!(status, 201);
    }
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "writes are not held up by the dead receiver"
    );
    let hits = live.wait_for(5);
    let seqs: Vec<u64> = hits
        .iter()
        .map(|h| h.event()["seq"].as_u64().unwrap())
        .collect();
    assert!(seqs.windows(2).all(|w| w[0] < w[1]), "in order: {seqs:?}");
}

#[test]
fn a_missing_secret_variable_drops_the_delivery() {
    let receiver = Receiver::start(&[]);
    let dir = tempfile::tempdir().unwrap();
    let api = start_hooked(
        dir.path(),
        &config(&[(&receiver.url, &["ticket.created"], Some(UNSET_VAR))]),
    );
    let (status, _) = api.post("/v1/tickets", r#"{"title":"Unsigned"}"#);
    assert_eq!(status, 201);
    std::thread::sleep(Duration::from_millis(800));
    assert!(receiver.hits().is_empty(), "nothing is sent unsigned");
}

#[test]
fn a_webhook_added_through_the_config_route_takes_effect() {
    let receiver = Receiver::start(&[]);
    let dir = tempfile::tempdir().unwrap();
    set_env();
    let api = start_with(dir.path(), false, SLOW, |o| o.webhook_backoff = fast());
    api.post("/v1/tickets", r#"{"title":"Before"}"#);
    let yaml = config(&[(&receiver.url, &["ticket.created"], None)]);
    let (status, body) = api.call(
        "PUT",
        "/v1/config",
        Some(&json!({"yaml": yaml}).to_string()),
    );
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["config"]["webhooks"][0]["url"], receiver.url.as_str());
    api.post("/v1/tickets", r#"{"title":"After"}"#);
    let hits = receiver.wait_for(1);
    assert_eq!(hits[0].event()["data"]["title"], "After");
}

#[test]
fn only_http_and_https_urls_are_accepted() {
    let dir = tempfile::tempdir().unwrap();
    let api = start(dir.path(), false, SLOW);
    for url in ["ftp://example.org/x", "file:///etc/passwd", "example.org"] {
        let yaml = format!("webhooks:\n  - url: \"{url}\"\n");
        let (status, body) = api.call(
            "PUT",
            "/v1/config",
            Some(&json!({"yaml": yaml}).to_string()),
        );
        assert_eq!(status, 400, "{url}");
        assert!(body["error"].as_str().unwrap().contains("http"), "{body}");
    }
    let yaml = "webhooks:\n  - url: https://example.org/x\n  - url: http://192.168.1.5:8080/y\n  - url: http://localhost/z\n";
    let (status, _) = api.call(
        "PUT",
        "/v1/config",
        Some(&json!({"yaml": yaml}).to_string()),
    );
    assert_eq!(status, 200, "private and local targets are allowed");
}
