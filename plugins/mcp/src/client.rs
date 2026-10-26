//! Minimal HTTP/1.1 client for the daemon's loopback API.

use serde_json::Value;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

/// Anything that can perform API requests; tests substitute a fake.
pub trait Backend {
    /// Performs a request and returns the parsed JSON reply, or the API's error message.
    fn request(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, String>;
}

/// Talks to the daemon at `host:port` with a bearer token.
pub struct HttpBackend {
    address: String,
    token: String,
}

impl HttpBackend {
    /// `url` looks like `http://127.0.0.1:7878`; only plain HTTP is supported.
    pub fn new(url: &str, token: &str) -> Result<HttpBackend, String> {
        let rest = url
            .strip_prefix("http://")
            .ok_or_else(|| format!("unsupported URL `{url}`: only http:// is supported"))?;
        let address = rest.trim_end_matches('/');
        if address.is_empty() || address.contains('/') {
            return Err(format!("invalid URL `{url}`"));
        }
        Ok(HttpBackend {
            address: address.to_string(),
            token: token.trim().to_string(),
        })
    }
}

impl Backend for HttpBackend {
    fn request(&self, method: &str, path: &str, body: Option<&Value>) -> Result<Value, String> {
        let payload = body.map(Value::to_string).unwrap_or_default();
        let mut stream = TcpStream::connect(&self.address)
            .map_err(|e| format!("cannot reach the daemon at {}: {e}", self.address))?;
        let timeout = Some(Duration::from_secs(30));
        let _ = stream.set_read_timeout(timeout);
        let _ = stream.set_write_timeout(timeout);
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nAuthorization: Bearer {}\r\nAccept: application/json\r\n",
            self.address, self.token
        );
        if body.is_some() {
            head.push_str("Content-Type: application/json\r\n");
        }
        head.push_str(&format!(
            "Content-Length: {}\r\n\r\n{payload}",
            payload.len()
        ));
        stream
            .write_all(head.as_bytes())
            .map_err(|e| format!("request failed: {e}"))?;
        let mut raw = Vec::new();
        stream
            .read_to_end(&mut raw)
            .map_err(|e| format!("reading the reply failed: {e}"))?;
        parse_response(&String::from_utf8_lossy(&raw))
    }
}

fn parse_response(text: &str) -> Result<Value, String> {
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or("malformed HTTP reply from the daemon")?;
    let status: u16 = head
        .split(' ')
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or("malformed HTTP status line")?;
    let value: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    if (200..300).contains(&status) {
        return Ok(value);
    }
    let message = value["error"].as_str().unwrap_or("request failed");
    Err(format!("{message} (HTTP {status})"))
}

/// Percent-encodes a query component.
pub fn encode(text: &str) -> String {
    let mut out = String::new();
    for b in text.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_reserved_characters() {
        assert_eq!(encode("a b&c=é"), "a%20b%26c%3D%C3%A9");
        assert_eq!(encode("ST-1_x.y~"), "ST-1_x.y~");
    }

    #[test]
    fn parses_success_and_error_replies() {
        let ok = parse_response("HTTP/1.1 200 OK\r\nX: y\r\n\r\n{\"a\":1}").unwrap();
        assert_eq!(ok["a"], 1);
        let err = parse_response("HTTP/1.1 404 Not Found\r\n\r\n{\"error\":\"no ticket X\"}")
            .unwrap_err();
        assert_eq!(err, "no ticket X (HTTP 404)");
        assert!(parse_response("garbage").is_err());
    }

    #[test]
    fn rejects_unsupported_urls() {
        assert!(HttpBackend::new("https://x", "t").is_err());
        assert!(HttpBackend::new("http://x/path", "t").is_err());
        assert!(HttpBackend::new("http://127.0.0.1:1/", "t").is_ok());
    }
}
