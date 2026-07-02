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
/// 1.6.0 adds `auth_required` + `uuid` here (built-in auth + stable
/// instance identity for HACS/zeroconf pairing) and the /api/v1/auth
/// endpoint family.
/// 1.7.0 (additive): SkipCheck.temp_min_24h_valid, DecisionTrace.degraded,
/// GET /api/v1/config/snapshots + POST rollback {ts}, ha.hacs_streaming;
/// action kind run_sequence_now retired (410 Gone).
/// 1.8.0 (additive): IrrigationSnapshot.soil_probe_faults +
/// /health.soil_probe_faults (configured soil probes with no valid
/// reading for 24h+; non-empty degrades /health status).
/// 1.9.0 (additive): GET /sources/openmeteo/models (forecast model
/// catalog backing sources[].config.model) + GET /radar/windgrid
/// (leaflet-velocity U/V wind grid for the radar map's wind layer).
/// 1.10.0 (additive): GET /radar/tropical (basin-aware tropical
/// cyclone tracking: all verified agency feeds, NHC/CPHC + JMA +
/// JTWC, normalized server-side into one GeoJSON FeatureCollection
/// with per-storm term/agency/basin properties and per-source health).
/// 1.11.0 (additive): GET /sensors/inventory (gateways, soil probes,
/// and flow status: capable vs connected vs live GPM) backing the
/// Sensors view and the wizard sensor step.
/// 1.12.0 (additive): IrrigationSnapshot.global_override (sticky
/// global Auto/Skip/Force override) + forecast.wind_gust_today_mph
/// and the wind_gust_forecast sensor manifest descriptor.
/// 1.13.0 (additive): `has_irrigation` (any controller/zone configured)
/// + `nerd_mode_default` here, so the client can hide irrigation nav on a
/// weather-only install and seed Simple vs Nerd mode from server config
/// instead of hard-coding new users into Nerd mode.
pub const API_VERSION: &str = "1.13.0";

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
    /// True when this instance requires authentication. Integration
    /// clients (HACS) read this on probe and prompt for an API token.
    pub auth_required: bool,
    /// Stable per-install id (also broadcast in the mDNS TXT record).
    /// Lets clients dedupe across IP/host changes. None before first
    /// boot completes init.
    pub uuid: Option<String>,
    /// True when any irrigation hardware is configured (at least one
    /// controller OR at least one zone in localsky.toml). The Wave-2 UI
    /// reads this at app root to HIDE the irrigation nav items on a
    /// weather-only install, so a no-hardware user is not staring at empty
    /// Zones/Irrigation/History doors. False on a fresh/weather-only config.
    /// `#[serde(default)]` so an older payload (pre-1.13.0) still decodes.
    #[serde(default)]
    pub has_irrigation: bool,
    /// The configured `features.nerd_mode_default`. The Wave-2 UI seeds the
    /// initial Simple vs Nerd presentation from this instead of hard-coding
    /// every new user into Nerd mode. Defaults to false (Simple).
    /// `#[serde(default)]` so an older payload (pre-1.13.0) still decodes.
    #[serde(default)]
    pub nerd_mode_default: bool,
}

pub fn router() -> Router {
    Router::new().route("/info", get(info))
}

fn env_flag(name: &str) -> bool {
    std::env::var(name).ok().as_deref() == Some("1")
}

/// Read `has_irrigation` + `nerd_mode_default` from the live config file.
/// Self-contained (no router state) so the info router stays stateless and
/// merge-mounted as-is: it loads localsky.toml from the same CONFIG_PATH the
/// boot path uses. A missing/unparseable config (fresh install) yields the
/// safe defaults (no irrigation, Simple mode), exactly what a weather-only or
/// pre-wizard install should report.
async fn config_signals() -> (bool, bool) {
    use crate::ports::config_store::ConfigStore;
    let path = std::env::var("CONFIG_PATH").unwrap_or_else(|_| "/data/localsky.toml".to_string());
    match crate::config::FileConfigStore::new(&path).load().await {
        Ok(cfg) => {
            let has_irrigation = !cfg.controllers.is_empty() || !cfg.zones.is_empty();
            (has_irrigation, cfg.features.nerd_mode_default)
        }
        Err(_) => (false, false),
    }
}

async fn info(req: axum::http::Request<axum::body::Body>) -> Json<Info> {
    let auth_required = req
        .extensions()
        .get::<crate::auth::middleware::AuthRequired>()
        .map(|a| a.0)
        .unwrap_or(false);
    let (has_irrigation, nerd_mode_default) = config_signals().await;
    Json(Info {
        service: "localsky",
        service_version: env!("CARGO_PKG_VERSION"),
        api_version: API_VERSION,
        api_prefix: "/api/v1",
        license: "Apache-2.0",
        repository: "https://github.com/silenthooligan/localsky",
        dry_run: env_flag("LOCALSKY_SMART_DRY_RUN"),
        demo: env_flag("LOCALSKY_DEMO"),
        auth_required,
        uuid: crate::instance::get().map(str::to_string),
        has_irrigation,
        nerd_mode_default,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn info_endpoint_returns_expected_shape() {
        let req = axum::http::Request::new(axum::body::Body::empty());
        let Json(body) = info(req).await;
        assert_eq!(body.service, "localsky");
        assert_eq!(body.api_prefix, "/api/v1");
        assert_eq!(body.license, "Apache-2.0");
        // API_VERSION must be semver-shaped.
        let parts: Vec<&str> = body.api_version.split('.').collect();
        assert_eq!(parts.len(), 3, "expected MAJOR.MINOR.PATCH");
        for p in parts {
            p.parse::<u32>().expect("each component must parse as u32");
        }
        // The Wave-2 signals are present. With no config file on disk in the
        // test environment they fall back to the safe weather-only defaults.
        assert!(
            !body.has_irrigation,
            "no config -> weather-only (has_irrigation=false)"
        );
        assert!(
            !body.nerd_mode_default,
            "no config -> Simple mode default (nerd_mode_default=false)"
        );
    }
}
