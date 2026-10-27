//! Request checks that keep other web pages from driving the board: the `Host` must be a
//! localhost name, requests must come from the board's own origin (state-changing ones must
//! name it), and every API request must carry the session token issued with the page.

/// Name of the header that carries the per-session token.
pub const SESSION_HEADER: &str = "X-SimpleTickets-Session";

/// The parts of a request the checks look at.
#[derive(Debug, Default, Clone)]
pub struct RequestInfo<'a> {
    pub method: &'a str,
    pub host: Option<&'a str>,
    pub origin: Option<&'a str>,
    pub session: Option<&'a str>,
    pub fetch_site: Option<&'a str>,
}

/// Why a request was refused: status code and message.
pub type Refusal = (u16, &'static str);

/// Splits a `Host` value into its name and optional port.
fn split_host(value: &str) -> Option<(&str, Option<&str>)> {
    if let Some(rest) = value.strip_prefix('[') {
        let (name, tail) = rest.split_once(']')?;
        return match tail {
            "" => Some((name, None)),
            _ => tail.strip_prefix(':').map(|port| (name, Some(port))),
        };
    }
    match value.rsplit_once(':') {
        Some((name, port)) if !name.contains(':') => Some((name, Some(port))),
        _ => Some((value, None)),
    }
}

/// True for `127.0.0.1`, `localhost` and `::1`, with an optional numeric port.
pub fn is_local_host(value: &str) -> bool {
    let Some((name, port)) = split_host(value) else {
        return false;
    };
    if let Some(port) = port {
        if !port.bytes().all(|b| b.is_ascii_digit()) || port.parse::<u16>().is_err() {
            return false;
        }
    }
    matches!(
        name.to_ascii_lowercase().as_str(),
        "127.0.0.1" | "localhost" | "::1"
    )
}

fn is_safe_method(method: &str) -> bool {
    matches!(method, "GET" | "HEAD")
}

/// An origin matches when it is `http://` plus exactly the request's own `Host`.
fn origin_matches(origin: &str, host: &str) -> bool {
    origin
        .strip_prefix("http://")
        .is_some_and(|rest| rest.eq_ignore_ascii_case(host))
}

/// Compares two strings without stopping at the first difference.
pub fn constant_time_eq(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0, |acc, (x, y)| acc | (x ^ y))
            == 0
}

/// Host check for every request, pages included.
pub fn check_host(request: &RequestInfo) -> Result<(), Refusal> {
    match request.host {
        Some(host) if is_local_host(host) => Ok(()),
        _ => Err((403, "Host is not a localhost name")),
    }
}

/// Checks for API requests: host, origin, fetch metadata and session token.
pub fn check_api(request: &RequestInfo, session: &str) -> Result<(), Refusal> {
    check_host(request)?;
    let host = request.host.unwrap_or_default();
    match request.origin {
        Some(origin) if origin_matches(origin, host) => {}
        Some(_) => return Err((403, "cross-origin request refused")),
        None if !is_safe_method(request.method) => {
            return Err((403, "state-changing requests need an Origin header"))
        }
        None => {}
    }
    if matches!(request.fetch_site, Some("cross-site" | "same-site")) {
        return Err((403, "cross-site request refused"));
    }
    match request.session {
        Some(sent) if constant_time_eq(sent, session) => Ok(()),
        _ => Err((403, "missing or invalid session token")),
    }
}

