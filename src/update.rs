//! "Is there a newer ferox?" — one GET to the GitHub releases API on its own
//! thread (own current_thread runtime, like the DB and AI workers), result
//! delivered over an mpsc channel so the UI thread never waits on the network.
//! Only the version number is compared; nothing is downloaded or installed —
//! the user gets a link to the release page.

use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use anyhow::{bail, Result};

const LATEST_RELEASE_API: &str = "https://api.github.com/repos/frkdrgt/ferox/releases/latest";

#[derive(Debug, Clone)]
pub struct ReleaseInfo {
    /// Version without the leading `v`, e.g. `0.3.1`.
    pub version: String,
    /// Release page on GitHub.
    pub url: String,
}

/// `Ok(Some)` = newer release available, `Ok(None)` = up to date.
pub type CheckResult = Result<Option<ReleaseInfo>, String>;

/// Start a check in the background. `ctx` is repainted when the answer
/// arrives so the result shows up without waiting for user input.
pub fn spawn_check(ctx: egui::Context) -> Receiver<CheckResult> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = match tokio::runtime::Builder::new_current_thread().enable_all().build() {
            Ok(rt) => rt.block_on(fetch_latest()).map_err(|e| e.to_string()),
            Err(e) => Err(e.to_string()),
        };
        let result = result.map(|latest| {
            is_newer(&latest.version, env!("CARGO_PKG_VERSION")).then_some(latest)
        });
        let _ = tx.send(result);
        ctx.request_repaint();
    });
    rx
}

async fn fetch_latest() -> Result<ReleaseInfo> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    let resp = client
        .get(LATEST_RELEASE_API)
        // GitHub rejects requests without a User-Agent.
        .header("User-Agent", concat!("ferox/", env!("CARGO_PKG_VERSION")))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?;
    let status = resp.status();
    if !status.is_success() {
        bail!("GitHub API {status}");
    }
    let json: serde_json::Value = resp.json().await?;
    let tag = json["tag_name"].as_str().unwrap_or_default();
    if parse_version(tag).is_none() {
        bail!("unexpected release tag {tag:?}");
    }
    Ok(ReleaseInfo {
        version: tag.trim_start_matches('v').to_owned(),
        url: json["html_url"]
            .as_str()
            .unwrap_or("https://github.com/frkdrgt/ferox/releases")
            .to_owned(),
    })
}

/// `v1.2.3`, `1.2.3`, `1.2.3-beta.1` → (1, 2, 3). Missing minor/patch count as 0.
pub fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    let core = s.trim().trim_start_matches('v');
    let core = core.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().map_or(Some(0), |p| p.parse().ok())?;
    let patch = parts.next().map_or(Some(0), |p| p.parse().ok())?;
    Some((major, minor, patch))
}

pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_version(latest), parse_version(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_versions() {
        assert_eq!(parse_version("v0.3.0"), Some((0, 3, 0)));
        assert_eq!(parse_version("1.10.2"), Some((1, 10, 2)));
        assert_eq!(parse_version("v2.0.0-beta.1"), Some((2, 0, 0)));
        assert_eq!(parse_version("v1.2"), Some((1, 2, 0)));
        assert_eq!(parse_version("nightly"), None);
    }

    /// Hits the real GitHub API — `cargo test --bin ferox update -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn fetches_latest_release() {
        let r = fetch_latest().await.expect("GitHub request failed");
        assert!(parse_version(&r.version).is_some(), "bad version {:?}", r.version);
        assert!(r.url.starts_with("https://github.com/frkdrgt/ferox/releases"), "{}", r.url);
    }

    #[test]
    fn compares_numerically_not_lexically() {
        assert!(is_newer("v0.10.0", "0.9.9"));
        assert!(is_newer("0.3.1", "0.3.0"));
        assert!(!is_newer("v0.3.0", "0.3.0"));
        assert!(!is_newer("v0.2.9", "0.3.0"));
        assert!(!is_newer("garbage", "0.3.0"));
    }
}
