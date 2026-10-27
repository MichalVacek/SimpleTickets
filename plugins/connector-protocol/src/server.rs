//! The connector side: read request lines, answer each with one response line.

use crate::{parse_request, response_line, ErrorCode, ErrorInfo, PROTOCOL_VERSION};
use serde_json::{json, Value};
use std::io::{BufRead, Write};

/// What a connector binary implements. `describe` and `shutdown` are answered by the
/// server loop itself.
pub trait Connector {
    /// `name`, `version` and `capabilities` (operation names beyond the mandatory
    /// `configure` and `push`: `close`, `fetch`, `check`).
    fn describe(&self) -> Value;

    /// Runs `op` with `params`.
    fn handle(&mut self, op: &str, params: &Value) -> Result<Value, ErrorInfo>;
}

/// Answers one request line. Returns the response line and whether the connector should
/// stop afterwards (`shutdown`).
pub fn dispatch<C: Connector>(connector: &mut C, line: &str) -> (String, bool) {
    let request = match parse_request(line) {
        Ok(r) => r,
        Err((id, error)) => return (response_line(id.unwrap_or(0), &Err(error)), false),
    };
    match request.op.as_str() {
        "describe" => {
            let mut description = connector.describe();
            description["protocol"] = json!([PROTOCOL_VERSION]);
            (response_line(request.id, &Ok(description)), false)
        }
        "shutdown" => (response_line(request.id, &Ok(json!({}))), true),
        op => {
            let outcome = connector.handle(op, &request.params);
            (response_line(request.id, &outcome), false)
        }
    }
}

/// Serves requests from `input` until end of input or `shutdown`. Blank lines are skipped.
pub fn serve<C: Connector, R: BufRead, W: Write>(
    connector: &mut C,
    input: R,
    output: &mut W,
) -> std::io::Result<()> {
    for line in input.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let (answer, stop) = dispatch(connector, &line);
        writeln!(output, "{answer}")?;
        output.flush()?;
        if stop {
            break;
        }
    }
    Ok(())
}

/// The answer to an operation the connector does not have.
pub fn unsupported(op: &str) -> ErrorInfo {
    ErrorInfo::new(ErrorCode::Unsupported, format!("unknown operation `{op}`"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_response;

    struct Echo;

    impl Connector for Echo {
        fn describe(&self) -> Value {
            json!({"name": "echo", "version": "0", "capabilities": []})
        }

        fn handle(&mut self, op: &str, params: &Value) -> Result<Value, ErrorInfo> {
            match op {
                "echo" => Ok(params.clone()),
                "fail" => Err(ErrorInfo::new(ErrorCode::Rejected, "no")),
                _ => Err(unsupported(op)),
            }
        }
    }

    fn run(input: &str) -> Vec<(u64, Result<Value, ErrorInfo>)> {
        let mut output = Vec::new();
        serve(&mut Echo, input.as_bytes(), &mut output).unwrap();
        String::from_utf8(output)
            .unwrap()
            .lines()
            .map(|l| parse_response(l).unwrap())
            .collect()
    }

    #[test]
    fn answers_each_request_in_order() {
        let input = [
            r#"{"v":1,"id":1,"op":"describe"}"#,
            "",
            r#"{"v":1,"id":2,"op":"echo","params":{"a":1}}"#,
            r#"{"v":1,"id":3,"op":"fail"}"#,
            r#"{"v":1,"id":4,"op":"nope"}"#,
            "garbage",
        ]
        .join("\n");
        let answers = run(&input);
        assert_eq!(answers.len(), 5);
        let describe = answers[0].1.as_ref().unwrap();
        assert_eq!(describe["name"], "echo");
        assert_eq!(describe["protocol"], json!([1]));
        assert_eq!(answers[1].1, Ok(json!({"a": 1})));
        assert_eq!(answers[2].1.as_ref().unwrap_err().code, ErrorCode::Rejected);
        assert_eq!(
            answers[3].1.as_ref().unwrap_err().code,
            ErrorCode::Unsupported
        );
        assert_eq!(answers[4].0, 0);
        assert_eq!(
            answers[4].1.as_ref().unwrap_err().code,
            ErrorCode::InvalidRequest
        );
    }

    #[test]
    fn shutdown_ends_the_loop() {
        let answers =
            run("{\"v\":1,\"id\":1,\"op\":\"shutdown\"}\n{\"v\":1,\"id\":2,\"op\":\"echo\"}\n");
        assert_eq!(answers.len(), 1);
        assert_eq!(answers[0].1, Ok(json!({})));
    }
}
