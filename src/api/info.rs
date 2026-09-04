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
/// 1.25.0: the engine stops reading Home Assistant, and the soil deficit
/// stops being fabricated. NULLABLE, following the 1.18.0 honest-unknowns
/// precedent exactly (null = unknown, never a sentinel): zones[].bucket_mm,
/// zones[].math.bucket_mm and zones[].today_run_minutes. The bucket fields'
/// only producer was the Home Assistant entity
/// sensor.smart_irrigation_<slug>, which the engine no longer reads for any
/// purpose, so on every install the old bare f64 published a hardcoded 0.0
/// as if it were a measurement; today_run_minutes has no producer on any
/// install (nothing sums a zone's valve-open minutes since local midnight)
/// and is null everywhere. Minor, not a break: the fields are still
/// present, null is the documented unknown, and a client that treated the
/// old 0.00 as data was reading a defect. Manifest schema 1.5
/// capability-gates the per-zone <slug>_soil_bucket descriptor on the value
/// being present, the same rule water_level_pct and the soil quartet already
/// take, and <slug>_run_today on today_run_minutes, so no permanently
/// unavailable entity is registered and the run-today sensor is no longer
/// registered at all; MQTT
/// zone discovery gates the same way AND clears the retained config/state
/// topics it used to publish, so a broker holding the old 0.00 drops it
/// and HA removes the entity instead of pinning it. NO HACS INTEGRATION
/// CHANGE IS
/// REQUIRED: it builds entities from the manifest, a manifest-driven value
/// of null already reads unavailable, and a descriptor that stops being
/// advertised is the documented capability-gate behavior. Additive:
/// zones[].smart_suppressed { weekdays, schedules, active_today }, set
/// when an enabled Override manual schedule suppresses smart dispatch for
/// that zone (display only; the suppression itself is unchanged).
/// Behavior, no shape change: Kc in zones[].math.kc now comes from the
/// native species catalog rather than the SI entity's multiplier;
/// water_budgets[] no longer lets HA input_number helpers outrank
/// LocalSky's own weekly_budget_in / sessions_per_week; the 24h rain-defer
/// gate weights forecast rain by probability and reads
/// engine.session_rain_defer_in instead of a compile-time constant;
/// zones[].math.cap_binding reports that the ceiling set tonight's minutes
/// (scheduled_seconds sits at max_duration_seconds and some stage wanted
/// more: the allocator's session, the seasonal dial, or a condition-rule
/// multiplier), and is false whenever no run is planned. It was previously
/// raw_seconds > max_duration_seconds off the Smart Irrigation soil
/// deficit: live on a Home Assistant install carrying that entity, always
/// false on standalone.
/// A smart-morning dispatch that fails writes a skipped run row with the
/// controller's error text, excluded from the boot dedupe so a restart
/// inside the catch-up window can still water a morning whose every row is
/// a dispatch failure (a verdict-skip row still marks the morning
/// handled). New validation error zone_sessions_per_week_range;
/// PATCH of a zone sessions_per_week outside 1..=7 is refused.
/// Additive: ha_adoption[] on the irrigation snapshot (and in localsky.toml),
/// one entry per retired Home Assistant helper carrying entity, outcome,
/// target, the value taken and the value it replaced. Empty on every
/// standalone install and omitted from the config file until first used; the
/// migration notice renders it. Behavior, no shape change: POST /action
/// set_threshold writes engine.skip_rules rather than an input_number helper
/// once that helper is retired (range-checked 0..50 mph, 20..70 F, 0..10 in),
/// and toggle writes LocalSky's own control store rather than an
/// input_boolean; both keep their request and response shapes, and both still
/// write the helper on a Home Assistant deployment that has not retired it
/// yet, because until then that is what the engine reads.
/// skip_check.max_wind_mph / min_temp_f / rain_skip_in stop reflecting a live
/// input_number helper once it is retired and report engine.skip_rules.
/// override_helpers_present keeps its shape and now means "the pause and
/// override controls will land somewhere", which is always true once they are
/// retired. Additive controls_persisted: true when a persistence database is
/// mounted, so the migration notice can tell "the four controls have nowhere
/// to land here" apart from "a control was not answering when the pass
/// looked". Absent reads false, which is the no-database wording. Additive
/// ha_adoption_awaiting_config: true while the pass cannot run because there
/// is no localsky.toml to record it in, so every helper read is still live;
/// absent reads false. Additive observed_value on an ha_adoption[] entry: what the helper
/// held when it sat outside the range LocalSky can represent and was adopted
/// at the nearest end. Manifest schema 1.6 adds min/max/step to `number`
/// descriptors: the integration should build the three threshold entities on
/// those, because set_threshold now answers 400 outside 0..50 mph, 20..70 F
/// and 0..10 in. The shipping integration builds them from fixed limits of
/// its own (0..50 mph, 20..60 F, 0..1 in), all inside the server's, so no
/// slider value is refused; a write outside the server's range from any
/// other client is refused there.
/// 1.26.0: single-day rain stops out-crediting the soil. Minor, following
/// the 1.25.0 precedent. Additive fields on water_budgets[] rows:
/// observed_rain_credited_mm (the trailing rain the balance actually
/// offset, each day held to the cap before summing; equals
/// observed_rain_mm whenever no day clipped), rain_credit_cap_mm (the
/// per-day cap in effect, mm; 0 on JSON from an older producer =
/// unknown/legacy), and rain_cap_inferred (true when the cap was derived
/// from soil texture and root depth rather than set). observed_rain_mm
/// keeps carrying the RAW trailing sum. Behavior change with no shape
/// change: each observed day and each forward forecast-credit day is
/// capped at the zone's root-zone capacity (TAW = (field capacity -
/// wilting point) x root depth), so a single day's rain beyond what the
/// root zone can bank no longer credits the weekly balance and
/// today_seconds / today_reason move for storm weeks; the covered reason
/// string is unchanged whenever no day clipped. New ZoneConfig field
/// rain_credit_cap_in (inches, 0.05..=5.0, null = derived) rides GET/PUT
/// /config and the config schema; new validation error
/// zone_rain_credit_cap_range; POST /config/zones/apply accepts the
/// field with the same band; a value already on disk is clamped at load.
/// No existing response shape changes.
/// 1.27.0: the soil scheduling model. Minor, following the 1.25.0
/// precedent: additive fields, and a behavior change only for zones
/// opted into the soil model. zones[].bucket_mm and math.bucket_mm gain
/// their producer (the soil model's evidence replay; negative = needs
/// water; still null where no bucket can be derived), so the manifest's
/// gated <slug>_soil_bucket descriptor and the MQTT bucket sensor
/// publish again on zones with agronomy config. Additive fields on
/// water_budgets[] rows: scheduling_model ("weekly" | "soil"; empty on
/// older JSON), soil_depletion_mm / soil_taw_mm / soil_raw_mm (null
/// where no bucket), soil_due, soil_planned_seconds (shadow figure
/// under weekly; under soil the POST-admission plan, window admission
/// applied, 0 when deferred with soil_deferred_reason carrying the
/// hold), soil_deferred_reason, soil_ceiling_binding. New config:
/// engine.scheduling_model (absent = never chosen, follows the shipped
/// default, weekly today, and the key is omitted from GET while unset
/// so a round-tripped body cannot stamp the default in as a choice;
/// the wizard writes soil for new installs) and
/// ZoneConfig.scheduling_model (null = engine default), both on
/// GET/PUT /config, the schema, and the per-field apply. Weekly-governed rows are byte-identical to 1.26.0 apart from
/// the additive fields and ONE declared value change: the run-evidence
/// fetch widened to the soil replay window, so last_run_epoch (the
/// water_budgets row, zones[].last_run_epoch, and the zone detail's
/// last-ran line) now populates for a zone whose newest run ended 8-15
/// days ago, where the 7-day fetch read 0. Planned seconds, reasons,
/// session spacing, and forecast credit are unchanged (min interval is
/// at most 7 days). A soil-governed zone's today_seconds /
/// today_reason / session_capped come from the soil plan.
/// New endpoints (registered only when the history database is
/// mounted): GET /irrigation/soil-invite answers whether the soil
/// opt-in offer shows on this install ({eligible}; when eligible also
/// state "open" | "snoozed" | "dismissed", until_epoch, and the
/// shadow_zones / deficit_zones / differs_today counts the popup
/// names), and POST /irrigation/soil-invite/dismiss {kind: "snooze" |
/// "permanent"} records the choice server side (privileged, same
/// posture as tuning/dismiss). No existing response shape changes.
pub const API_VERSION: &str = "1.27.0";

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
