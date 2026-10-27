//! The Jira REST client: basic authentication, JSON in and out, and retries with backoff.
//!
//! The authorization header is built once and only ever handed to the HTTP library; it is
//! never printed, logged or put into an error message.

use serde_json::Value;
use simpletickets_connector_protocol::{ErrorCode, ErrorInfo};
use std::time::Duration;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Retry policy for rate limits, server errors and network failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retry {
    /// Total tries per request, the first included.
    pub attempts: u32,
    pub base_ms: u64,
    pub max_ms: u64,
}

impl Default for Retry {
    fn default() -> Retry {
        Retry {
            attempts: 5,
            base_ms: 500,
            max_ms: 30_000,
        }
    }
}

impl Retry {
    /// The wait before try number `attempt` (1 for the first retry).
    fn delay(&self, attempt: u32, retry_after_seconds: Option<u64>) -> Duration {
        let exponential = self
            .base_ms
            .saturating_mul(1 << attempt.saturating_sub(1).min(16));
        let wanted = retry_after_seconds.map_or(exponential, |s| s.saturating_mul(1000));
        Duration::from_millis(wanted.min(self.max_ms))
    }
}

pub struct Reply {
    pub status: u16,
    pub body: Value,
}

pub struct Client {
    base: String,
    authorization: String,
    retry: Retry,
}

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64(input: &[u8]) -> String {
    let mut out = String::new();
    for chunk in input.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(ALPHABET[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// A readable summary of Jira's error body: `errorMessages` and per-field `errors`.
fn jira_message(body: &str) -> String {
    let value: Value = serde_json::from_str(body).unwrap_or(Value::Null);
    let mut parts: Vec<String> = value["errorMessages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m.as_str().map(String::from))
        .collect();
    if let Some(errors) = value["errors"].as_object() {
        parts.extend(
            errors
                .iter()
                .filter_map(|(k, v)| v.as_str().map(|m| format!("{k}: {m}"))),
        );
    }
    parts.join("; ")
}

/// Maps a non-success status to the protocol's error model.
pub fn classify(status: u16, body: &str) -> ErrorInfo {
    let detail = jira_message(body);
    let suffix = if detail.is_empty() {
        String::new()
    } else {
        format!(": {detail}")
    };
    match status {
        401 | 403 => ErrorInfo::new(
            ErrorCode::Auth,
            format!("Jira refused the credentials or the permission (HTTP {status}){suffix}"),
        ),
        404 => ErrorInfo::new(
            ErrorCode::NotFound,
            format!("Jira does not know that item (HTTP 404){suffix}"),
        ),
        _ => ErrorInfo::new(
            ErrorCode::Rejected,
            format!("Jira refused the request (HTTP {status}){suffix}"),
        ),
    }
}

impl Client {
    pub fn new(base: &str, email: &str, token: &str, retry: Retry) -> Client {
        Client {
            base: base.trim_end_matches('/').to_string(),
            authorization: format!("Basic {}", base64(format!("{email}:{token}").as_bytes())),
            retry,
        }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// Sends a request. A request that is `repeatable` is retried after network failures
    /// and server errors; any request is retried after a rate limit, which Jira answers
    /// before doing anything. Other 4xx answers are returned as errors at once.
    pub fn send(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
        repeatable: bool,
    ) -> Result<Reply, ErrorInfo> {
        let url = format!("{}{}", self.base, path);
        let payload = body.map(Value::to_string);
        let mut last = ErrorInfo::new(ErrorCode::Unavailable, "no attempt was made");
        let mut wait = Duration::ZERO;
        for attempt in 0..self.retry.attempts.max(1) {
            if attempt > 0 {
                std::thread::sleep(wait);
            }
            let next = attempt + 1;
            match self.once(method, &url, payload.as_deref()) {
                Ok((status, _, text)) if (200..300).contains(&status) => {
                    return Ok(Reply {
                        status,
                        body: serde_json::from_str(&text).unwrap_or(Value::Null),
                    })
                }
                Ok((429, retry_after, _)) => {
                    wait = self.retry.delay(next, retry_after);
                    last = ErrorInfo::new(
                        ErrorCode::RateLimited,
                        "Jira is rate limiting this client (HTTP 429)",
                    );
                    if let Some(s) = retry_after {
                        last = last.with_retry_after(s.saturating_mul(1000));
                    }
                }
                Ok((status, _, _)) if status >= 500 => {
                    last = ErrorInfo::new(
                        ErrorCode::Unavailable,
                        format!("Jira is unavailable (HTTP {status})"),
                    );
                    if !repeatable {
                        return Err(last);
                    }
                    wait = self.retry.delay(next, None);
                }
                Ok((status, _, text)) => return Err(classify(status, &text)),
                Err(e) => {
                    last =
                        ErrorInfo::new(ErrorCode::Unavailable, format!("cannot reach Jira: {e}"));
                    if !repeatable {
                        return Err(last);
                    }
                    wait = self.retry.delay(next, None);
                }
            }
        }
        Err(last)
    }

    /// One try: status, `Retry-After` in seconds, body text.
    fn once(
        &self,
        method: &str,
        url: &str,
        body: Option<&str>,
    ) -> Result<(u16, Option<u64>, String), String> {
        macro_rules! prepared {
            ($request:expr) => {
                $request
                    .config()
                    .timeout_global(Some(REQUEST_TIMEOUT))
                    .http_status_as_error(false)
                    .max_redirects(0)
                    .build()
                    .header("Authorization", &self.authorization)
                    .header("Accept", "application/json")
            };
        }
        let sent = match method {
            "GET" => prepared!(ureq::get(url)).call(),
            "DELETE" => prepared!(ureq::delete(url)).call(),
            "POST" => prepared!(ureq::post(url))
                .header("Content-Type", "application/json")
                .send(body.unwrap_or("{}")),
            "PUT" => prepared!(ureq::put(url))
                .header("Content-Type", "application/json")
                .send(body.unwrap_or("{}")),
            other => return Err(format!("unsupported method {other}")),
        };
        let mut response = sent.map_err(|e| e.to_string())?;
        let status = response.status().as_u16();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.trim().parse::<u64>().ok());
        let text = response.body_mut().read_to_string().unwrap_or_default();
        Ok((status, retry_after, text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_the_reference_vectors() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foob"), "Zm9vYg==");
        assert_eq!(
            base64(b"user@example.com:token"),
            "dXNlckBleGFtcGxlLmNvbTp0b2tlbg=="
        );
    }

    #[test]
    fn delays_double_cap_and_follow_retry_after() {
        let retry = Retry {
            attempts: 5,
            base_ms: 100,
            max_ms: 1000,
        };
        assert_eq!(retry.delay(1, None), Duration::from_millis(100));
        assert_eq!(retry.delay(3, None), Duration::from_millis(400));
        assert_eq!(retry.delay(9, None), Duration::from_millis(1000));
        assert_eq!(retry.delay(1, Some(1)), Duration::from_millis(1000));
        assert_eq!(retry.delay(1, Some(60)), Duration::from_millis(1000));
    }

    #[test]
    fn statuses_map_to_the_protocol_errors() {
        let body = r#"{"errorMessages":["Issue does not exist"],"errors":{"priority":"bad"}}"#;
        let auth = classify(401, "");
        assert_eq!(auth.code, ErrorCode::Auth);
        assert!(!auth.retryable);
        assert_eq!(classify(404, body).code, ErrorCode::NotFound);
        let rejected = classify(400, body);
        assert_eq!(rejected.code, ErrorCode::Rejected);
        assert!(rejected.message.contains("Issue does not exist"));
        assert!(rejected.message.contains("priority: bad"));
    }
}
