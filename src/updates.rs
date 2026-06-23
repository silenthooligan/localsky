// Opt-in update check. When [updates].check_enabled is true, a
// background task polls the project's version manifest at
// https://localsky.io/latest.json daily (with jitter) and caches the
// newest version; GET /api/v1/updates serves the comparison. The request
// is a plain GET whose only identifier is the User-Agent (localsky/<ver>),
// so the manifest host sees aggregate version counts but nothing
// per-install rides along. Nothing self-updates: docker pull stays the
// upgrade mechanism. The manifest mirrors the GitHub release shape
// (tag_name + html_url) so the comparison logic is source-agnostic.

use std::sync::OnceLock;

use serde::Serialize;
use tokio::sync::RwLock;

const RELEASES_URL: &str = "https://localsky.io/latest.json";

#[derive(Debug, Clone, Serialize, Default)]
pub struct UpdateStatus {
    pub current: String,
    pub latest: Option<String>,
    pub update_available: bool,
    pub release_url: Option<String>,
    pub checked_at_epoch: Option<i64>,
    pub check_enabled: bool,
}

fn cache() -> &'static RwLock<UpdateStatus> {
    static CACHE: OnceLock<RwLock<UpdateStatus>> = OnceLock::new();
    CACHE.get_or_init(|| {
        RwLock::new(UpdateStatus {
            current: env!("CARGO_PKG_VERSION").to_string(),
            ..Default::default()
        })
    })
}

pub async fn status() -> UpdateStatus {
    cache().read().await.clone()
}

fn newer(latest: &str, current: &str) -> bool {
    let parse = |s: &str| semver::Version::parse(s.trim_start_matches('v'));
    match (parse(latest), parse(current)) {
        (Ok(l), Ok(c)) => l > c,
        _ => false,
    }
}

async fn check_once(client: &reqwest::Client) {
    let resp = client
        .get(RELEASES_URL)
        .header(
            "User-Agent",
            concat!("localsky/", env!("CARGO_PKG_VERSION")),
        )
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
    let mut c = cache().write().await;
    c.checked_at_epoch = Some(chrono::Utc::now().timestamp());
    if let Some(tag) = tag {
        c.update_available = newer(&tag, &c.current);
        c.latest = Some(tag);
        c.release_url = url;
    }
}

/// Spawn the daily checker (only call when [updates].check_enabled).
pub fn spawn() {
    tokio::spawn(async move {
        {
            cache().write().await.check_enabled = true;
        }
        let Ok(client) = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(15))
            .build()
        else {
            return;
        };
        // First check shortly after boot, then daily with PID-seeded
        // jitter so a fleet doesn't thundering-herd GitHub.
        tokio::time::sleep(std::time::Duration::from_secs(60)).await;
        loop {
            check_once(&client).await;
            let jitter = u64::from(std::process::id() % 1800);
            tokio::time::sleep(std::time::Duration::from_secs(86_400 + jitter)).await;
        }
    });
}

/// GET /api/v1/updates handler.
pub async fn updates_handler() -> axum::Json<UpdateStatus> {
    axum::Json(status().await)
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
