//! Forwards requests to the daemon's loopback API with the token read server-side.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::time::Duration;

/// A daemon reply, reduced to what the board may see.
pub struct Reply {
    pub status: u16,
    pub content_type: String,
    pub api_version: Option<String>,
    pub body: Vec<u8>,
}

/// Where the daemon is and where its token lives.
pub struct Daemon {
    pub address: String,
    pub token_file: PathBuf,
}

impl Daemon {
    /// `url` looks like `http://127.0.0.1:7878`; only plain HTTP is supported.
    pub fn new(url: &str, token_file: PathBuf) -> Result<Daemon, String> {
        let rest = url
            .strip_prefix("http://")
            .ok_or_else(|| format!("unsupported URL `{url}`: only http:// is supported"))?;
        let address = rest.trim_end_matches('/');
        if address.is_empty() || address.contains('/') {
            return Err(format!("invalid URL `{url}`"));
        }
        Ok(Daemon {
            address: address.to_string(),
            token_file,
        })
    }

    /// Performs one request. The token file is read on every call, so a rotated token
    /// is picked up without a restart. Errors are messages safe to show the board.
    pub fn forward(&self, method: &str, route: &str, body: &[u8]) -> Result<Reply, String> {
        let token = std::fs::read_to_string(&self.token_file)
            .map_err(|_| "cannot read the daemon token file".to_string())?;
        let target = self
            .address
            .to_socket_addrs()
            .ok()
            .and_then(|mut a| a.next())
            .ok_or_else(|| format!("cannot resolve {}", self.address))?;
        let mut stream = TcpStream::connect_timeout(&target, Duration::from_secs(3))
            .map_err(|_| format!("cannot reach the daemon at {}", self.address))?;
        let timeout = Some(Duration::from_secs(45));
        let _ = stream.set_read_timeout(timeout);
        let _ = stream.set_write_timeout(timeout);
        let mut head = format!(
            "{method} {route} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nAuthorization: Bearer {}\r\nAccept: application/json\r\n",
            self.address,
            token.trim()
        );
        if !body.is_empty() {
            head.push_str("Content-Type: application/json\r\n");
        }
        head.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
        stream
            .write_all(head.as_bytes())
            .and_then(|_| stream.write_all(body))
            .map_err(|_| "sending the request to the daemon failed".to_string())?;
        let mut raw = Vec::new();
        stream
            .read_to_end(&mut raw)
            .map_err(|_| "reading the daemon's reply failed".to_string())?;
        parse_reply(&raw)
    }
}

fn parse_reply(raw: &[u8]) -> Result<Reply, String> {
    let split = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("malformed reply from the daemon")?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let mut lines = head.split("\r\n");
    let status: u16 = lines
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or("malformed status line from the daemon")?;
    let mut content_type = "application/json".to_string();
    let mut api_version = None;
    let mut chunked = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.to_ascii_lowercase().as_str() {
            "content-type" => content_type = value.to_string(),
            "x-simpletickets-api-version" => api_version = Some(value.to_string()),
            "transfer-encoding" => chunked = value.to_ascii_lowercase().contains("chunked"),
            _ => {}
        }
    }
    let payload = &raw[split + 4..];
    let body = if chunked {
        dechunk(payload).ok_or("malformed chunked reply from the daemon")?
    } else {
        payload.to_vec()
    };
    Ok(Reply {
        status,
        content_type,
        api_version,
        body,
    })
}

fn dechunk(mut data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = data.windows(2).position(|w| w == b"\r\n")?;
        let size_text = std::str::from_utf8(&data[..line_end]).ok()?;
        let size = usize::from_str_radix(size_text.split(';').next()?.trim(), 16).ok()?;
        data = &data[line_end + 2..];
        if size == 0 {
            return Some(out);
        }
        if data.len() < size + 2 {
            return None;
        }
        out.extend_from_slice(&data[..size]);
        data = &data[size + 2..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_and_chunked_replies() {
        let plain = b"HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nX-SimpleTickets-Api-Version: 1\r\nSet-Cookie: x=1\r\n\r\n{\"a\":1}";
        let reply = parse_reply(plain).unwrap();
        assert_eq!(reply.status, 201);
        assert_eq!(reply.api_version.as_deref(), Some("1"));
        assert_eq!(reply.body, b"{\"a\":1}");
        let chunked =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2\r\nde\r\n0\r\n\r\n";
        assert_eq!(parse_reply(chunked).unwrap().body, b"abcde");
        assert!(parse_reply(b"garbage").is_err());
    }

    #[test]
    fn rejects_unsupported_urls() {
        assert!(Daemon::new("https://x", PathBuf::new()).is_err());
        assert!(Daemon::new("http://x/p", PathBuf::new()).is_err());
        assert!(Daemon::new("http://127.0.0.1:7878/", PathBuf::new()).is_ok());
    }
}
