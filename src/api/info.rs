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
/// 1.14.0 (additive): GET /config/source_catalog + POST /config/field_sources
/// (per-field priority + backup-chain editor), the `__renames` PUT /config
/// hint (rename a source/controller id), and the corrected sources[].status
/// taxonomy on /health (a fresh but outranked source now reads `standby`, not
/// `falling_through`, which is reserved for a source that previously owned a
/// field and no longer does). All additive or bug-fix; no response-shape break.
/// 1.15.0 (additive): manifest schema 1.3: optional `group` sub-device hint
/// on entity descriptors (files the forecast scalars under the Forecast
/// device), the force_overrode_guard sensor, and capability-gated flow/leaf
/// publishing (no phantom always-0 sensors on installs without the hardware).
/// 1.16.0 (additive): forecast failover + extended model variables.
/// ForecastSnapshot gains `source_is_backup` (failover provenance) and the
/// extended Open-Meteo series on daily (precip_hours, rain/showers/snow
/// split, sunshine_s, apparent_temp_max_f, cape_max_jkg, et0_in) + hourly
/// (et0_in, vpd_kpa, soil moisture 3-9/9-27cm, soil_temp_6cm_f, gusts,
/// snowfall_in). IrrigationSnapshot.forecast gains eto_spent_today_mm,
/// vpd_now/max_today_kpa, and the model-soil advisory scalars. Config gains
/// `seeded_source_ids` (region-authority upgrade seeding tombstones).
/// 1.17.0: lightning honesty. `tempest.lightning_avg_dist_mi` is now
/// NULLABLE and null whenever the reporting interval detected no strikes;
/// it previously published the station's bare 0, which on a distance
/// channel reads as a strike directly overhead. The field is still always
/// present, and null is the documented unknown value, so this is a minor
/// rather than a break; a client that treated 0 as a real distance was
/// already reading a defect. Additive alongside it: the
/// `last_strike_distance_mi` sensor descriptor (the distance that persists
/// between strikes). Also a correctness fix with no shape change:
/// `lightning_strikes_last_hour` now decays as strikes age out of the hour
/// instead of holding the last storm's total until the next strike.
/// 1.18.0: honest unknowns. NULLABLE (null = unknown, never a sentinel
/// zero): tempest.pop_pct + leaf_wetness_pct (until a source writes them),
/// irrigation.water_level_pct (controller reports no level; was a
/// fabricated 100/0), forecast.eto_today_mm (the flat 5.0 fallback no
/// longer publishes), forecast.temp_max_today_f / temp_min_today_f /
/// humidity_mean_today_pct (forecast-first resolution, null when absent),
/// and the precipitation probabilities (skip_check.rain_tomorrow_prob_pct,
/// forecast.rain_tomorrow_prob_pct, seven_day_verdicts[].
/// precip_probability_max; probability-less rain now weights at full value
/// in the rollups instead of zeroing). Additive:
/// irrigation.water_level_capable. Manifest schema 1.4 capability-gates
/// pop_pct, the station-only scalars, water_level_pct, and per-zone soil
/// entities. Minor, not a break: every field is still present and null is
/// the documented unknown; a client that treated the old 0 as data was
/// already reading a defect (the 1.17.0 precedent).
/// 1.19.0 (additive): the per-zone tuning report. GET
/// /irrigation/tuning?days=N (clamp 7..=30, default 14) returns
/// TuningReport {generated_epoch, window_days, zones[], scorecard}: at
/// most one plain-language recommendation per zone with its evidence,
/// plus the install-wide forecast-skip scorecard. POST
/// /config/zones/apply writes one recommendation through the validated
/// config path (privileged like every config write; 409 when the
/// recommendation no longer derives from current data). Every count that
/// lacks data is null, never a zero sentinel (the 1.18.0 register). No
/// existing response shape changes.
/// 1.20.0 (additive): the per-zone run limit becomes real config.
/// ZoneConfig.max_run_minutes (whole minutes, 5..=360, null = the 60
/// minute default) rides GET/PUT /config and the config schema, and
/// hot-reloads on save. The tuning report gains the max_run_minutes
/// recommendation kind (suggested_value in MINUTES; the sessions split
/// is the fallback when the raise would pass 360 or the longer morning
/// no longer fits the pre-sunrise dispatch window), POST
/// /config/zones/apply accepts the field with the same 5..=360 band,
/// and a config write that raises a zone's limit past 60 emits the
/// run-cap-raised Web Push notice after the save. No existing response
/// shape changes.
/// 1.21.0 (additive): the weekly budget becomes a true water balance.
/// WaterBudget rows gain observed_rain_mm + observed_rain_source
/// ('gauge'|'radar'|'model_archive'|'none'), applied_mm,
/// forecast_credit_mm + forecast_credit_source
/// ('bias_forecast'|'none'), bias_multiplier, bias_sample_count, and
/// remaining_sessions; today_seconds keeps its contract (actual
/// seconds to water today) and expected_rain_mm keeps its historical
/// capture-adjusted scaling (weighted 7-day forecast x 25.4 x 0.7).
/// ZoneTuning gains dismissed + dismissed_fields.
/// New endpoints: POST /irrigation/tuning/dismiss {zone_slug, field,
/// recommendation_id, kind: snooze|permanent} and POST
/// /irrigation/tuning/undismiss {zone_slug, field}, privileged like
/// zones/apply; a dismissed recommendation is stripped from the report
/// server-side (no pill, no count, no weekly push). GET
/// /irrigation/history rows gain source + status. Open-Meteo
/// past_days config is honored (clamp 1..=7, default 3). No existing
/// response shape changes.
/// 1.22.0 (additive): Rachio first-class. RachioConfig gains
/// poll_interval_s (60..=3600, null = 120s default; validator-gated)
/// and base_url (null = the production endpoint) in GET/PUT /config.
/// POST /wizard/test_controller for a rachio entry adds
/// discovered_device ({device_id, name, device_count}, null unless the
/// posted entry had a token but no device id) and rate_limit_remaining
/// (the cloud's X-RateLimit-Remaining, null when absent). POST
/// /wizard/scan_zones and /wizard/test_controller now restore
/// redacted-secret sentinels from the stored config by entry id and
/// answer 400 unmatched_redacted_secret when no stored value matches;
/// when a stored secret is restored the probe's transport fields pin
/// to the stored entry (400 transport_field_mismatch on a change).
/// The manual Stop action response gains scope ('zone'|'device') and,
/// for device-wide stops, note. ControllerCaps.per_zone_stop is
/// internal (caps never ride the wire). No existing response shape
/// changes.
/// 1.23.0: controller failures become distinguishable. POST
/// /irrigation/action no longer answers 502 for every controller
/// failure: AuthFailed is 424 FAILED_DEPENDENCY and RateLimited is 429,
/// while 502 keeps Remote/Transport/Offline/Init. ZoneUnknown stays 400
/// and Unsupported stays 501. Deliberately NOT 401 for a vendor
/// credential: 401 on any endpoint is this deploy's OWN auth outcome,
/// and the HACS integration reacts to one by invalidating its stored
/// LocalSky token and starting a reauth flow, so a revoked Rachio key
/// would have sent it into a reauth loop over a valid token. Every error
/// body now carries a stable `code` (zone_unknown,
/// controller_auth_failed, controller_rate_limited,
/// controller_unsupported, controller_unreachable): branch on that, not
/// on the status. A client that treated any non-2xx as one failure is
/// unaffected; one that pinned 502 as the controller-failure status must
/// widen. No HACS integration change is required. Additive alongside it:
/// the 429 body carries rate_limit_remaining (what the controller's LAST
/// RESPONSE reported, null when it reported none, never a zero
/// sentinel), the 400 body carries mapped_zones (the controller's zone
/// map keys, so a slug mismatch is visible from the error), the
/// 400/424/429 bodies carry a hint naming the fix, the ZoneUnknown error
/// text now says the zone is not mapped to a zone on the controller
/// rather than "zone unknown: <slug>", and a successful run/stop/stop_all
/// response carries confirm_within_s (how long that controller can take
/// to REPORT the change, in seconds; null when it reads state on demand)
/// so a client can say a change was accepted and confirmation is still
/// pending.
/// 1.24.0: a zone binds to a controller zone by id. Additive on the
/// wire: ZoneConfig gains `controller_zone_name`, the controller's own
/// name for the bound zone (null when typed by hand or predating this
/// release). It is a LABEL: nothing dispatches on it, nothing keys on
/// it, and a stale value cannot mis-actuate. `controller_station` keeps
/// its shape and gains #[serde(default)] so a config omitting it parses.
/// Behavior: `controller_station` is now the binding a user picks, and a
/// controller's own zone_*_map is the fallback that keeps a pre-existing
/// config watering unchanged. loader::backfill_zone_stations copies a
/// map entry onto any zone of that controller whose station is empty and
/// whose slug the map covers, never overwriting a set station and never
/// touching the map. ha_service_call now READS controller_station and
/// overlays it onto zone_entity_map (entity-id shaped, junk warn-skipped);
/// mqtt_command is exempt (its per-zone value is a struct) and
/// esphome_native still builds nothing. The overlay logs a line whenever
/// a zone entry displaces a DIFFERENT map value, so an HA binding that
/// moves on upgrade is never silent; an identity copy from the backfill
/// stays quiet. New validate warning `zone_unbound`, honest per kind:
/// mqtt_command counts only its zone_command_map (its station field
/// dispatches nothing), dry_run is never unbound (it accepts any slug),
/// and esphome_native reports `zone_controller_not_built` instead. Its
/// companion `zone_station_unparseable` covers a station value the kind
/// cannot use (a Rachio uuid on a Hydrawise zone, an OpenSprinkler 0),
/// which dispatch already ignored and which used to read as bound; the
/// check calls the same per-kind parsers build_controllers binds with, so
/// validation and dispatch cannot disagree. BOTH
/// whole-config write paths (PUT /config and PUT /config/raw) answer 422
/// zone_key_renamed for a save that drops one zone key and adds another,
/// because the slug keys history, overrides, the run ledger, tuning
/// dismissals, the soil channel, HA entity ids and retained MQTT topics;
/// override with ?allow_zone_key_change=1 (also =true/=yes/=on, or the
/// bare flag). The unknown-zone 400 hint stops advising a rename and
/// points at Controller station. No HACS integration change: it gates on
/// the API major and never reads /api/config.
pub const API_VERSION: &str = "1.24.0";

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
