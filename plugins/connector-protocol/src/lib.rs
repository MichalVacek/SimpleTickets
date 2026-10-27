//! The connector protocol: one JSON object per line over a connector's standard input and
//! output, a request answered by exactly one response. See `docs/sync.md` for the contract.
//!
//! This crate holds the message shapes, the error model, a server loop for connector
//! binaries and a client that runs a connector as a child process.

mod client;
mod server;

pub use client::{Process, Spawn, Transport};
pub use server::{dispatch, serve, unsupported, Connector};

use serde_json::{json, Value};

/// The protocol version spoken by this crate. It appears as `v` in every message.
pub const PROTOCOL_VERSION: u64 = 1;

/// Why an operation failed. The code decides whether the sync plugin retries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    /// The request is malformed or missing a parameter.
    InvalidRequest,
    /// The connector does not implement the operation or the protocol version.
    Unsupported,
    /// Credentials are missing or refused; retrying cannot help.
    Auth,
    /// The remote item named by `ref` does not exist.
    NotFound,
    /// The remote asked to slow down; retry later.
    RateLimited,
    /// The remote or the network is unavailable; retry later.
    Unavailable,
    /// The remote refused the content; retrying the same push cannot help.
    Rejected,
    /// A bug or an unexpected state inside the connector.
    Internal,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            ErrorCode::InvalidRequest => "invalid_request",
            ErrorCode::Unsupported => "unsupported",
            ErrorCode::Auth => "auth",
            ErrorCode::NotFound => "not_found",
            ErrorCode::RateLimited => "rate_limited",
            ErrorCode::Unavailable => "unavailable",
            ErrorCode::Rejected => "rejected",
            ErrorCode::Internal => "internal",
        }
    }

    pub fn parse(text: &str) -> ErrorCode {
        match text {
            "invalid_request" => ErrorCode::InvalidRequest,
            "unsupported" => ErrorCode::Unsupported,
            "auth" => ErrorCode::Auth,
            "not_found" => ErrorCode::NotFound,
            "rate_limited" => ErrorCode::RateLimited,
            "unavailable" => ErrorCode::Unavailable,
            "rejected" => ErrorCode::Rejected,
            _ => ErrorCode::Internal,
        }
    }

    /// Whether the same request may succeed later.
    pub fn is_retryable(self) -> bool {
        matches!(self, ErrorCode::RateLimited | ErrorCode::Unavailable)
    }
}

/// An error answer. Messages are for people and never contain credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ErrorInfo {
    pub code: ErrorCode,
    pub message: String,
    pub retryable: bool,
    /// How long the remote asked to wait, when it said so.
    pub retry_after_ms: Option<u64>,
}

impl ErrorInfo {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> ErrorInfo {
        ErrorInfo {
            code,
            message: message.into(),
            retryable: code.is_retryable(),
            retry_after_ms: None,
        }
    }

    pub fn with_retry_after(mut self, ms: u64) -> ErrorInfo {
        self.retry_after_ms = Some(ms);
        self
    }

    pub fn invalid(message: impl Into<String>) -> ErrorInfo {
        ErrorInfo::new(ErrorCode::InvalidRequest, message)
    }

    pub fn to_json(&self) -> Value {
        json!({
            "code": self.code.as_str(),
            "message": self.message,
            "retryable": self.retryable,
            "retry_after_ms": self.retry_after_ms,
        })
    }

    pub fn from_json(value: &Value) -> ErrorInfo {
        let code = ErrorCode::parse(value["code"].as_str().unwrap_or(""));
        ErrorInfo {
            code,
            message: value["message"].as_str().unwrap_or("unknown error").into(),
            retryable: value["retryable"].as_bool().unwrap_or(code.is_retryable()),
            retry_after_ms: value["retry_after_ms"].as_u64(),
        }
    }
}

impl std::fmt::Display for ErrorInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.message, self.code.as_str())
    }
}

/// A parsed request line.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub id: u64,
    pub op: String,
    pub params: Value,
}

/// Encodes a request as one line (no trailing newline).
pub fn request_line(id: u64, op: &str, params: &Value) -> String {
    json!({"v": PROTOCOL_VERSION, "id": id, "op": op, "params": params}).to_string()
}

