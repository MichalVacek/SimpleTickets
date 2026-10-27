//! Example workflow plugin: when a ticket's status changes to a `done` category status,
//! create a follow-up ticket linked to it.
//!
//! It follows the daemon's event stream by long polling and writes through the API under
//! its own actor name, with an idempotency key per ticket so that a repeated or replayed
//! event never creates a second follow-up.

use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Title prefix of the tickets this plugin creates; finishing one does not start a chain.
pub const PREFIX: &str = "Follow up: ";
/// Seconds one event poll waits for news.
pub const POLL_WAIT: u64 = 20;
const RETRIES: u32 = 3;

/// A minimal HTTP/1.1 client for the daemon's loopback API.
pub struct Client {
    address: String,
    token: String,
    pub actor: String,
}

impl Client {
    /// `url` looks like `http://127.0.0.1:7878`.
    pub fn new(url: &str, token: &str, actor: &str) -> Result<Client, String> {
        let address = url
            .strip_prefix("http://")
            .map(|a| a.trim_end_matches('/'))
            .filter(|a| !a.is_empty() && !a.contains('/'))
            .ok_or_else(|| format!("unsupported URL `{url}`: expected http://host:port"))?;
        Ok(Client {
            address: address.to_string(),
            token: token.trim().to_string(),
            actor: actor.to_string(),
        })
    }

    /// Reads the connection settings the daemon puts in a plugin's environment.
    pub fn from_env() -> Result<Client, String> {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let url = var("SIMPLETICKETS_URL").unwrap_or_else(|| "http://127.0.0.1:7878".into());
        let token = match (var("SIMPLETICKETS_TOKEN_FILE"), var("SIMPLETICKETS_TOKEN")) {
            (Some(file), _) => std::fs::read_to_string(&file)
                .map_err(|e| format!("cannot read the token file {file}: {e}"))?,
            (None, Some(token)) => token,
            (None, None) => return Err("SIMPLETICKETS_TOKEN_FILE is not set".into()),
        };
        let actor = var("SIMPLETICKETS_ACTOR").unwrap_or_else(|| "plugin:follow-up".into());
        Client::new(&url, &token, &actor)
    }

    /// Sends a request as this plugin's actor; `headers` are added. Returns the status and
    /// the parsed JSON body (null when there is none).
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
        let timeout = Some(Duration::from_secs(POLL_WAIT + 20));
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

    pub fn get(&self, path: &str) -> Result<(u16, Value), String> {
        self.request("GET", path, &[], None)
    }
}

/// What to do about one event: the follow-up to create, as `(source ticket id, body)`.
pub fn follow_up_for(event: &Value) -> Option<(String, Value)> {
    if event["type"] != "ticket.status_changed" || event["data"]["to_category"] != "done" {
        return None;
    }
    let id = event["data"]["id"].as_str()?;
    let title = event["data"]["title"].as_str()?;
    if title.starts_with(PREFIX) {
        return None;
    }
    let body = json!({
        "title": format!("{PREFIX}{title}"),
        "related": [id],
        "tags": ["follow-up"],
    });
    Some((id.to_string(), body))
}

/// Creates the follow-up for `id`. The key names the intended effect (one follow-up per
/// ticket), so a retry, a replayed event or a restart cannot create a second one.
fn create_follow_up(client: &Client, id: &str, body: &Value) -> Result<String, String> {
    let key = format!("follow-up:{id}");
    let mut last = String::new();
    for attempt in 0..RETRIES {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(250 * u64::from(attempt)));
        }
        match client.request(
            "POST",
            "/v1/tickets",
            &[("Idempotency-Key", &key)],
            Some(body),
        ) {
            Ok((201, ticket)) => {
                return Ok(format!(
                    "created {} for {id}",
                    ticket["id"].as_str().unwrap_or("?")
                ))
            }
            // The key was used for a different request: this ticket was handled before.
            Ok((422, _)) => return Ok(format!("{id} already has its follow-up")),
            Ok((status, reply)) if status < 500 => {
                return Err(format!("{id}: the daemon refused ({status}): {reply}"))
            }
            Ok((status, _)) => last = format!("status {status}"),
            Err(e) => last = e,
        }
    }
    Err(format!("{id}: giving up after {RETRIES} attempts: {last}"))
}

/// Follows the event stream until `stop` is set, creating follow-ups. The retained backlog
/// is processed too, which is safe because the idempotency keys absorb repeats.
pub fn run(client: &Client, stop: &AtomicBool, wait: u64) {
    let mut since: Option<u64> = None;
    let mut stream = String::new();
    while !stop.load(Ordering::Relaxed) {
        let mut path = format!("/v1/events?wait={wait}");
        if let Some(n) = since {
            path.push_str(&format!("&since={n}&stream={stream}"));
        }
        let page = match client.get(&path) {
            Ok((200, page)) => page,
            Ok((status, _)) => {
                eprintln!("events: unexpected status {status}");
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
            Err(e) => {
                eprintln!("events: {e}");
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }
        };
        for event in page["events"].as_array().into_iter().flatten() {
            if let Some((id, body)) = follow_up_for(event) {
                match create_follow_up(client, &id, &body) {
                    Ok(done) => println!("{done}"),
                    Err(e) => eprintln!("{e}"),
                }
            }
        }
        if page["gap"] == true {
            // Events may have been missed; this plugin only reacts to new changes, so it
            // continues from the newest one.
            eprintln!(
                "events: gap ({}); continuing from the latest",
                page["gap_reason"]
            );
        }
        since = page["next"].as_u64();
        stream = page["stream"].as_str().unwrap_or_default().to_string();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn status_changed(to_category: &str, title: &str) -> Value {
        json!({"seq": 3, "type": "ticket.status_changed",
               "data": {"id": "ST-4", "title": title, "to_category": to_category}})
    }

    #[test]
    fn only_finished_tickets_get_a_follow_up() {
        let (id, body) = follow_up_for(&status_changed("done", "Ship it")).unwrap();
        assert_eq!(id, "ST-4");
        assert_eq!(body["title"], "Follow up: Ship it");
        assert_eq!(body["related"][0], "ST-4");
        assert!(follow_up_for(&status_changed("in-progress", "Ship it")).is_none());
        assert!(follow_up_for(&status_changed("cancelled", "Ship it")).is_none());
        let other = json!({"type": "ticket.updated", "data": {"to_category": "done"}});
        assert!(follow_up_for(&other).is_none());
    }

    #[test]
    fn a_follow_up_does_not_start_a_chain() {
        assert!(follow_up_for(&status_changed("done", "Follow up: Ship it")).is_none());
    }

    #[test]
    fn rejects_unsupported_urls() {
        assert!(Client::new("https://x", "t", "a").is_err());
        assert!(Client::new("http://x/path", "t", "a").is_err());
        assert!(Client::new("http://127.0.0.1:1/", "t", "a").is_ok());
    }
}
