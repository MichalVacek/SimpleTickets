//! `simpletickets-follow-up`: run by the daemon as a service plugin.

use simpletickets_follow_up::{run, Client, POLL_WAIT};
use std::process::ExitCode;
use std::sync::atomic::AtomicBool;

fn main() -> ExitCode {
    let client = match Client::from_env() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("simpletickets-follow-up: {e}");
            return ExitCode::from(2);
        }
    };
    println!("following events as {}", client.actor);
    run(&client, &AtomicBool::new(false), POLL_WAIT);
    ExitCode::SUCCESS
}
