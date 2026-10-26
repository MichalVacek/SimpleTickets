//! JSON-RPC 2.0 over stdio, newline-delimited, implementing the subset of MCP that
//! tool servers need: `initialize`, `ping`, `tools/list` and `tools/call`.

use crate::client::Backend;
use crate::tools;
use serde_json::{json, Value};
use std::io::{BufRead, Write};

const SUPPORTED_VERSIONS: [&str; 3] = ["2025-06-18", "2025-03-26", "2024-11-05"];
const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;

/// Reads requests line by line until the input closes.
pub fn run(
    input: impl BufRead,
    mut output: impl Write,
    backend: &dyn Backend,
) -> std::io::Result<()> {
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(reply) = handle_line(&line, backend) {
            writeln!(output, "{reply}")?;
            output.flush()?;
        }
    }
    Ok(())
}

/// Handles one message; `None` for notifications, which get no reply.
pub fn handle_line(line: &str, backend: &dyn Backend) -> Option<Value> {
    let message: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(e) => {
            return Some(failure(
                Value::Null,
                PARSE_ERROR,
                &format!("parse error: {e}"),
            ))
        }
    };
    let Some(method) = message["method"].as_str() else {
        // A response or malformed message; only malformed ones with an id get an error.
        return message
            .get("id")
            .filter(|_| message.get("result").is_none() && message.get("error").is_none())
            .map(|id| failure(id.clone(), INVALID_REQUEST, "missing method"));
    };
    let id = message.get("id").cloned()?;
    let params = &message["params"];
    Some(match dispatch(method, params, backend) {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err((code, text)) => failure(id, code, &text),
    })
}

fn failure(id: Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

fn dispatch(method: &str, params: &Value, backend: &dyn Backend) -> Result<Value, (i64, String)> {
    match method {
        "initialize" => {
            let asked = params["protocolVersion"].as_str().unwrap_or("");
            let version = SUPPORTED_VERSIONS
                .iter()
                .find(|v| **v == asked)
                .unwrap_or(&SUPPORTED_VERSIONS[0]);
            Ok(json!({
                "protocolVersion": version,
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "simpletickets-mcp", "version": env!("CARGO_PKG_VERSION")}
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools::definitions()})),
        "tools/call" => {
            let name = params["name"]
                .as_str()
                .ok_or((INVALID_PARAMS, "missing tool name".to_string()))?;
            Ok(match tools::call(backend, name, &params["arguments"]) {
                Ok(value) => tool_result(&value, false),
                Err(message) => tool_result(&Value::String(message), true),
            })
        }
        other => Err((METHOD_NOT_FOUND, format!("method not found: {other}"))),
    }
}

fn tool_result(value: &Value, is_error: bool) -> Value {
    let text = match value {
        Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    };
    json!({"content": [{"type": "text", "text": text}], "isError": is_error})
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Offline;
    impl Backend for Offline {
        fn request(&self, _: &str, _: &str, _: Option<&Value>) -> Result<Value, String> {
            Err("cannot reach the daemon".into())
        }
    }

    fn ask(line: &str) -> Option<Value> {
        handle_line(line, &Offline)
    }

    #[test]
    fn initialize_negotiates_the_version() {
        let r = ask(r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05"}}"#)
            .unwrap();
        assert_eq!(r["id"], 1);
        assert_eq!(r["result"]["protocolVersion"], "2024-11-05");
        let r = ask(
            r#"{"jsonrpc":"2.0","id":2,"method":"initialize","params":{"protocolVersion":"1999"}}"#,
        )
        .unwrap();
        assert_eq!(r["result"]["protocolVersion"], "2025-06-18");
        assert!(r["result"]["capabilities"]["tools"].is_object());
    }

    #[test]
    fn notifications_get_no_reply() {
        assert!(ask(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#).is_none());
        assert!(ask(r#"{"jsonrpc":"2.0","id":3,"result":{}}"#).is_none());
    }

    #[test]
    fn protocol_errors() {
        let r = ask("{not json").unwrap();
        assert_eq!(r["error"]["code"], PARSE_ERROR);
        assert!(r["id"].is_null());
        let r = ask(r#"{"jsonrpc":"2.0","id":4,"method":"nope"}"#).unwrap();
        assert_eq!(r["error"]["code"], METHOD_NOT_FOUND);
        let r = ask(r#"{"jsonrpc":"2.0","id":5}"#).unwrap();
        assert_eq!(r["error"]["code"], INVALID_REQUEST);
        let r = ask(r#"{"jsonrpc":"2.0","id":6,"method":"tools/call","params":{}}"#).unwrap();
        assert_eq!(r["error"]["code"], INVALID_PARAMS);
    }

    #[test]
    fn tool_failures_are_results_not_protocol_errors() {
        let r = ask(r#"{"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"show_ticket","arguments":{"id":"ST-1"}}}"#)
            .unwrap();
        assert!(r.get("error").is_none());
        assert_eq!(r["result"]["isError"], true);
        assert!(r["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("cannot reach"));
    }

    #[test]
    fn run_answers_each_line_and_skips_blanks() {
        let input = b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"ping\"}\n\n{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}\n";
        let mut out = Vec::new();
        run(&input[..], &mut out, &Offline).unwrap();
        let lines: Vec<&str> = std::str::from_utf8(&out).unwrap().lines().collect();
        assert_eq!(lines.len(), 2);
        let list: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 6);
    }
}
