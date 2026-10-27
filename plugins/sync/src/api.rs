//! A minimal HTTP/1.1 client for the daemon's loopback API, acting as this plugin's actor.

use serde_json::Value;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::Path;
use std::time::Duration;

/// Seconds one event poll may wait for news.
pub const POLL_WAIT: u64 = 2;

pub struct Api {
    address: String,
    token: String,
    pub actor: String,
}

/// One page of the event stream.
pub struct Page {
    pub events: Vec<Value>,
    pub next: u64,
    pub stream: String,
    pub gap: bool,
}

impl Api {
    /// `url` looks like `http://127.0.0.1:7878`.
    pub fn new(url: &str, token: &str, actor: &str) -> Result<Api, String> {
        let address = url
            .strip_prefix("http://")
            .map(|a| a.trim_end_matches('/'))
            .filter(|a| !a.is_empty() && !a.contains('/'))
            .ok_or_else(|| format!("unsupported URL `{url}`: expected http://host:port"))?;
        Ok(Api {
            address: address.to_string(),
            token: token.trim().to_string(),
            actor: actor.to_string(),
        })
    }

    /// Reads the token from `token_file`, or from the environment the daemon provides.
    pub fn connect(
        url: Option<&str>,
        token_file: Option<&Path>,
        tickets_dir: &Path,
    ) -> Result<Api, String> {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let url = url
            .map(str::to_string)
            .or_else(|| var("SIMPLETICKETS_URL"))
            .unwrap_or_else(|| "http://127.0.0.1:7878".into());
        let file = token_file
            .map(Path::to_path_buf)
            .or_else(|| var("SIMPLETICKETS_TOKEN_FILE").map(Into::into))
            .unwrap_or_else(|| tickets_dir.join(".simpletickets").join("token"));
        let token = match std::fs::read_to_string(&file) {
            Ok(t) => t,
            Err(e) => match var("SIMPLETICKETS_TOKEN") {
                Some(t) => t,
                None => {
                    return Err(format!(
                        "cannot read the token file {}: {e}",
                        file.display()
                    ))
                }
            },
        };
        let actor = var("SIMPLETICKETS_ACTOR").unwrap_or_else(|| "plugin:sync".into());
        Api::new(&url, &token, &actor)
    }

    /// Sends a request; returns the status and the parsed JSON body (null when empty).
    pub fn request(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Result<(u16, Value), String> {
        let payload = body.map(Value::to_string).unwrap_or_default();
        let mut stream = TcpStream::connect(&self.address)
            .map_err(|e| format!("cannot reach the daemon at {}: {e}", self.address))?;
        let timeout = Some(Duration::from_secs(POLL_WAIT + 25));
        let _ = stream.set_read_timeout(timeout);
        let _ = stream.set_write_timeout(timeout);
        let mut head = format!(
            "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n\
             Authorization: Bearer {}\r\nX-SimpleTickets-Actor: {}\r\n",
            self.address, self.token, self.actor
        );
        for (name, value) in headers {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
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
        let text = String::from_utf8_lossy(&raw);
        let (head, body) = text
            .split_once("\r\n\r\n")
            .ok_or("malformed HTTP reply from the daemon")?;
        let status = head
            .split(' ')
            .nth(1)
            .and_then(|s| s.parse().ok())
            .ok_or("malformed HTTP status line")?;
        Ok((status, serde_json::from_str(body).unwrap_or(Value::Null)))
    }

    fn get_ok(&self, path: &str) -> Result<Value, String> {
        match self.request("GET", path, &[], None)? {
            (200, value) => Ok(value),
            (status, value) => Err(format!("GET {path}: status {status}: {}", value["error"])),
        }
    }

    pub fn list_tickets(&self) -> Result<Vec<Value>, String> {
        match self.get_ok("/v1/tickets")? {
            Value::Array(items) => Ok(items),
            _ => Err("GET /v1/tickets: unexpected reply".into()),
        }
    }

    /// One ticket with its body; `None` when the daemon does not know the id.
    pub fn get_ticket(&self, id: &str) -> Result<Option<Value>, String> {
        let path = format!("/v1/tickets/{id}");
        match self.request("GET", &path, &[], None)? {
            (200, ticket) => Ok(Some(ticket)),
            (404, _) => Ok(None),
            (status, value) => Err(format!("GET {path}: status {status}: {}", value["error"])),
        }
    }

    /// Updates a ticket under an idempotency key; returns the updated ticket.
    pub fn patch_ticket(&self, id: &str, changes: &Value, key: &str) -> Result<Value, String> {
        let path = format!("/v1/tickets/{id}");
        match self.request("PATCH", &path, &[("Idempotency-Key", key)], Some(changes))? {
            (200, ticket) => Ok(ticket),
            (status, value) => Err(format!(
                "PATCH {path}: status {status}: {}",
                value["error"].as_str().unwrap_or("refused")
            )),
        }
    }

    /// Long-polls the event stream. `since` is `None` before the first page.
    pub fn events(&self, since: Option<(u64, &str)>, wait: u64) -> Result<Page, String> {
        let mut path = format!("/v1/events?wait={wait}");
        if let Some((n, stream)) = since {
            path.push_str(&format!("&since={n}&stream={stream}"));
        }
        let page = self.get_ok(&path)?;
        Ok(Page {
            events: page["events"].as_array().cloned().unwrap_or_default(),
            next: page["next"].as_u64().unwrap_or(0),
            stream: page["stream"].as_str().unwrap_or_default().to_string(),
            gap: page["gap"] == true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unsupported_urls() {
        assert!(Api::new("https://x", "t", "a").is_err());
        assert!(Api::new("http://x/path", "t", "a").is_err());
        assert!(Api::new("http://127.0.0.1:1/", "t", "a").is_ok());
    }
}
