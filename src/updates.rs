// Opt-in update check. When [updates].check_enabled is true, a
// background task polls the project's version manifest at
// https://localsky.io/latest.json daily (with jitter) and caches the
// newest version; GET /api/v1/updates serves the comparison. The request
// is a plain GET whose only identifier is the User-Agent (localsky/<ver>),
// so the manifest host sees aggregate version counts but nothing
// per-install rides along. Nothing self-updates: docker pull stays the
// upgrade mechanism. The manifest mirrors the GitHub release shape
// (tag_name + html_url) so the comparison logic is source-agnostic.

use std::sync::Arc;

use serde::Serialize;
use tokio::sync::RwLock;

const RELEASES_URL: &str = "https://localsky.io/latest.json";

/// The only identifier that rides along: package + version, deliberately
/// NOT `sources::derived_user_agent` (which carries the per-install
/// instance id) so the manifest host sees aggregate version counts and
/// nothing per-install.
const USER_AGENT: &str = concat!("localsky/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Clone, Serialize, Default)]
pub struct UpdateStatus {
    pub current: String,
    pub latest: Option<String>,
    pub update_available: bool,
    pub release_url: Option<String>,
    pub checked_at_epoch: Option<i64>,
    pub check_enabled: bool,
}

/// The cached comparison, shared by the checker task and the handler.
#[derive(Clone)]
pub struct Updates {
    cache: Arc<RwLock<UpdateStatus>>,
}

impl Default for Updates {
    fn default() -> Self {
        Self::new()
    }
}

impl Updates {
    pub fn new() -> Self {
        Self {
            cache: Arc::new(RwLock::new(UpdateStatus {
                current: env!("CARGO_PKG_VERSION").to_string(),
                ..Default::default()
            })),
        }
    }

    pub async fn status(&self) -> UpdateStatus {
        self.cache.read().await.clone()
    }
}

fn newer(latest: &str, current: &str) -> bool {
    let parse = |s: &str| semver::Version::parse(s.trim_start_matches('v'));
    match (parse(latest), parse(current)) {
        (Ok(l), Ok(c)) => l > c,
        _ => false,
    }
}

async fn check_once(cache: &RwLock<UpdateStatus>, client: &reqwest::Client) {
    let resp = client
        .get(RELEASES_URL)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/json")
        .send()
        .await;
    let Ok(resp) = resp else { return };
    let Ok(v) = resp.json::<serde_json::Value>().await else {
        return;
    };
    let tag = v
        .get("tag_name")
        .and_then(|t| t.as_str())
        .map(str::to_string);
    let url = v
        .get("html_url")
        .and_then(|u| u.as_str())
        .map(str::to_string);
    let mut c = cache.write().await;
    c.checked_at_epoch = Some(chrono::Utc::now().timestamp());
    if let Some(tag) = tag {
        c.update_available = newer(&tag, &c.current);
        c.latest = Some(tag);
        c.release_url = url;
    }
}

/// Spawn the daily checker (only call when [updates].check_enabled).
/// Start the daily check. Only called when `[updates].check_enabled`;
/// the handle answers "check disabled" until then.
pub fn spawn(updates: &Updates) {
    let cache = updates.cache.clone();
    tokio::spawn(async move {
        {
            cache.write().await.check_enabled = true;
        }
        // client_with, not client: the derived identity would leak the
        // instance id to the manifest host (see USER_AGENT).
        let client = crate::net::client_with(std::time::Duration::from_secs(15), USER_AGENT);
        // First check shortly after boot, then daily with PID-seeded
        // jitter so a fleet doesn't thundering-herd GitHub.
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        loop {
            check_once(&cache, &client).await;
            let jitter = u64::from(std::process::id() % 1800);
            tokio::time::sleep(std::time::Duration::from_secs(86_400 + jitter)).await;
        }
    });
}

/// GET /api/v1/updates handler.
pub async fn updates_handler(
    axum::extract::State(updates): axum::extract::State<Updates>,
) -> axum::Json<UpdateStatus> {
    axum::Json(updates.status().await)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_comparison() {
        assert!(newer("v0.3.0", "0.2.0"));
        assert!(newer("0.2.1", "0.2.0"));
        assert!(!newer("0.2.0", "0.2.0"));
        assert!(!newer("v0.1.9", "0.2.0"));
        // Prerelease -> release counts as newer.
        assert!(newer("0.2.0", "0.2.0-alpha.1"));
        assert!(!newer("garbage", "0.2.0"));
    }
}
