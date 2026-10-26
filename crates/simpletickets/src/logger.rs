//! A small line logger: leveled, timestamped, written to stderr or an append-only file.
//!
//! Each call writes and flushes one line, so nothing is lost when the process exits.
//! Callers pass only values they know to be safe; the API token is never a log argument.

use simpletickets_core::time::now_timestamp;
use std::fs::OpenOptions;
use std::io::{self, Write};
use std::path::Path;
use std::sync::Mutex;

/// Environment variables read by the command line for the default level and file.
pub const LEVEL_ENV: &str = "SIMPLETICKETS_LOG_LEVEL";
pub const FILE_ENV: &str = "SIMPLETICKETS_LOG_FILE";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug,
}

impl Level {
    pub fn parse(text: &str) -> Option<Level> {
        match text.trim().to_ascii_lowercase().as_str() {
            "error" => Some(Level::Error),
            "warn" | "warning" => Some(Level::Warn),
            "info" => Some(Level::Info),
            "debug" => Some(Level::Debug),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Level::Error => "ERROR",
            Level::Warn => "WARN",
            Level::Info => "INFO",
            Level::Debug => "DEBUG",
        }
    }
}

enum Sink {
    Discard,
    Stderr,
    Writer(Mutex<Box<dyn Write + Send>>),
}

pub struct Logger {
    level: Level,
    sink: Sink,
}

impl std::fmt::Debug for Logger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Logger({:?})", self.level)
    }
}

impl Logger {
    /// A logger that drops everything.
    pub fn discard() -> Logger {
        Logger {
            level: Level::Error,
            sink: Sink::Discard,
        }
    }

    pub fn stderr(level: Level) -> Logger {
        Logger {
            level,
            sink: Sink::Stderr,
        }
    }

    /// Appends to `path`, creating the file and its folder if needed.
    pub fn file(level: Level, path: &Path) -> io::Result<Logger> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Logger::writer(level, Box::new(file)))
    }

    pub fn writer(level: Level, writer: Box<dyn Write + Send>) -> Logger {
        Logger {
            level,
            sink: Sink::Writer(Mutex::new(writer)),
        }
    }

    pub fn enabled(&self, level: Level) -> bool {
        !matches!(self.sink, Sink::Discard) && level <= self.level
    }

    pub fn log(&self, level: Level, message: &str) {
        if !self.enabled(level) {
            return;
        }
        let line = format!("{} {:<5} {}\n", now_timestamp(), level.label(), message);
        match &self.sink {
            Sink::Discard => {}
            Sink::Stderr => {
                let _ = io::stderr().write_all(line.as_bytes());
            }
            Sink::Writer(w) => {
                let mut w = w.lock().unwrap_or_else(|e| e.into_inner());
                let _ = w.write_all(line.as_bytes());
                let _ = w.flush();
            }
        }
    }

    pub fn error(&self, message: &str) {
        self.log(Level::Error, message);
    }

    pub fn warn(&self, message: &str) {
        self.log(Level::Warn, message);
    }

    pub fn info(&self, message: &str) {
        self.log(Level::Info, message);
    }

    pub fn debug(&self, message: &str) {
        self.log(Level::Debug, message);
    }

    pub fn flush(&self) {
        match &self.sink {
            Sink::Stderr => {
                let _ = io::stderr().flush();
            }
            Sink::Writer(w) => {
                let _ = w.lock().unwrap_or_else(|e| e.into_inner()).flush();
            }
            Sink::Discard => {}
        }
    }
}

/// Renders `key=value` pairs, quoting values that contain whitespace or quotes.
pub fn fields(pairs: &[(&str, String)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| {
            if v.is_empty() || v.chars().any(|c| c.is_whitespace() || c == '"') {
                format!("{k}={v:?}")
            } else {
                format!("{k}={v}")
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[derive(Clone, Default)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);

    impl Write for Buffer {
        fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn parses_levels() {
        assert_eq!(Level::parse("INFO"), Some(Level::Info));
        assert_eq!(Level::parse(" warning "), Some(Level::Warn));
        assert_eq!(Level::parse("loud"), None);
    }

    #[test]
    fn filters_by_level_and_formats_lines() {
        let buffer = Buffer::default();
        let logger = Logger::writer(Level::Warn, Box::new(buffer.clone()));
        logger.debug("hidden");
        logger.info("hidden");
        logger.warn("shown");
        logger.error("also shown");
        let text = String::from_utf8(buffer.0.lock().unwrap().clone()).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].ends_with(" WARN  shown"), "{}", lines[0]);
        assert!(lines[1].ends_with(" ERROR also shown"), "{}", lines[1]);
        assert!(lines[0].contains('T') && lines[0].contains('Z'));
    }

    #[test]
    fn discard_logger_is_never_enabled() {
        assert!(!Logger::discard().enabled(Level::Error));
    }

    #[test]
    fn file_sink_appends_and_creates_folders() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logs/daemon.log");
        Logger::file(Level::Info, &path).unwrap().info("one");
        Logger::file(Level::Info, &path).unwrap().info("two");
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 2);
    }

    #[test]
    fn fields_quote_when_needed() {
        let out = fields(&[
            ("a", "x".into()),
            ("b", "two words".into()),
            ("c", "".into()),
        ]);
        assert_eq!(out, r#"a=x b="two words" c="""#);
    }
}
