// /api/v1/info - service + API version metadata.
//
// The first endpoint a third-party client (HACS integration, MQTT bridge,
// curl smoke test) hits to confirm it's talking to LocalSky and to
// detect the API contract version. SemVer on `api_version`:
//   major  - breaking change to any response shape or required field
//   minor  - additive field on a response, new endpoint
//   patch  - bug fix to data correctness, no shape change
//
// Bumping requires editing API_VERSION below + adding the migration note
// to docs/api.md.

use axum::{response::Json, routing::get, Router};
use serde::{Deserialize, Serialize};

/// Semantic version of the /api/v1 contract. Increment major on any
/// response-shape break. 1.2.0 adds the `dry_run` and `demo` flags
/// so dashboards can surface a banner when the morning scheduler is
/// silenced (otherwise it logs dispatch but never waters, and the
/// operator has no signal that something's wrong). 1.3.0 adds the
/// additive `zone_verdicts` array (per-zone watering verdicts) to the
/// irrigation snapshot. 1.4.0 adds the additive `GET /devices` endpoint
/// (the MA-style device topology: gateways/controllers + their children).
/// 1.5.0 adds `GET /devices/discover` (native LAN gateway discovery).
pub const API_VERSION: &str = "1.5.0";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Info {
    /// Always "localsky".
    pub service: &'static str,
    /// Crate version from Cargo.toml. Surfaces the running build to
    /// integrators (HACS, MQTT, etc.) so they can compare against the
    /// minimum-required version they were built for.
    pub service_version: &'static str,
    /// SemVer of the /api/v1 contract.
    pub api_version: &'static str,
    /// Where /api/v1 is mounted. Always "/api/v1". Lets a client confirm
    /// it followed the right prefix when discovering the service through
    /// mDNS or a manual host:port entry.
    pub api_prefix: &'static str,
    /// Apache-2.0. Surfaced so the client UI can attribute properly.
    pub license: &'static str,
    /// Where to file bugs / read docs.
    pub repository: &'static str,
    /// True when LOCALSKY_SMART_DRY_RUN=1. In this mode the smart-morning
    /// scheduler logs every dispatch it WOULD have made but never calls
    /// the controller; zones stay closed. The dashboard surfaces a banner
    /// so the operator notices that "nothing happens at 6 AM" is
    /// intentional, not a regression.
    pub dry_run: bool,
    /// True when LOCALSKY_DEMO=1. Synthetic weather feed, no live
    /// pollers, controllers in record-only mode. Surfaced for the same
    /// reason as dry_run so deployed-demo instances are visually
    /// distinct.
    pub demo: bool,
}

pub fn router() -> Router {
    Router::new().route("/info", get(info))
}

fn env_flag(name: &str) -> bool {
    std::env::var(name).ok().as_deref() == Some("1")
}

async fn info() -> Json<Info> {
    Json(Info {
        service: "localsky",
        service_version: env!("CARGO_PKG_VERSION"),
        api_version: API_VERSION,
        api_prefix: "/api/v1",
        license: "Apache-2.0",
        repository: "https://github.com/silenthooligan/localsky",
        dry_run: env_flag("LOCALSKY_SMART_DRY_RUN"),
        demo: env_flag("LOCALSKY_DEMO"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn info_endpoint_returns_expected_shape() {
        let Json(body) = info().await;
        assert_eq!(body.service, "localsky");
        assert_eq!(body.api_prefix, "/api/v1");
        assert_eq!(body.license, "Apache-2.0");
        // API_VERSION must be semver-shaped.
        let parts: Vec<&str> = body.api_version.split('.').collect();
        assert_eq!(parts.len(), 3, "expected MAJOR.MINOR.PATCH");
        for p in parts {
            p.parse::<u32>().expect("each component must parse as u32");
        }
    }
}
