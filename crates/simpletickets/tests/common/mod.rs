#![allow(dead_code)]

use serde_json::Value;
use simpletickets::daemon::{self, Options, Running};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::path::Path;
use std::time::Duration;

pub struct Api {
    pub running: Option<Running>,
    pub addr: SocketAddr,
    pub token: String,
}

pub fn start(dir: &Path, watch: bool, interval: Duration) -> Api {
    let mut options = Options::new(dir.to_path_buf());
    options.port = 0;
    options.watch = watch;
    options.scan_interval = interval;
    let running = daemon::start(options).expect("daemon starts");
    let token = std::fs::read_to_string(running.token_file()).unwrap();
    Api {
        addr: running.addr,
        token: token.trim().to_string(),
        running: Some(running),
    }
}

impl Api {
    pub fn call(&self, method: &str, path: &str, body: Option<&str>) -> (u16, Value) {
        raw(self.addr, method, path, Some(&self.token), None, body)
    }

    pub fn get(&self, path: &str) -> (u16, Value) {
        self.call("GET", path, None)
    }

    pub fn post(&self, path: &str, body: &str) -> (u16, Value) {
        self.call("POST", path, Some(body))
    }

    pub fn patch(&self, path: &str, body: &str) -> (u16, Value) {
        self.call("PATCH", path, Some(body))
    }
}

impl Drop for Api {
    fn drop(&mut self) {
        if let Some(r) = self.running.take() {
            r.stop();
        }
    }
}

pub fn raw(
    addr: SocketAddr,
    method: &str,
    path: &str,
    token: Option<&str>,
    host: Option<&str>,
    body: Option<&str>,
) -> (u16, Value) {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let body = body.unwrap_or("");
    let host = host.map_or_else(|| addr.to_string(), str::to_string);
    let mut request = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n");
    if let Some(t) = token {
        request.push_str(&format!("Authorization: Bearer {t}\r\n"));
    }
    request.push_str(&format!("Content-Length: {}\r\n\r\n{body}", body.len()));
    stream.write_all(request.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    let status = response.split(' ').nth(1).unwrap().parse().unwrap();
    let payload = response.split_once("\r\n\r\n").map_or("", |(_, b)| b);
    (status, serde_json::from_str(payload).unwrap_or(Value::Null))
}

pub fn write(dir: &Path, rel: &str, text: &str) {
    let path = dir.join(rel);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

pub fn wait_for(mut condition: impl FnMut() -> bool) -> bool {
    for _ in 0..100 {
        if condition() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    false
}
