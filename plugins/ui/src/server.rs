//! The board's web server: static page, session issue and the relay to the daemon.

use crate::client::Daemon;
use crate::guard::{self, RequestInfo, SESSION_HEADER};
use serde_json::json;
use std::io::Read;
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use tiny_http::{Header, Request, Response, Server};

const INDEX: &str = include_str!("../assets/index.html");
const SCRIPT: &str = include_str!("../assets/app.js");
const STYLE: &str = include_str!("../assets/app.css");
const SESSION_PLACEHOLDER: &str = "__SESSION__";
const MAX_BODY: u64 = 1024 * 1024;
const MAX_CONNECTIONS: usize = 64;
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; connect-src 'self'; \
img-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'";

struct Shared {
    daemon: Daemon,
    session: String,
    active: AtomicUsize,
}

/// A running board server.
pub struct Running {
    pub addr: SocketAddr,
    pub session: String,
    server: Arc<Server>,
    accept: Option<JoinHandle<()>>,
}

impl Running {
    /// Stops accepting requests and waits for the accept loop to end.
    pub fn stop(mut self) {
        self.server.unblock();
        if let Some(handle) = self.accept.take() {
            let _ = handle.join();
        }
    }

    /// Blocks until the server stops.
    pub fn wait(mut self) {
        if let Some(handle) = self.accept.take() {
            let _ = handle.join();
        }
    }
}

/// A fresh random session token, 256 bits as hex.
pub fn new_session() -> Result<String, String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| format!("no random source: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Binds `127.0.0.1:port` (0 picks a free port) and serves in the background.
pub fn start(port: u16, daemon: Daemon) -> Result<Running, String> {
    let session = new_session()?;
    let server = Server::http(SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port))
        .map_err(|e| format!("cannot listen on 127.0.0.1:{port}: {e}"))?;
    let addr = server
        .server_addr()
        .to_ip()
        .ok_or("the server is not bound to an IP address")?;
    let server = Arc::new(server);
    let shared = Arc::new(Shared {
        daemon,
        session: session.clone(),
        active: AtomicUsize::new(0),
    });
    let accept = {
        let server = Arc::clone(&server);
        thread::spawn(move || {
            for request in server.incoming_requests() {
                if shared.active.fetch_add(1, Ordering::SeqCst) >= MAX_CONNECTIONS {
                    shared.active.fetch_sub(1, Ordering::SeqCst);
                    respond_error(request, 503, "too many open requests");
                    continue;
                }
                let shared = Arc::clone(&shared);
                thread::spawn(move || {
                    handle(request, &shared);
                    shared.active.fetch_sub(1, Ordering::SeqCst);
                });
            }
        })
    };
    Ok(Running {
        addr,
        session,
        server,
        accept: Some(accept),
    })
}

fn header_value(request: &Request, name: &str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|h| h.value.as_str().to_string())
}

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("static header is valid")
}

fn with_common<R: Read>(response: Response<R>, content_type: &str) -> Response<R> {
    response
        .with_header(header("Content-Type", content_type))
        .with_header(header("Cache-Control", "no-store"))
        .with_header(header("X-Content-Type-Options", "nosniff"))
        .with_header(header("X-Frame-Options", "DENY"))
        .with_header(header("Referrer-Policy", "no-referrer"))
        .with_header(header("Content-Security-Policy", CSP))
}

fn respond_error(request: Request, status: u16, message: &str) {
    let body = json!({ "error": message }).to_string();
    let response = with_common(
        Response::from_data(body.into_bytes()).with_status_code(status),
        "application/json",
    );
    let _ = request.respond(response);
}

fn respond_static(request: Request, text: String, content_type: &str) {
    let _ = request.respond(with_common(
        Response::from_data(text.into_bytes()),
        content_type,
    ));
}

fn handle(mut request: Request, shared: &Shared) {
    let method = request.method().as_str().to_string();
    let url = request.url().to_string();
    let (path, query) = url.split_once('?').unwrap_or((url.as_str(), ""));
    let host = header_value(&request, "Host");
    let origin = header_value(&request, "Origin");
    let session = header_value(&request, SESSION_HEADER);
    let fetch_site = header_value(&request, "Sec-Fetch-Site");
    let info = RequestInfo {
        method: &method,
        host: host.as_deref(),
        origin: origin.as_deref(),
        session: session.as_deref(),
        fetch_site: fetch_site.as_deref(),
    };

    if path.starts_with("/api/") {
        if let Err((status, message)) = guard::check_api(&info, &shared.session) {
            return respond_error(request, status, message);
        }
        let Some(route) = guard::daemon_route(&method, path, query) else {
            return respond_error(request, 404, "no such board route");
        };
        let mut body = Vec::new();
        if request
            .as_reader()
            .take(MAX_BODY + 1)
            .read_to_end(&mut body)
            .is_err()
        {
            return respond_error(request, 400, "could not read the request body");
        }
        if body.len() as u64 > MAX_BODY {
            return respond_error(request, 413, "request body is over 1 MiB");
        }
        return match shared.daemon.forward(&method, &route, &body) {
            Ok(reply) => {
                let mut response = with_common(
                    Response::from_data(reply.body).with_status_code(reply.status),
                    &reply.content_type,
                );
                if let Some(version) = reply.api_version {
                    response =
                        response.with_header(header("X-SimpleTickets-Api-Version", &version));
                }
                let _ = request.respond(response);
            }
            Err(message) => respond_error(request, 502, &message),
        };
    }

    if let Err((status, message)) = guard::check_host(&info) {
        return respond_error(request, status, message);
    }
    if !matches!(method.as_str(), "GET" | "HEAD") {
        return respond_error(request, 405, "method not allowed");
    }
    match path {
        "/" | "/index.html" => respond_static(
            request,
            INDEX.replace(SESSION_PLACEHOLDER, &shared.session),
            "text/html; charset=utf-8",
        ),
        "/app.js" => respond_static(
            request,
            SCRIPT.to_string(),
            "text/javascript; charset=utf-8",
        ),
        "/app.css" => respond_static(request, STYLE.to_string(), "text/css; charset=utf-8"),
        "/favicon.ico" => {
            let _ = request.respond(
                with_common(Response::from_data(Vec::new()), "image/x-icon").with_status_code(204),
            );
        }
        _ => respond_error(request, 404, "not found"),
    }
}