/// Parses a request line. The error carries the request id when it could be read, so the
/// caller can still answer it.
pub fn parse_request(line: &str) -> Result<Request, (Option<u64>, ErrorInfo)> {
    let value: Value = serde_json::from_str(line)
        .map_err(|e| (None, ErrorInfo::invalid(format!("not valid JSON: {e}"))))?;
    let id = value["id"].as_u64();
    let fail = |e: ErrorInfo| (id, e);
    let Some(id_value) = id else {
        return Err(fail(ErrorInfo::invalid("`id` must be an unsigned integer")));
    };
    if value["v"].as_u64() != Some(PROTOCOL_VERSION) {
        return Err(fail(ErrorInfo::new(
            ErrorCode::Unsupported,
            format!(
                "protocol version {} is not supported; this connector speaks {PROTOCOL_VERSION}",
                value["v"]
            ),
        )));
    }
    let op = value["op"]
        .as_str()
        .ok_or_else(|| fail(ErrorInfo::invalid("`op` must be a string")))?;
    let params = match &value["params"] {
        Value::Null => json!({}),
        p @ Value::Object(_) => p.clone(),
        _ => return Err(fail(ErrorInfo::invalid("`params` must be an object"))),
    };
    Ok(Request {
        id: id_value,
        op: op.to_string(),
        params,
    })
}

/// Encodes a response line for `id`.
pub fn response_line(id: u64, outcome: &Result<Value, ErrorInfo>) -> String {
    match outcome {
        Ok(result) => json!({"v": PROTOCOL_VERSION, "id": id, "ok": true, "result": result}),
        Err(e) => json!({"v": PROTOCOL_VERSION, "id": id, "ok": false, "error": e.to_json()}),
    }
    .to_string()
}

/// Parses a response line into the request id and its outcome.
pub fn parse_response(line: &str) -> Result<(u64, Result<Value, ErrorInfo>), String> {
    let value: Value = serde_json::from_str(line).map_err(|e| format!("not valid JSON: {e}"))?;
    let id = value["id"].as_u64().ok_or("response without an id")?;
    match value["ok"].as_bool() {
        Some(true) => Ok((id, Ok(value["result"].clone()))),
        Some(false) => Ok((id, Err(ErrorInfo::from_json(&value["error"])))),
        None => Err("response without `ok`".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips() {
        let line = request_line(7, "push", &json!({"ref": null}));
        assert!(!line.contains('\n'));
        let request = parse_request(&line).unwrap();
        assert_eq!(request.id, 7);
        assert_eq!(request.op, "push");
        assert_eq!(request.params, json!({"ref": null}));
    }

    #[test]
    fn missing_params_become_an_empty_object() {
        let request = parse_request(r#"{"v":1,"id":1,"op":"describe"}"#).unwrap();
        assert_eq!(request.params, json!({}));
    }

    #[test]
    fn malformed_requests_are_answered_with_their_id_when_known() {
        let (id, e) = parse_request("not json").unwrap_err();
        assert_eq!((id, e.code), (None, ErrorCode::InvalidRequest));
        let (id, e) = parse_request(r#"{"v":2,"id":5,"op":"describe"}"#).unwrap_err();
        assert_eq!((id, e.code), (Some(5), ErrorCode::Unsupported));
        let (id, e) = parse_request(r#"{"v":1,"id":6,"op":3}"#).unwrap_err();
        assert_eq!((id, e.code), (Some(6), ErrorCode::InvalidRequest));
        let (_, e) = parse_request(r#"{"v":1,"id":6,"op":"x","params":[]}"#).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidRequest);
        let (id, _) = parse_request(r#"{"v":1,"op":"x"}"#).unwrap_err();
        assert_eq!(id, None);
    }

    #[test]
    fn responses_round_trip_including_errors() {
        let ok = response_line(3, &Ok(json!({"ref": "K-1"})));
        assert_eq!(parse_response(&ok).unwrap(), (3, Ok(json!({"ref": "K-1"}))));
        let error = ErrorInfo::new(ErrorCode::RateLimited, "slow down").with_retry_after(1500);
        let line = response_line(4, &Err(error.clone()));
        assert_eq!(parse_response(&line).unwrap(), (4, Err(error)));
        assert!(parse_response("{}").is_err());
        assert!(parse_response(r#"{"id":1}"#).is_err());
    }

    #[test]
    fn only_transient_codes_are_retryable() {
        for code in [ErrorCode::RateLimited, ErrorCode::Unavailable] {
            assert!(ErrorInfo::new(code, "x").retryable);
        }
        for code in [
            ErrorCode::Auth,
            ErrorCode::Rejected,
            ErrorCode::NotFound,
            ErrorCode::InvalidRequest,
            ErrorCode::Unsupported,
            ErrorCode::Internal,
        ] {
            assert!(!ErrorInfo::new(code, "x").retryable);
            assert_eq!(ErrorCode::parse(code.as_str()), code);
        }
        assert_eq!(ErrorCode::parse("something new"), ErrorCode::Internal);
    }
}
