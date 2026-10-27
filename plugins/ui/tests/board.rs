use serde_json::{json, Value};
use simpletickets::daemon::{self, Options, Running as Daemon};
use simpletickets_ui::client::Daemon as DaemonTarget;
use simpletickets_ui::server::{self, Running as Board};
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

struct Reply {
    status: u16,
    head: String,
    body: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

struct Req<'a> {
    method: &'a str,
    path: &'a str,
    host: Option<String>,
    origin: Option<String>,
    session: Option<String>,
    body: Option<&'a str>,
}

fn req<'a>(method: &'a str, path: &'a str) -> Req<'a> {
    Req {
        method,
        path,
        host: None,
        origin: None,
        session: None,
        body: None,
    }
}

fn send(addr: SocketAddr, r: Req) -> Reply {
    let mut stream = std::net::TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let body = r.body.unwrap_or("");
    let host = r.host.unwrap_or_else(|| addr.to_string());
    let mut text = format!(
        "{} {} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n",
        r.method, r.path
    );
    if let Some(o) = r.origin {
        text.push_str(&format!("Origin: {o}\r\n"));
    }
    if let Some(s) = r.session {
        text.push_str(&format!("X-SimpleTickets-Session: {s}\r\n"));
    }
    text.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    stream.write_all(text.as_bytes()).unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let raw = String::from_utf8_lossy(&raw).to_string();
    let status = raw.split(' ').nth(1).unwrap().parse().unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
    Reply {
        status,
        head: head.to_string(),
        body: body.to_string(),
    }
}

struct Setup {
    _dir: tempfile::TempDir,
    daemon: Option<Daemon>,
    board: Option<Board>,
    token: String,
}

impl Setup {
    fn addr(&self) -> SocketAddr {
        self.board.as_ref().unwrap().addr
    }

    fn session(&self) -> String {
        self.board.as_ref().unwrap().session.clone()
    }

    /// A request carrying the board's own origin and session, as the page sends.
    fn page_req<'a>(&self, method: &'a str, path: &'a str, body: Option<&'a str>) -> Req<'a> {
        Req {
            method,
            path,
            host: None,
            origin: Some(format!("http://{}", self.addr())),
            session: Some(self.session()),
            body,
        }
    }
}

impl Drop for Setup {
    fn drop(&mut self) {
        if let Some(b) = self.board.take() {
            b.stop();
        }
        if let Some(d) = self.daemon.take() {
            d.stop();
        }
    }
}

fn write(dir: &Path, name: &str, text: &str) {
    std::fs::write(dir.join(name), text).unwrap();
}

fn setup() -> Setup {
    let dir = tempfile::tempdir().unwrap();
    write(
        dir.path(),
        "a.md",
        "---\nid: ST-1\ntitle: First\nstatus: todo\npriority: high\ntags: [work]\n---\nbody of first\n",
    );
    write(
        dir.path(),
        "b.md",
        "---\nid: ST-2\ntitle: Second\nstatus: todo\nblocked_by: [ST-1]\n---\n",
    );
    let mut options = Options::new(dir.path().to_path_buf());
    options.port = 0;
    options.watch = false;
    options.scan_interval = Duration::from_secs(3600);
    let daemon = daemon::start(options).unwrap();
    let token_file = daemon.token_file();
    let token_file = token_file.to_path_buf();
    let token = std::fs::read_to_string(&token_file)
        .unwrap()
        .trim()
        .to_string();
    let target = DaemonTarget::new(&format!("http://{}", daemon.addr), token_file).unwrap();
    let board = server::start(0, target).unwrap();
    Setup {
        _dir: dir,
        daemon: Some(daemon),
        board: Some(board),
        token,
    }
}

#[test]
fn page_carries_a_session_token_and_never_the_api_token() {
    let s = setup();
    let page = send(s.addr(), req("GET", "/"));
    assert_eq!(page.status, 200);
    assert!(page.body.contains(&s.session()));
    assert!(!page.body.contains("__SESSION__"));
    assert_ne!(s.session(), s.token);
    assert_eq!(s.session().len(), 64);
    assert!(page.head.contains("Content-Security-Policy"));
    assert!(page.head.to_lowercase().contains("x-frame-options: deny"));
    for path in ["/", "/app.js", "/app.css", "/favicon.ico"] {
        let r = send(s.addr(), req("GET", path));
        assert!(!r.body.contains(&s.token), "{path}");
        assert!(!r.head.contains(&s.token), "{path}");
    }
}

