//! `simpletickets-sync`: run by the daemon as a service plugin (`run`), or by hand for
//! `status`, `dry-run`, `push-now` and `list-conflicts`.

fn main() -> std::process::ExitCode {
    simpletickets_sync::cli::main(std::env::args().skip(1).collect())
}
