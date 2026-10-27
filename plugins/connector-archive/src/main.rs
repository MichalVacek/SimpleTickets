//! `simpletickets-connector-archive`: started by the sync plugin, speaks the connector
//! protocol on standard input and output.

use simpletickets_connector_archive::ArchiveConnector;
use simpletickets_connector_protocol::serve;
use std::io::{stdin, stdout};
use std::process::ExitCode;

fn main() -> ExitCode {
    let mut connector = ArchiveConnector::default();
    match serve(&mut connector, stdin().lock(), &mut stdout().lock()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("simpletickets-connector-archive: {e}");
            ExitCode::FAILURE
        }
    }
}