#[test]
fn assets_load_nothing_from_the_network() {
    let s = setup();
    for path in ["/", "/app.js", "/app.css"] {
        let body = send(s.addr(), req("GET", path)).body;
        assert!(!body.contains("http://"), "{path}");
        assert!(!body.contains("https://"), "{path}");
        assert!(!body.contains("//cdn"), "{path}");
    }
}

#[test]
fn refuses_foreign_host_headers() {
    let s = setup();
    for host in ["evil.example", "127.0.0.1.evil.example:80", "10.0.0.5"] {
        let mut page = req("GET", "/");
        page.host = Some(host.to_string());
        assert_eq!(send(s.addr(), page).status, 403, "{host}");
        let mut api = s.page_req("GET", "/api/v1/tickets", None);
        api.host = Some(host.to_string());
        assert_eq!(send(s.addr(), api).status, 403, "{host}");
    }
    let mut ok = req("GET", "/");
    ok.host = Some(format!("localhost:{}", s.addr().port()));
    assert_eq!(send(s.addr(), ok).status, 200);
}

#[test]
fn api_requires_the_session_header() {
    let s = setup();
    let none = send(s.addr(), req("GET", "/api/v1/tickets"));
    assert_eq!(none.status, 403);
    let mut wrong = req("GET", "/api/v1/tickets");
    wrong.session = Some("0".repeat(64));
    assert_eq!(send(s.addr(), wrong).status, 403);
    let mut bearer = req("GET", "/api/v1/tickets");
    bearer.session = Some(s.token.clone());
    assert_eq!(
        send(s.addr(), bearer).status,
        403,
        "the API token is not a session"
    );
    let ok = send(s.addr(), s.page_req("GET", "/api/v1/tickets", None));
    assert_eq!(ok.status, 200);
    assert_eq!(ok.json().as_array().unwrap().len(), 2);
}

