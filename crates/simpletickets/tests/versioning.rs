mod common;

use common::*;
use serde_json::Value;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const SLOW: Duration = Duration::from_secs(3600);

/// Status line, lower-cased headers and body of one request.
fn exchange(api: &Api, method: &str, path: &str, token: Option<&str>) -> (u16, String, String) {
    let mut stream = TcpStream::connect(api.addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nContent-Length: 0\r\n",
        api.addr
    );
    if let Some(t) = token {
        request.push_str(&format!("Authorization: Bearer {t}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let status = response.split(' ').nth(1).unwrap().parse().unwrap();
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    (status, head.to_lowercase(), body.to_string())
}

#[test]
fn health_reports_the_api_version_and_the_daemon_version() {
    let dir = tempfile::tempdir().unwrap();
    let api = start(dir.path(), false, SLOW);
    let (status, health) = api.get("/v1/health");
    assert_eq!(status, 200);
    assert_eq!(health["api_version"], 1);
    assert!(health["api_version"].is_u64());
    assert_eq!(health["version"], env!("CARGO_PKG_VERSION"));
}

#[test]
fn every_response_carries_the_api_version_header() {
    let dir = tempfile::tempdir().unwrap();
    let api = start(dir.path(), false, SLOW);
    let token = api.token.clone();
    let cases = [
        ("GET", "/v1/health", Some(token.as_str()), 200),
        ("GET", "/v1/health", None, 401),
        ("DELETE", "/v1/health", Some(token.as_str()), 405),
        ("GET", "/v1/tickets/NOPE", Some(token.as_str()), 404),
        ("GET", "/v1/nothing", Some(token.as_str()), 404),
    ];
    for (method, path, token, expected) in cases {
        let (status, head, _) = exchange(&api, method, path, token);
        assert_eq!(status, expected, "{method} {path}");
        assert!(
            head.contains("x-simpletickets-api-version: 1\r\n"),
            "{method} {path}: {head}"
        );
    }
}

#[test]
fn unknown_routes_are_a_json_404() {
    let dir = tempfile::tempdir().unwrap();
    let api = start(dir.path(), false, SLOW);
    for path in ["/v1/nothing", "/v2/health", "/", "/v1/tickets/ST-1/nothing"] {
        let (status, head, body) = exchange(&api, "GET", path, Some(&api.token));
        assert_eq!(status, 404, "{path}");
        assert!(head.contains("content-type: application/json"), "{head}");
        let json: Value = serde_json::from_str(&body).unwrap();
        assert!(json["error"].as_str().unwrap().contains("no such route"));
    }
    let (status, _, _) = exchange(&api, "GET", "/v2/health", None);
    assert_eq!(
        status, 401,
        "an unauthenticated client learns nothing about routes"
    );
}
