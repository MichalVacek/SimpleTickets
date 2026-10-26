//! `simpletickets-mcp`: speaks MCP on stdin/stdout and forwards to a running daemon.

use simpletickets_mcp::client::HttpBackend;
use simpletickets_mcp::server;
use std::io::{self, BufReader};
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "simpletickets-mcp [--url URL] [--token-file PATH | --token TOKEN] [--dir DIR]

Speaks MCP over stdio and forwards tool calls to the SimpleTickets daemon.

  --url URL          daemon address (default: $SIMPLETICKETS_URL or http://127.0.0.1:7878)
  --token-file PATH  file holding the API token
  --token TOKEN      the API token (also $SIMPLETICKETS_TOKEN)
  --dir DIR          tickets folder; its .simpletickets/token is used when no token is given
                     (default: $SIMPLETICKETS_DIR or the current folder)";

#[derive(Default)]
struct Args {
    url: Option<String>,
    token: Option<String>,
    token_file: Option<PathBuf>,
    dir: Option<PathBuf>,
}

fn parse_args() -> Result<Option<Args>, String> {
    let mut args = Args::default();
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = |name: &str| it.next().ok_or(format!("{name} needs a value"));
        match flag.as_str() {
            "-h" | "--help" => return Ok(None),
            "--url" => args.url = Some(value("--url")?),
            "--token" => args.token = Some(value("--token")?),
            "--token-file" => args.token_file = Some(value("--token-file")?.into()),
            "--dir" => args.dir = Some(value("--dir")?.into()),
            other => return Err(format!("unknown argument `{other}`")),
        }
    }
    Ok(Some(args))
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn build_backend(args: Args) -> Result<HttpBackend, String> {
    let url = args
        .url
        .or_else(|| env("SIMPLETICKETS_URL"))
        .unwrap_or_else(|| "http://127.0.0.1:7878".into());
    let token = match args.token.or_else(|| env("SIMPLETICKETS_TOKEN")) {
        Some(t) => t,
        None => {
            let file = args.token_file.unwrap_or_else(|| {
                let dir = args
                    .dir
                    .or_else(|| env("SIMPLETICKETS_DIR").map(PathBuf::from))
                    .unwrap_or_else(|| PathBuf::from("."));
                dir.join(".simpletickets").join("token")
            });
            std::fs::read_to_string(&file)
                .map_err(|e| format!("cannot read token file {}: {e}", file.display()))?
        }
    };
    HttpBackend::new(&url, &token)
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(Some(a)) => a,
        Ok(None) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("simpletickets-mcp: {e}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let backend = match build_backend(args) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("simpletickets-mcp: {e}");
            return ExitCode::from(2);
        }
    };
    let stdin = BufReader::new(io::stdin().lock());
    match server::run(stdin, io::stdout().lock(), &backend) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("simpletickets-mcp: {e}");
            ExitCode::FAILURE
        }
    }
}
