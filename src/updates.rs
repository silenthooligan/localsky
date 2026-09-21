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
    pub attempted_at_epoch: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub diagnostic: Option<crate::failure::FailureRecord>,
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

async fn fetch_manifest(
    client: &reqwest::Client,
    url: &str,
) -> anyhow::Result<(String, Option<String>)> {
    use crate::failure::{Failure, FailureCode as Code};
    let response = client
        .get(url)
        .header("User-Agent", USER_AGENT)
        .header("Accept", "application/json")
        .send()
        .await
        .map_err(|e| crate::net::source_failure::from_reqwest(&e, "updates fetch manifest"))?;
    if !response.status().is_success() {
        return Err(Failure::http(
            response.status().as_u16(),
            Some(crate::net::source_failure::response_format(&response)),
            "updates fetch manifest",
        )
        .into());
    }
    let value: serde_json::Value = crate::net::safe_fetch::read_json_capped(response)
        .await
        .map_err(|e| crate::net::source_failure::from_safe(&e, "updates decode manifest"))?;
    let tag = value
        .get("tag_name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            Failure::new(Code::UpdateManifest, "updates validate manifest").with_field("tag_name")
        })?;
    semver::Version::parse(tag.strip_prefix('v').unwrap_or(tag)).map_err(|_| {
        Failure::new(Code::UpdateManifest, "updates validate version").with_field("tag_name")
    })?;
    let release_url = value.get("html_url").and_then(|v| v.as_str());
    if let Some(url) = release_url {
        let parsed = reqwest::Url::parse(url).map_err(|_| {
            Failure::new(Code::UpdateManifest, "updates validate release link")
                .with_field("html_url")
        })?;
        if parsed.scheme() != "https"
            || parsed.host_str().is_none()
            || !parsed.username().is_empty()
            || parsed.password().is_some()
        {
            return Err(
                Failure::new(Code::UpdateManifest, "updates validate release link")
                    .with_field("html_url")
                    .into(),
            );
        }
    }
    Ok((tag.to_owned(), release_url.map(str::to_owned)))
}

async fn check_at(cache: &RwLock<UpdateStatus>, client: &reqwest::Client, url: &str) {
    let result = fetch_manifest(client, url).await;
    let now = chrono::Utc::now().timestamp();
    let mut status = cache.write().await;
    status.attempted_at_epoch = Some(now);
    match result {
        Ok((tag, url)) => {
            status.update_available = newer(&tag, &status.current);
            status.latest = Some(tag);
            status.release_url = url;
            status.checked_at_epoch = Some(now);
            status.diagnostic = None;
        }
        Err(error) => {
            let failure = crate::diagnostics::from_anyhow(&error, "updates check");
            tracing::warn!(error_code = failure.code.as_str(), error = %failure, "update check failed");
            status.diagnostic = Some(crate::failure::FailureRecord {
                at_epoch: now,
                failure,
            });
        }
    }
}

async fn check_once(cache: &RwLock<UpdateStatus>, client: &reqwest::Client) {
    check_at(cache, client, RELEASES_URL).await;
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

    #[tokio::test]
    async fn failed_check_keeps_last_success_and_clears_diagnostic_only_after_recovery() {
        use wiremock::{matchers::method, Mock, MockServer, ResponseTemplate};
        let server = MockServer::start().await;
        let updates = Updates::new();
        {
            let mut status = updates.cache.write().await;
            status.latest = Some("v0.9.1".into());
            status.checked_at_epoch = Some(100);
        }
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(502).set_body_string("secret proxy response"))
            .mount(&server)
            .await;
        let client = crate::net::client(std::time::Duration::from_secs(2));
        check_at(&updates.cache, &client, &server.uri()).await;
        let failed = updates.status().await;
        assert_eq!(failed.checked_at_epoch, Some(100));
        assert_eq!(failed.latest.as_deref(), Some("v0.9.1"));
        assert_eq!(failed.diagnostic.unwrap().failure.http_status, Some(502));
        server.reset().await;
        Mock::given(method("GET"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(serde_json::json!({"tag_name":"invalid-secret-version"})),
            )
            .mount(&server)
            .await;
        check_at(&updates.cache, &client, &server.uri()).await;
        let failed = updates.status().await;
        assert_eq!(
            failed.diagnostic.as_ref().unwrap().failure.code,
            crate::failure::FailureCode::UpdateManifest
        );
        assert_eq!(failed.checked_at_epoch, Some(100));
        assert!(!serde_json::to_string(&failed).unwrap().contains("secret"));
        server.reset().await;
        Mock::given(method("GET")).respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"tag_name":"v0.9.2","html_url":"https://github.com/silenthooligan/localsky/releases/tag/v0.9.2"})))
            .mount(&server).await;
        check_at(&updates.cache, &client, &server.uri()).await;
        let healthy = updates.status().await;
        assert_eq!(healthy.latest.as_deref(), Some("v0.9.2"));
        assert!(healthy.diagnostic.is_none());
        assert_eq!(healthy.checked_at_epoch, healthy.attempted_at_epoch);
    }

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
