//! The sync side: run a connector as a child process and call it.

use crate::{parse_response, request_line, ErrorCode, ErrorInfo};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::Duration;

/// Variables every connector receives besides the ones it asks for.
const BASE_ENV: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "LC_ALL",
    "TMPDIR",
    "TMP",
    "TEMP",
    "TZ",
    "SYSTEMROOT",
    "SYSTEMDRIVE",
    "WINDIR",
    "COMSPEC",
    "PATHEXT",
    "USERPROFILE",
    "APPDATA",
    "LOCALAPPDATA",
    "PROGRAMDATA",
];

/// Anything that answers connector operations; the real one is a [`Process`].
pub trait Transport {
    fn call(&mut self, op: &str, params: Value) -> Result<Value, ErrorInfo>;

    /// False once the connector can no longer answer and must be started again.
    fn is_alive(&mut self) -> bool;
}

/// How to start a connector.
#[derive(Debug, Clone)]
pub struct Spawn {
    pub command: String,
    pub args: Vec<String>,
    /// Names of environment variables copied from this process into the connector's
    /// otherwise minimal environment. The values are never read or logged here.
    pub pass_env: Vec<String>,
    /// How long one call may take before the connector is considered hung and killed.
    pub timeout: Duration,
}

/// A connector child process. Its standard error is inherited, so its log lines end up
/// wherever this process's do.
pub struct Process {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Receiver<String>,
    next_id: u64,
    timeout: Duration,
    dead: bool,
}

/// A bare command is looked up next to the running executable first, which is where the
/// installer puts connectors, then on the `PATH`.
fn resolve(command: &str) -> PathBuf {
    let path = PathBuf::from(command);
    if path.components().count() == 1 {
        if let Some(dir) = std::env::current_exe()
            .ok()
            .and_then(|e| e.parent().map(PathBuf::from))
        {
            let sibling = dir.join(format!("{command}{}", std::env::consts::EXE_SUFFIX));
            if sibling.is_file() {
                return sibling;
            }
        }
    }
    path
}

impl Process {
    pub fn spawn(spec: &Spawn) -> Result<Process, String> {
        let mut command = Command::new(resolve(&spec.command));
        command
            .args(&spec.args)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        for name in BASE_ENV
            .iter()
            .copied()
            .chain(spec.pass_env.iter().map(String::as_str))
        {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
        let mut child = command
            .spawn()
            .map_err(|e| format!("cannot start connector `{}`: {e}", spec.command))?;
        let stdin = child.stdin.take();
        let stdout = child.stdout.take().ok_or("connector has no stdout")?;
        let (sender, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if sender.send(line).is_err() {
                    break;
                }
            }
        });
        Ok(Process {
            child,
            stdin,
            lines,
            next_id: 1,
            timeout: spec.timeout,
            dead: false,
        })
    }

    fn kill(&mut self) {
        self.dead = true;
        self.stdin = None;
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Asks the connector to stop, and kills it if it does not within a moment.
    pub fn close(mut self) {
        if !self.dead {
            let id = self.next_id;
            if let Some(stdin) = self.stdin.as_mut() {
                let _ = writeln!(stdin, "{}", request_line(id, "shutdown", &Value::Null));
                let _ = stdin.flush();
            }
            self.stdin = None;
            for _ in 0..40 {
                if matches!(self.child.try_wait(), Ok(Some(_))) {
                    self.dead = true;
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            self.kill();
        }
    }
}

impl Transport for Process {
    fn call(&mut self, op: &str, params: Value) -> Result<Value, ErrorInfo> {
        let down = |message: &str| ErrorInfo::new(ErrorCode::Unavailable, message);
        if self.dead {
            return Err(down("the connector is not running"));
        }
        let id = self.next_id;
        self.next_id += 1;
        let line = request_line(id, op, &params);
        let sent = self
            .stdin
            .as_mut()
            .map(|s| writeln!(s, "{line}").and_then(|()| s.flush()));
        if !matches!(sent, Some(Ok(()))) {
            self.kill();
            return Err(down("the connector closed its input"));
        }
        loop {
            match self.lines.recv_timeout(self.timeout) {
                Ok(line) => match parse_response(&line) {
                    Ok((answered, outcome)) if answered == id => return outcome,
                    Ok(_) | Err(_) => eprintln!("connector: ignoring unexpected output"),
                },
                Err(RecvTimeoutError::Timeout) => {
                    self.kill();
                    return Err(down("the connector did not answer in time"));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    self.kill();
                    return Err(down("the connector exited"));
                }
            }
        }
    }

    fn is_alive(&mut self) -> bool {
        if !self.dead && !matches!(self.child.try_wait(), Ok(None)) {
            self.dead = true;
        }
        !self.dead
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if !self.dead {
            self.kill();
        }
    }
}