#[test]
fn state_changes_need_the_boards_own_origin() {
    let s = setup();
    let create = r#"{"title":"From board"}"#;
    let mut no_origin = s.page_req("POST", "/api/v1/tickets", Some(create));
    no_origin.origin = None;
    assert_eq!(send(s.addr(), no_origin).status, 403);
    for origin in ["http://evil.example", "null", "https://127.0.0.1"] {
        let mut cross = s.page_req("POST", "/api/v1/tickets", Some(create));
        cross.origin = Some(origin.to_string());
        assert_eq!(send(s.addr(), cross).status, 403, "{origin}");
    }
    let mut patch = s.page_req(
        "PATCH",
        "/api/v1/tickets/ST-1",
        Some(r#"{"status":"done"}"#),
    );
    patch.origin = Some("http://evil.example".into());
    assert_eq!(send(s.addr(), patch).status, 403);
    let count = send(s.addr(), s.page_req("GET", "/api/v1/tickets", None))
        .json()
        .as_array()
        .unwrap()
        .len();
    assert_eq!(count, 2, "refused requests changed nothing");
}

#[test]
fn relays_reads_and_writes_without_leaking_the_token() {
    let s = setup();
    let mut seen = Vec::new();

    let health = send(s.addr(), s.page_req("GET", "/api/v1/health", None));
    assert_eq!(health.status, 200);
    assert_eq!(health.json()["api_version"], 1);
    assert!(health.head.contains("X-SimpleTickets-Api-Version: 1"));
    seen.push(health);

    let created = send(
        s.addr(),
        s.page_req(
            "POST",
            "/api/v1/tickets",
            Some(r#"{"title":"From board","priority":"low","tags":["ui"]}"#),
        ),
    );
    assert_eq!(created.status, 201);
    let id = created.json()["id"].as_str().unwrap().to_string();
    seen.push(created);

    let moved = send(
        s.addr(),
        s.page_req(
            "PATCH",
            &format!("/api/v1/tickets/{id}"),
            Some(r#"{"status":"in-progress"}"#),
        ),
    );
    assert_eq!(moved.status, 200);
    assert_eq!(moved.json()["status"], "in-progress");
    seen.push(moved);

    let detail = send(s.addr(), s.page_req("GET", "/api/v1/tickets/ST-1", None));
    assert_eq!(detail.json()["body"], "body of first\n");
    assert_eq!(detail.json()["blocks"], json!(["ST-2"]));
    seen.push(detail);

    let search = send(s.addr(), s.page_req("GET", "/api/v1/search?q=first", None));
    assert_eq!(search.status, 200);
    assert_eq!(search.json()[0]["id"], "ST-1");
    seen.push(search);

    let config = send(s.addr(), s.page_req("GET", "/api/v1/config", None));
    assert!(config.json()["config"]["statuses"]["todo"].is_array());
    seen.push(config);

    let doctor = send(s.addr(), s.page_req("GET", "/api/v1/doctor", None));
    assert!(doctor.json()["warnings"].is_array());
    seen.push(doctor);

    let events = send(
        s.addr(),
        s.page_req("GET", "/api/v1/events?since=0&wait=1", None),
    );
    assert_eq!(events.status, 200);
    assert!(events.json()["events"].is_array());
    seen.push(events);

    let missing = send(s.addr(), s.page_req("GET", "/api/v1/tickets/ST-404", None));
    assert_eq!(missing.status, 404);
    assert!(missing.json()["error"].is_string());
    seen.push(missing);

    for reply in &seen {
        assert!(!reply.body.contains(&s.token));
        assert!(!reply.head.contains(&s.token));
        assert!(!reply.head.to_lowercase().contains("authorization"));
    }

    let direct = s
        .daemon
        .as_ref()
        .map(|d| send_with_token(d.addr, "GET", &format!("/v1/tickets/{id}"), &s.token).json())
        .unwrap();
    assert_eq!(direct["status"], "in-progress");
}

fn send_with_token(addr: SocketAddr, method: &str, path: &str, token: &str) -> Reply {
    let mut stream = std::net::TcpStream::connect(addr).unwrap();
    let text = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\nAuthorization: Bearer {token}\r\nContent-Length: 0\r\n\r\n"
    );
    stream.write_all(text.as_bytes()).unwrap();
    let mut raw = String::new();
    stream.read_to_string(&mut raw).unwrap();
    let status = raw.split(' ').nth(1).unwrap().parse().unwrap();
    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((&raw, ""));
    Reply {
        status,
        head: head.to_string(),
        body: body.to_string(),
    }
}

#[test]
fn only_the_boards_routes_reach_the_daemon() {
    let s = setup();
    let blocked = [
        ("PUT", "/api/v1/config", Some(r#"{"yaml":"id_prefix: X"}"#)),
        ("POST", "/api/v1/migration", Some(r#"{"yaml":""}"#)),
        ("DELETE", "/api/v1/tombstones/ST-1", None),
        ("POST", "/api/v1/tickets/ST-1/reassign-id", Some("{}")),
        ("GET", "/api/v1/audit", None),
        ("GET", "/api/v1/tickets/../config", None),
        ("GET", "/api/v2/health", None),
        ("GET", "/v1/tickets", None),
    ];
    for (method, path, body) in blocked {
        let r = send(s.addr(), s.page_req(method, path, body));
        assert_eq!(r.status, 404, "{method} {path}");
    }
    let page_post = send(s.addr(), s.page_req("POST", "/", None));
    assert_eq!(page_post.status, 405);
}

#[test]
fn reports_an_unreachable_daemon_without_exposing_paths_or_tokens() {
    let dir = tempfile::tempdir().unwrap();
    let token_file = dir.path().join("token");
    std::fs::write(&token_file, "super-secret-token\n").unwrap();
    let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}", dead.local_addr().unwrap());
    drop(dead);
    let board = server::start(0, DaemonTarget::new(&url, token_file).unwrap()).unwrap();
    let mut r = req("GET", "/api/v1/health");
    r.origin = Some(format!("http://{}", board.addr));
    r.session = Some(board.session.clone());
    let reply = send(board.addr, r);
    assert_eq!(reply.status, 502);
    assert!(reply.json()["error"].as_str().unwrap().contains("daemon"));
    assert!(!reply.body.contains("super-secret-token"));
    board.stop();

    let missing = dir.path().join("nope");
    let board = server::start(0, DaemonTarget::new(&url, missing.clone()).unwrap()).unwrap();
    let mut r = req("GET", "/api/v1/health");
    r.session = Some(board.session.clone());
    let reply = send(board.addr, r);
    assert_eq!(reply.status, 502);
    assert!(!reply.body.contains(missing.to_str().unwrap()));
    board.stop();
}

#[test]
fn binds_the_loopback_interface_only() {
    let s = setup();
    assert!(s.addr().ip().is_loopback());
}
