//! Update check: periodically asks the project's GitHub releases for a newer version.
//!
//! The result is only reported (a daemon event and the `/v1/health` field); nothing is
//! ever downloaded or installed. Failures such as being offline are silent and retried
//! at the next interval.

use serde_json::Value;
use std::cmp::Ordering;
use std::sync::Arc;
use std::time::Duration;

pub const RELEASES_URL: &str =
    "https://api.github.com/repos/MichalVacek/SimpleTickets/releases/latest";
pub const DEFAULT_INTERVAL: Duration = Duration::from_secs(24 * 3600);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// The newest published release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    /// Version tag as published, for example `v0.2.0`.
    pub tag: String,
    pub url: String,
}

/// Source of release information; replaced by a fake in tests.
pub trait Fetcher: Send + Sync {
    fn latest(&self) -> Result<Release, String>;
}

/// Reads the latest release from the GitHub API.
pub struct GithubFetcher {
    url: String,
}

impl GithubFetcher {
    pub fn new() -> GithubFetcher {
        GithubFetcher {
            url: RELEASES_URL.to_string(),
        }
    }
}

impl Default for GithubFetcher {
    fn default() -> Self {
        Self::new()
    }
}

impl Fetcher for GithubFetcher {
    fn latest(&self) -> Result<Release, String> {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(REQUEST_TIMEOUT))
            .build()
            .into();
        let mut response = agent
            .get(&self.url)
            .header("Accept", "application/vnd.github+json")
            .header(
                "User-Agent",
                concat!("simpletickets/", env!("CARGO_PKG_VERSION")),
            )
            .call()
            .map_err(|e| e.to_string())?;
        let text = response
            .body_mut()
            .read_to_string()
            .map_err(|e| e.to_string())?;
        parse_release(&text)
    }
}

/// Extracts the tag and page URL from a GitHub "latest release" response.
pub fn parse_release(text: &str) -> Result<Release, String> {
    let v: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
    let tag = v["tag_name"].as_str().ok_or("release has no tag_name")?;
    let url = v["html_url"].as_str().unwrap_or("");
    Ok(Release {
        tag: tag.to_string(),
        url: url.to_string(),
    })
}

fn numbers(version: &str) -> Option<Vec<u64>> {
    let core = version.trim().trim_start_matches(['v', 'V']);
    let core = core.split(['-', '+']).next()?;
    core.split('.').map(|p| p.parse().ok()).collect()
}

/// Whether `candidate` is a higher version than `current`. Unparseable versions never
/// count as newer.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    let (Some(mut a), Some(mut b)) = (numbers(candidate), numbers(current)) else {
        return false;
    };
    let len = a.len().max(b.len());
    a.resize(len, 0);
    b.resize(len, 0);
    a.cmp(&b) == Ordering::Greater
}

/// How the daemon checks for updates.
#[derive(Clone)]
pub struct UpdateCheck {
    pub fetcher: Arc<dyn Fetcher>,
    pub interval: Duration,
}

impl std::fmt::Debug for UpdateCheck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UpdateCheck")
            .field("interval", &self.interval)
            .finish_non_exhaustive()
    }
}

impl UpdateCheck {
    pub fn github() -> UpdateCheck {
        UpdateCheck {
            fetcher: Arc::new(GithubFetcher::new()),
            interval: DEFAULT_INTERVAL,
        }
    }
}

/// Runs one check; `Some` when the latest release is newer than `current`.
pub fn check_once(fetcher: &dyn Fetcher, current: &str) -> Option<Release> {
    fetcher.latest().ok().filter(|r| is_newer(&r.tag, current))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_versions() {
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(is_newer("1.0", "0.9.9"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(is_newer("0.10.0", "0.9.0"));
        assert!(!is_newer("nightly", "0.1.0"));
        assert!(!is_newer("0.2.0", "garbage"));
    }

    #[test]
    fn ignores_prerelease_suffix() {
        assert!(!is_newer("0.1.0-rc1", "0.1.0"));
        assert!(is_newer("0.2.0-rc1", "0.1.0"));
    }

    #[test]
    fn parses_release_payload() {
        let r = parse_release(r#"{"tag_name":"v0.2.0","html_url":"https://x/r","draft":false}"#)
            .unwrap();
        assert_eq!(r.tag, "v0.2.0");
        assert_eq!(r.url, "https://x/r");
        assert!(parse_release("{}").is_err());
        assert!(parse_release("not json").is_err());
    }

    struct Fixed(Result<Release, String>);
    impl Fetcher for Fixed {
        fn latest(&self) -> Result<Release, String> {
            self.0.clone()
        }
    }

    #[test]
    fn check_reports_only_newer_releases_and_hides_errors() {
        let release = |tag: &str| {
            Fixed(Ok(Release {
                tag: tag.into(),
                url: String::new(),
            }))
        };
        assert!(check_once(&release("v9.0.0"), "0.1.0").is_some());
        assert!(check_once(&release("v0.1.0"), "0.1.0").is_none());
        assert!(check_once(&Fixed(Err("offline".into())), "0.1.0").is_none());
    }
}