/// Maps a board request onto the daemon route it may reach, or `None` when the board
/// has no business calling it. `path` starts with `/api` and `query` is the raw query.
pub fn daemon_route(method: &str, path: &str, query: &str) -> Option<String> {
    if !query
        .bytes()
        .all(|b| (0x21..0x7f).contains(&b) && b != b'#')
    {
        return None;
    }
    let rest = path.strip_prefix("/api/v1/")?;
    let segments: Vec<&str> = rest.split('/').collect();
    let allowed = match (method, segments.as_slice()) {
        (
            "GET",
            ["health" | "parents" | "tickets" | "search" | "events" | "doctor" | "config"
            | "templates"],
        ) => true,
        ("POST", ["tickets" | "rescan"]) => true,
        ("GET" | "PATCH", ["tickets", id]) => valid_id(id),
        _ => false,
    };
    if !allowed {
        return None;
    }
    let mut route = format!("/v1/{rest}");
    if !query.is_empty() {
        route.push('?');
        route.push_str(query);
    }
    Some(route)
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 128
        && id != "."
        && id != ".."
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req<'a>(
        method: &'a str,
        origin: Option<&'a str>,
        session: Option<&'a str>,
    ) -> RequestInfo<'a> {
        RequestInfo {
            method,
            host: Some("127.0.0.1:7979"),
            origin,
            session,
            fetch_site: None,
        }
    }

    #[test]
    fn accepts_only_localhost_hosts() {
        for ok in [
            "127.0.0.1",
            "127.0.0.1:7979",
            "localhost:80",
            "LOCALHOST",
            "[::1]:9",
            "::1",
        ] {
            assert!(is_local_host(ok), "{ok}");
        }
        for bad in [
            "evil.example",
            "127.0.0.1.evil.example",
            "localhost.evil.example:7979",
            "127.0.0.1:",
            "127.0.0.1:99999",
            "127.0.0.1:80x",
            "[::1",
            "",
            "10.0.0.1",
        ] {
            assert!(!is_local_host(bad), "{bad}");
        }
    }

    #[test]
    fn api_needs_session_and_matching_origin() {
        let origin = Some("http://127.0.0.1:7979");
        assert!(check_api(&req("GET", None, Some("s3")), "s3").is_ok());
        assert!(check_api(&req("PATCH", origin, Some("s3")), "s3").is_ok());
        assert_eq!(check_api(&req("GET", None, None), "s3").unwrap_err().0, 403);
        assert!(check_api(&req("GET", None, Some("nope")), "s3").is_err());
        assert!(check_api(&req("PATCH", None, Some("s3")), "s3").is_err());
        assert!(check_api(&req("POST", Some("http://evil.example"), Some("s3")), "s3").is_err());
        assert!(check_api(&req("POST", Some("null"), Some("s3")), "s3").is_err());
        assert!(check_api(&req("GET", Some("http://localhost:7979"), Some("s3")), "s3").is_err());
    }

    #[test]
    fn rejects_cross_site_fetch_metadata_and_foreign_hosts() {
        let mut r = req("GET", None, Some("s3"));
        r.fetch_site = Some("cross-site");
        assert!(check_api(&r, "s3").is_err());
        r.fetch_site = Some("same-origin");
        assert!(check_api(&r, "s3").is_ok());
        r.host = Some("rebind.example");
        assert!(check_api(&r, "s3").is_err());
        r.host = None;
        assert!(check_host(&r).is_err());
    }

    #[test]
    fn allowlists_daemon_routes() {
        assert_eq!(
            daemon_route("GET", "/api/v1/tickets", "status=todo").as_deref(),
            Some("/v1/tickets?status=todo")
        );
        assert_eq!(
            daemon_route("PATCH", "/api/v1/tickets/ST-1", "").as_deref(),
            Some("/v1/tickets/ST-1")
        );
        assert!(daemon_route("GET", "/api/v1/events", "since=3&wait=25").is_some());
        for (method, path) in [
            ("PUT", "/api/v1/config"),
            ("POST", "/api/v1/migration"),
            ("DELETE", "/api/v1/tombstones/ST-1"),
            ("POST", "/api/v1/tickets/ST-1/reassign-id"),
            ("GET", "/api/v1/tickets/../config"),
            ("GET", "/api/v1/tickets/a%2Fb"),
            ("GET", "/api/v2/tickets"),
            ("GET", "/api/v1/audit"),
            ("DELETE", "/api/v1/tickets/ST-1"),
        ] {
            assert!(daemon_route(method, path, "").is_none(), "{method} {path}");
        }
        assert!(daemon_route("GET", "/api/v1/tickets", "a=b c").is_none());
        assert!(daemon_route("GET", "/api/v1/tickets", "a=\r\nx").is_none());
    }

    #[test]
    fn constant_time_eq_compares_values() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "ab"));
    }
}
