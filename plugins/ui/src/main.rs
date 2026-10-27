//! `simpletickets-ui`: serves the kanban board on 127.0.0.1 and relays to the daemon.

use simpletickets_ui::client::Daemon;
use simpletickets_ui::server;
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "simpletickets-ui [--dir DIR] [--port N] [--url URL] [--token-file PATH]

Serves a kanban board for the SimpleTickets daemon on 127.0.0.1.

  --dir DIR          tickets folder; its .simpletickets/token is used
                     (default: $SIMPLETICKETS_DIR or the current folder)
  --port N           port of the board (default: 7979)
  --url URL          daemon address (default: $SIMPLETICKETS_URL or http://127.0.0.1:7878)
  --token-file PATH  file holding the daemon's API token (overrides --dir)";

struct Args {
    dir: Option<PathBuf>,
    port: u16,
    url: Option<String>,
    token_file: Option<PathBuf>,
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut args = Args {
        dir: None,
        port: 7979,
        url: None,
        token_file: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = |name: &str| it.next().ok_or(format!("{name} needs a value"));
        match flag.as_str() {
            "-h" | "--help" => return Ok(None),
            "--dir" => args.dir = Some(value("--dir")?.into()),
            "--port" => {
                args.port = value("--port")?
                    .parse()
                    .map_err(|_| "--port needs a number from 0 to 65535".to_string())?
            }
            "--url" => args.url = Some(value("--url")?),
            "--token-file" => args.token_file = Some(value("--token-file")?.into()),
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    Ok(Some(args))
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn run(args: Args) -> Result<(), String> {
    let url = args
        .url
        .or_else(|| env("SIMPLETICKETS_URL"))
        .unwrap_or_else(|| "http://127.0.0.1:7878".into());
    let token_file = args.token_file.unwrap_or_else(|| {
        let dir = args
            .dir
            .or_else(|| env("SIMPLETICKETS_DIR").map(PathBuf::from))
            .unwrap_or_else(|| PathBuf::from("."));
        dir.join(".simpletickets").join("token")
    });
    let daemon = Daemon::new(&url, token_file)?;
    let running = server::start(args.port, daemon)?;
    println!("SimpleTickets board: http://{}/", running.addr);
    running.wait();
    Ok(())
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(Some(a)) => a,
        Ok(None) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("simpletickets-ui: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("simpletickets-ui: {e}");
            ExitCode::FAILURE
        }
    }
}
