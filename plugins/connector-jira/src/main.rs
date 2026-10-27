//! `simpletickets-connector-jira`: started by the sync plugin, speaks the connector
//! protocol on standard input and output.

use simpletickets_connector_jira::JiraConnector;
use simpletickets_connector_protocol::serve;
use std::io::{stdin, stdout};
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut connector = JiraConnector::default();
    match serve(&mut connector, stdin().lock(), &mut stdout().lock()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("simpletickets-connector-jira: {e}");
            ExitCode::FAILURE
        }
    }
}
