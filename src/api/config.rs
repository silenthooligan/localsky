// /api/config router. Reads + writes /data/localsky.toml via FileConfigStore.
//
// Endpoints:
//   GET  /api/config              -> current Config, secrets replaced with
//                                    SECRET_REDACTED_SENTINEL by redact_secrets()
//   PUT  /api/config              -> validate + save; restores any field still
//                                    set to the sentinel from the stored value
//                                    via unredact_secrets() so partial edits work
//   GET  /api/config/schema       -> JsonSchema for the settings UI forms
//   POST /api/config/preview      -> dry-run validation against a candidate
//   GET  /api/config/snapshots    -> file snapshots (<config_dir>/snapshots)
//   POST /api/config/rollback     -> {"ts": <snapshot ts>} restore (also
//                                    accepts legacy ?to=<ts>)
//
// Not wired into the main api router yet. Phase 5 composition root passes
// a constructed FileConfigStore via state.

use std::sync::Arc;

use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::{get, post},
    Router,
};
use schemars::schema_for;
use serde::{Deserialize, Serialize};
use tower_http::limit::RequestBodyLimitLayer;

use crate::config::schema::Config;
use crate::config::FileConfigStore;
use crate::ports::config_store::{ConfigStore, ConfigStoreError};
use crate::runtime::RuntimeHandles;

/// State for the /api/config router: the config store plus the live runtime
/// handles a save re-applies to the running engine. `runtime` is `None` only
/// in unit tests + the demo posture where no live engine is wired; in that case
/// a save persists but does not hot-reload (there is nothing to reload into).
#[derive(Clone)]
pub struct ConfigApiState {
    pub store: Arc<FileConfigStore>,
    pub runtime: Option<RuntimeHandles>,
}

impl ConfigApiState {
    /// Construct from a store with no live runtime (tests / demo).
    pub fn store_only(store: Arc<FileConfigStore>) -> Self {
        Self {
            store,
            runtime: None,
        }
    }
}

/// Re-apply the engine-tunable subset of a freshly-saved config to the live
/// system when a runtime is wired; a no-op (default outcome) otherwise. Shared
/// by PUT /api/config and PUT /api/config/raw so both hot-reload identically.
fn apply_runtime_config_if_live(
    runtime: &Option<RuntimeHandles>,
    prev: Option<&Config>,
    new_cfg: &Config,
) -> crate::runtime::ConfigApplyOutcome {
    match runtime {
        Some(h) => crate::runtime::apply_runtime_config(h, prev, new_cfg),
        // No live engine wired (tests / demo posture): the save persisted but
        // there is nothing to reload into. Report no restart requirement.
        None => crate::runtime::ConfigApplyOutcome::default(),
    }
}

/// Upper bound on a config write (LS-API-09). A full localsky.toml with
/// many zones/sources/rules is a few tens of KiB at most; 2 MiB is a
/// comfortable ceiling that still refuses an over-large body before it is
/// buffered. Applies to PUT / (JSON), PUT /raw (TOML text), POST /preview
/// and POST /rollback. The route is privileged-gated already; this cap is
/// defense-in-depth.
const CONFIG_BODY_LIMIT: usize = 2 * 1024 * 1024;

pub fn router(state: ConfigApiState) -> Router {
    Router::new()
        .route("/", get(get_config).put(put_config))
        .route("/validate", get(get_validate))
        .route("/schema", get(get_schema))
        .route("/preview", post(preview_config))
        .route("/snapshots", get(get_snapshots))
        .route("/rollback", post(post_rollback))
        .route("/raw", get(get_raw_toml).put(put_raw_toml))
        .route("/field_sources", get(get_field_sources))
        .route("/source_catalog", get(get_source_catalog))
        .with_state(state)
        .layer(RequestBodyLimitLayer::new(CONFIG_BODY_LIMIT))
}

/// One configured source as a candidate per-field owner, for the Data sources
/// settings picker.
#[derive(Debug, Serialize)]
struct FieldSourceCandidate {
    /// Source id (the override value the UI PUTs into `field_source_overrides`).
    id: String,
    /// Human label for the picker (id today; kept distinct so a future
    /// friendly-name lookup is a one-line change).
    label: String,
    /// True for a real live station (drives current conditions), false for a
    /// cloud weather service. A source only appears here when it emits a CURRENT
    /// scalar for at least one field (so its `fields` is non-empty); whether it
    /// is a physical device or a cloud service is what `tier` distinguishes.
    live_current: bool,
    /// Source-tier taxonomy for the picker, one of:
    ///   "device": a local physical sensor on the network (live_current=true),
    ///   "cloud": a cloud weather service that emits a CURRENT scalar for a
    ///            field it covers (Open-Meteo, NWS, OpenWeather, Met.no,
    ///            WeatherKit). A cloud source that provides current conditions
    ///            for a field is "cloud", NOT "forecast".
    /// (The "forecast" tier, a source that only forecasts a field, never appears
    /// in THIS list: a candidate is here precisely because it emits a current
    /// scalar. Forecast-only sources are surfaced via `forecast_candidates`.)
    tier: &'static str,
    /// Canonical source kind string (open_meteo / nws / openweather / ...), so
    /// the picker can look up the shared plain-language descriptor for a cloud
    /// service at the point of choice.
    kind: &'static str,
    /// Canonical WeatherField names this source can own (snake_case, matching
    /// `field_source_overrides` keys + the snapshot's `field_sources`).
    fields: Vec<&'static str>,
    /// The researched per-field merge priority this source's KIND would seed at
    /// the deployment location (`region::default_priority_for`). Higher wins. The
    /// client sorts the candidates by this DESC to render the region-DEFAULT chain
    /// order ("Automatic") for a field before the user has reordered it, so an
    /// un-edited field shows exactly the order LocalSky would arbitrate by.
    region_priority: i32,
    /// The HONEST data nature of this source, so the client can badge each chain
    /// entry measured-vs-model at the point of choice:
    ///   * "device"       -> a local physical sensor on the network (measured),
    ///   * "observation"  -> a cloud service that MEASURES the field (NWS gauges),
    ///   * "radar_qpe"    -> gauge-corrected radar rainfall (NOAA MRMS),
    ///   * "nowcast"      -> a real-time cloud analysis (Pirate temp/wind),
    ///   * "forecast"     -> a model forecast (Open-Meteo, Met.no, ...).
    /// The cloud values are exactly the snake_case `CloudDataNature` wire strings
    /// the cloud-onboarding UI already matches on (measured = observation/radar_qpe,
    /// model = forecast); a source with no cloud metadata falls back to "device"
    /// (it is a live sensor) so the field is always present.
    ///
    /// This is the SOURCE-LEVEL headline nature (a single value for the whole
    /// source). It is a FALLBACK: the honest badge is PER FIELD (`field_natures`),
    /// because a cloud source can be a live nowcast for one field and a model
    /// forecast for another in the SAME row (Pirate temp = nowcast, Pirate rain =
    /// forecast). The client uses `field_natures` for the field it is rendering and
    /// falls back to this flat value only when a per-field entry is absent.
    nature: String,
    /// The HONEST PER-FIELD data nature, one `(field_name, nature)` pair per field
    /// this source can own (over EXACTLY `fields`, same snake_case keys). Each
    /// nature is the same snake_case `CloudDataNature` wire string the flat
    /// `nature` uses (device / observation / radar_qpe / nowcast / forecast). This
    /// is the per-CELL truth the single `nature` cannot express: Pirate under Rain
    /// resolves "forecast" while Pirate under Temperature resolves "nowcast". For a
    /// live device every field is "device"; for a cloud source each field takes its
    /// `CloudSourceMeta::field_nature` (rain keys track `rain_nature`, the rest the
    /// headline plus per-kind overrides). Serializes as an array of
    /// `[field, nature]` two-tuples so the client badges each chain row by the
    /// field it renders, falling back to the flat `nature` if a field is absent.
    field_natures: Vec<(String, String)>,
}

/// One forecast-capable source the "Forecast source" picker can pin as the
/// provider that drives the whole forecast pipeline (daily/hourly arrays, ET0,
/// rain-tomorrow). Distinct from the per-field `FieldSourceCandidate`: a
/// forecast is arbitrated whole-snapshot, not per-field.
#[derive(Debug, Serialize)]
struct ForecastCandidate {
    /// Source id (the value the UI PUTs into `forecast_provider`).
    id: String,
    /// Human label for the picker (id today; a future friendly-name lookup is a
    /// one-line change, matching FieldSourceCandidate).
    label: String,
    /// Source kind tag (open_meteo / nws / met_norway / openweather /
    /// pirate_weather / weatherkit) so the UI can show a pretty kind name.
    kind: &'static str,
}

/// What the Data sources page renders against: the user-relevant fields it
/// offers a picker for, every enabled source that can provide each, and the
/// current overrides. The page derives the live owner from the irrigation
/// snapshot's `field_sources`, so this read is config-shaped + cacheable.
#[derive(Debug, Serialize)]
struct FieldSourcesResponse {
    /// (field_name, display label) for each user-facing field, in display order.
    user_fields: Vec<(&'static str, &'static str)>,
    /// Enabled sources + the fields each provides.
    sources: Vec<FieldSourceCandidate>,
    /// Current `field_source_overrides` (field_name -> source id), echoed so the
    /// page renders the saved selection without a second round-trip.
    overrides: std::collections::BTreeMap<String, String>,
    /// Current `field_source_chains` (field_name -> ORDERED list of source ids),
    /// echoed so the page renders the saved custom chain per field. A field ABSENT
    /// here (and absent from `overrides`) has no user chain: the client renders the
    /// region-DEFAULT order by sorting that field's candidate `sources` on
    /// `region_priority` DESC ("Automatic"). A field PRESENT here renders exactly
    /// this saved order ("Custom"). The single pin in `overrides` is the special
    /// case of a one-element chain and the two never both apply to a field.
    field_source_chains: std::collections::BTreeMap<String, Vec<String>>,
    /// Enabled FORECAST-capable sources, the candidates for the "Forecast
    /// source" picker. Empty when no forecast source is configured (the
    /// out-of-the-box default synthesizes an Open-Meteo entry, so this normally
    /// has at least one entry).
    forecast_candidates: Vec<ForecastCandidate>,
    /// The saved `forecast_provider` pin (a source id) or null for "Auto (by
    /// priority)". Echoed so the picker renders the saved selection.
    forecast_provider: Option<String>,
    /// A short human region label for the deployment location ("US" / "Europe" /
    /// "Global"), resolved from the deployment lat/lon via `region::region_for`.
    /// The chain editor tags an un-edited field "Automatic (<region> default)" so
    /// "where does the automatic order come from" is answered per region without a
    /// second round-trip. Empty is never sent (the fn always returns a label).
    region_label: &'static str,
}

/// Short human region label for the chain editor's "Automatic (<region> default)"
/// tag, from the coarse default-ranking region the priorities are seeded against.
fn region_label_for(lat: f64, lon: f64) -> &'static str {
    match crate::config::region::region_for(lat, lon) {
        crate::config::region::Region::Us => "US",
        crate::config::region::Region::EuropeNordic => "Europe",
        crate::config::region::Region::Global => "Global",
    }
}

/// Source-tier taxonomy for the per-field picker. The tier a source carries for
/// a field depends on whether it is a local physical sensor and whether it emits
/// a CURRENT value for that field:
///   "device":   a local physical sensor on the network (live_current=true): it
///               drives current conditions directly (Tempest, Ecowitt, Davis,
///               Netatmo, YoLink, ...).
///   "cloud":    a cloud weather service that emits a CURRENT scalar for the
///               field (Open-Meteo, NWS, OpenWeather, Met.no, WeatherKit). This
///               is the key classification: a cloud source providing current
///               conditions for a field is a usable CURRENT source, not a
///               forecast-only one.
///   "forecast": a source that only forecasts the field, with no current scalar.
///
/// `emits_current` is true when the source emits a current scalar for the field
/// in question (the per-field picker only ever lists sources for which this is
/// true, so callers there pass true). A source that emits no current scalar for
/// the field is tier "forecast".
fn source_field_tier(live_current: bool, emits_current: bool) -> &'static str {
    if live_current {
        "device"
    } else if emits_current {
        "cloud"
    } else {
        "forecast"
    }
}

/// The HONEST data nature of a source for the per-field chain badge: a local
/// physical sensor (`live_current`) is a measured "device"; a cloud service takes
/// its `CloudDataNature` (observation / radar_qpe / nowcast / forecast) from the
/// cloud catalog, serialized to the SAME snake_case wire string the cloud UI
/// matches on. A cloud kind with no catalog metadata (or a device that is not
/// forecast-capable) falls back to "device" so the field is always present.
fn source_nature(kind: &crate::config::schema::SourceKind, live_current: bool) -> String {
    if live_current {
        return "device".to_string();
    }
    match crate::sources::cloud_catalog::cloud_meta(kind) {
        Some(meta) => serde_json::to_value(meta.data_nature)
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_else(|| "device".to_string()),
        None => "device".to_string(),
    }
}

/// The HONEST PER-FIELD data nature of a source for ONE canonical field key (the
/// snake_case `field_overrides::field_name`, e.g. "air_temp_f", "rain_today_in"),
/// as the SAME snake_case `CloudDataNature` wire string `source_nature` emits. A
/// local physical sensor (`live_current`) MEASURES every field, so each is
/// "device". A cloud source asks the catalog PER FIELD via
/// `CloudSourceMeta::field_nature`: rain keys track the honest `rain_nature`, the
/// rest the headline `data_nature` plus per-kind overrides, so Pirate's
/// "air_temp_f" reads "nowcast" while its "rain_today_in" reads "forecast" in the
/// SAME source. A cloud kind with no catalog metadata falls back to the source
/// headline `source_nature` (which is "device" when there is none) so the field
/// always resolves.
fn field_nature_for(
    kind: &crate::config::schema::SourceKind,
    live_current: bool,
    field: &str,
) -> String {
    if live_current {
        return "device".to_string();
    }
    match crate::sources::cloud_catalog::cloud_meta(kind) {
        Some(meta) => serde_json::to_value(meta.field_nature(field))
            .ok()
            .and_then(|v| v.as_str().map(str::to_string))
            .unwrap_or_else(|| source_nature(kind, live_current)),
        None => source_nature(kind, live_current),
    }
}

/// Kind-aware OBSERVATION-liveness window (seconds) for the catalog's honest
/// status, MIRRORING the SAME per-kind mapping /api/health uses (health.rs
/// `obs_alive_window_s`): a polled forecast model / MRMS refreshes every 10-30
/// min, so a healthy one between polls (or one stably reachable but only
/// observing on its own slow cadence) must NOT read `offline`; the LaCrosse
/// cloud station polls hourly. Every other kind falls back to the 30-min
/// reachability window (health's `HARD_OFFLINE_WINDOW_S`, inlined here since it
/// is private to that module). Keeping this congruent with /api/health is what
/// makes a recently-observing source read its calm status on BOTH surfaces.
fn catalog_obs_alive_window_s(kind: &crate::config::schema::SourceKind) -> i64 {
    use crate::config::schema::SourceKind;
    match kind {
        SourceKind::OpenMeteo(_)
        | SourceKind::Nws(_)
        | SourceKind::OpenWeather(_)
        | SourceKind::PirateWeather(_)
        | SourceKind::MetNorway(_)
        | SourceKind::WeatherKit(_)
        | SourceKind::Netatmo(_)
        | SourceKind::NoaaMrms(_) => 10800,
        SourceKind::Lacrosse(_) => 3600,
        // Mirrors health.rs's private HARD_OFFLINE_WINDOW_S (30 min) fallback.
        _ => 1800,
    }
}

/// GET /api/config/field_sources -> the per-field source picker dataset PLUS
/// the forecast-source picker candidates.
async fn get_field_sources(
    State(ConfigApiState { store, .. }): State<ConfigApiState>,
) -> impl IntoResponse {
    let cfg = store.load().await.unwrap_or_default();
    // The deployment location the region-default priority ordering is resolved
    // against (same lat/lon `source_catalog` uses), so the per-field candidates
    // carry the region-DEFAULT chain order the client renders as "Automatic".
    let lat = cfg.deployment.location.lat;
    let lon = cfg.deployment.location.lon;
    // Per-field candidates. A source appears here when `source_field_names`
    // returns a non-empty CURRENT-scalar set for it, so every entry is a real
    // current-conditions owner for the fields it lists (an override only fills a
    // field when no fresher source owns it, so listing one is safe).
    //
    // The cloud weather services (open_meteo, nws, openweather, met_norway,
    // pirate_weather, weatherkit) appear here too: their adapters declare scalar
    // current fields (AirTempF, WindMph, ...) in capabilities, AND Open-Meteo's
    // refresher ingests the `current=` block to emit live scalars into the merge.
    // The KEY taxonomy fix: a cloud source that provides a CURRENT value for a
    // field is tier "cloud" (a usable current source), NOT "forecast". Only a
    // real local physical sensor (live_current=true) is tier "device". This is
    // what `source_tier` computes below, replacing the old blanket "(forecast)"
    // label that made a usable cloud current source read as forecast-only.
    let sources = cfg
        .sources
        .iter()
        .filter(|e| e.enabled)
        .map(|e| {
            // A local physical sensor on the network drives current conditions
            // directly; matches the adapters' capabilities().live_current. Every
            // cloud weather service (forecast-capable or vendor cloud station)
            // reports false here.
            let live_current = !e.source.is_forecast()
                && matches!(
                    e.source,
                    crate::config::schema::SourceKind::TempestUdp(_)
                        | crate::config::schema::SourceKind::TempestWs(_)
                        | crate::config::schema::SourceKind::EcowittLocal(_)
                        | crate::config::schema::SourceKind::EcowittGwPoll(_)
                        | crate::config::schema::SourceKind::DavisWll(_)
                        | crate::config::schema::SourceKind::AmbientWeather(_)
                        | crate::config::schema::SourceKind::Netatmo(_)
                        | crate::config::schema::SourceKind::Yolink(_)
                        | crate::config::schema::SourceKind::Lacrosse(_)
                        | crate::config::schema::SourceKind::TuyaCloud(_)
                        | crate::config::schema::SourceKind::HaPassthrough(_)
                        | crate::config::schema::SourceKind::Mqtt(_)
                        | crate::config::schema::SourceKind::HttpWebhook(_)
                        | crate::config::schema::SourceKind::RestPoll(_)
                        | crate::config::schema::SourceKind::Prometheus(_)
                        | crate::config::schema::SourceKind::InfluxDb(_)
                );
            let fields = crate::runtime::source_field_names(&cfg, e);
            // Tier for THIS source, given it is a current owner for `fields`:
            //   live_current=true  -> "device" (a local physical sensor),
            //   live_current=false -> "cloud"  (a cloud service supplying a
            //                                    CURRENT scalar for the field).
            // The "forecast" tier is never reached here: a candidate is present
            // precisely because it emits a current scalar, not a forecast.
            let tier = source_field_tier(live_current, !fields.is_empty());
            // The EFFECTIVE merge priority for THIS source: its CONFIGURED
            // priority (`e.priority`), which is exactly what source_priority_map
            // (runtime.rs) sorts the live merge by. Using this (not the region
            // default, which returns 50 for every local station) is what makes the
            // client's "Automatic" chain order MATCH what the merge actually
            // arbitrates: a station seeded at 100 heads its fields, cloud backs up.
            // (Named region_priority for wire-compat; it carries the effective
            // merge priority.) Plus the honest measured/model nature per field.
            let region_priority = e.priority;
            let nature = source_nature(&e.source, live_current);
            // The HONEST PER-FIELD nature, one entry per field this source owns.
            // So the client badges Pirate under Rain "forecast" while Pirate under
            // Temperature shows "real-time" (nowcast), instead of one blanket
            // source-level badge on every field. Falls back to the flat `nature`
            // per field inside field_nature_for when there is no catalog metadata.
            let field_natures = fields
                .iter()
                .map(|&f| (f.to_string(), field_nature_for(&e.source, live_current, f)))
                .collect();
            FieldSourceCandidate {
                id: e.id.clone(),
                label: e.id.clone(),
                live_current,
                tier,
                kind: crate::config::kind_labels::source_kind_label(&e.source),
                fields,
                region_priority,
                nature,
                field_natures,
            }
        })
        .filter(|c| !c.fields.is_empty())
        .collect();
    // Forecast-source picker candidates: every enabled forecast-capable source.
    let forecast_candidates = cfg
        .sources
        .iter()
        .filter(|e| e.enabled && e.source.is_forecast())
        .map(|e| ForecastCandidate {
            id: e.id.clone(),
            label: e.id.clone(),
            kind: crate::config::kind_labels::source_kind_label(&e.source),
        })
        .collect();
    Json(FieldSourcesResponse {
        user_fields: crate::config::field_overrides::USER_FIELDS.to_vec(),
        sources,
        overrides: cfg.field_source_overrides.clone(),
        field_source_chains: cfg.field_source_chains.clone(),
        forecast_candidates,
        forecast_provider: cfg.forecast_provider.clone(),
        region_label: region_label_for(lat, lon),
    })
}

/// One cloud weather service as the cloud-onboarding UI (Wave B) renders it:
/// the honest per-service facts from `sources::cloud_catalog` PLUS the live
/// per-deployment wiring (which current fields it actually emits, whether it is
/// recommended here, and whether it is already configured + enabled). Flattens
/// the static `CloudSourceMeta` so its fields (kind, data_nature, real_time,
/// localization, watering_risk, key_tier, emits_current_rain, pop_is_synthetic,
/// honesty_rank) sit at the top level alongside the runtime additions below.
#[derive(Debug, Serialize)]
struct CloudCatalogEntry {
    /// The honest static facts (flattened to the top level): `kind`,
    /// `data_nature`, `real_time`, `localization`, `watering_risk`, `key_tier`,
    /// `emits_current_rain`, `pop_is_synthetic`, `honesty_rank`.
    #[serde(flatten)]
    meta: crate::sources::cloud_catalog::CloudSourceMeta,
    /// Canonical WeatherField names (snake_case) this kind emits as LIVE current
    /// scalars into the merge, via `runtime::source_field_names`. Empty only for
    /// a kind that emits no overrideable current scalar; every cloud kind here
    /// emits at least one post-fix. The UI lists what "current conditions" this
    /// option can actually fill.
    live_current_fields: Vec<&'static str>,
    /// The HONEST per-field data nature for EACH field this kind can emit, as
    /// `(canonical_field_key, nature)` pairs over EXACTLY `live_current_fields`
    /// (same keys, same order). The capability matrix reads this to tint each LIT
    /// cell by its own truth: a cell lit from `live_current_fields` shows the
    /// matching nature here. This is the per-CELL refinement the single overall
    /// `data_nature` cannot express, so Pirate's `wind_mph` carries `nowcast`
    /// while its `rain_today_in` carries `forecast` in the SAME row. Each nature
    /// is `CloudSourceMeta::field_nature` (rain keys track `rain_nature`, the rest
    /// track `data_nature` plus the per-kind overrides). Serializes as an array of
    /// `[key, nature_string]` two-tuples; nature strings are the same snake_case
    /// `CloudDataNature` wire values (`observation` / `radar_qpe` / `nowcast` /
    /// `forecast`) the Panel already matches `data_nature` / `rain_nature` on.
    field_natures: Vec<(&'static str, crate::sources::cloud_catalog::CloudDataNature)>,
    /// True when this kind is part of the region's TRUE auto-seeded keyless
    /// authority set (`region::is_region_keyless_authority`), the exact set
    /// `wizard::finalize_sources` / `env_compat::synthesize` enable zero-clicks:
    /// Open-Meteo everywhere, NWS only in the US, Met.no only in Europe/Nordic.
    /// A paid provider (OpenWeather, WeatherKit, Pirate) is NEVER recommended,
    /// and Met.no is not recommended outside the Nordics, so the "Recommended
    /// here" badge can never claim a service the install does not actually seed.
    /// The UI also grays out NWS outside the US off this flag.
    recommended_here: bool,
    /// The researched per-field merge priority this kind would seed at the
    /// deployment location (`region::default_priority_for`). Higher wins; the UI
    /// can show the default ranking order without re-deriving it.
    region_priority: i32,
    /// True when this kind is region-APPROPRIATE at the deployment location
    /// (`region::is_region_appropriate`), the softer UI-collapse signal distinct
    /// from `recommended_here`. False today only for Met.no outside Europe/the
    /// Nordics (a coarse 9 km or worse grid for a US yard); true for every other
    /// kind everywhere (incl. NWS / NOAA MRMS, whose US-only coverage is the
    /// harder `recommended_here` / enablement gate, not this one). The UI uses
    /// this to collapse a region-irrelevant option without hiding a working one.
    region_appropriate: bool,
    /// True when this kind carries an `upgrade_reason` (`meta.upgrade_reason`
    /// is `Some`), so the UI can PROMOTE the option (show the upgrade line, offer
    /// a one-click add) WITHOUT auto-enabling it. Today this is `Some` only for
    /// Pirate in CONUS: its rain is a model forecast (so it is never recommended
    /// or auto-seeded), but its free key still sharpens the live temp/wind reads.
    /// The marker lets the UI surface that honest upgrade without ever implying
    /// its rain is measured or flipping it on behind the user's back.
    upgrade_available: bool,
    /// True when a source of THIS kind is already present AND enabled in the
    /// saved config. The UI shows "configured" / offers manage-vs-add. A
    /// disabled or absent source of this kind reads false.
    already_configured: bool,
    /// True when a source of THIS kind exists in the saved config REGARDLESS of
    /// enabled. The unified device-card list owns every configured source (on or
    /// off), so the cloud panel's "add coverage" discovery filters on this (not
    /// `already_configured`) to avoid showing a disabled cloud source in BOTH the
    /// device list and the discovery list.
    configured_present: bool,
    /// The honest source-status taxonomy (spec 1.6) for this kind right now, one
    /// of `active` / `watching` / `standby` / `falling_through` / `offline`.
    /// Computed by the SAME shared fn `api::health::compute_source_status` that
    /// drives /api/health, off the live `field_sources` ownership, so the
    /// cloud-source ROW UI and the /api/health rollup read ONE source of truth.
    /// Meaningful for an `already_configured` (enabled) kind; for a NOT-enabled
    /// kind it reads `offline` and the row UI maps the homeowner words off
    /// `already_configured` + `meta.key_tier` + `region_appropriate` instead
    /// (the contract's "Add key to turn on" / "Off" / "Not in your area" cases).
    /// CONTRACT OUT: JSON field name `status`, snake_case enum strings above.
    status: &'static str,
}

/// What the cloud-onboarding page renders against: the honest catalog of cloud
/// weather services, ordered highest-honesty first (NWS, NOAA MRMS, Pirate,
/// OpenWeather, WeatherKit, Open-Meteo, Met.no), each annotated with its live
/// field set, the region recommendation, region-appropriateness, the upgrade
/// marker, and whether it is already configured here.
#[derive(Debug, Serialize)]
struct SourceCatalogResponse {
    /// The deployment latitude/longitude the region recommendation was resolved
    /// against, echoed so the UI can label "recommended here" with the place.
    lat: f64,
    lon: f64,
    /// One entry per cloud weather kind, highest honesty first.
    cloud_sources: Vec<CloudCatalogEntry>,
}

/// GET /api/config/source_catalog -> the honest cloud-source catalog for the
/// no-hardware "cloud weather" onboarding experience.
///
/// For each of the seven cloud weather kinds (the six forecast kinds plus NOAA
/// MRMS radar QPE) it returns the static honesty facts
/// (`sources::cloud_catalog::cloud_meta`, including the per-rain `rain_nature`,
/// `irrigation_rank`, and `upgrade_reason`, flattened to the top level) joined
/// to the live per-deployment wiring: the current-field list this kind actually
/// emits, whether it is the region-recommended default at the configured
/// location, whether it is region-appropriate there, whether it carries an
/// upgrade marker, and whether a source of that kind is already configured +
/// enabled. Read-only + config-shaped: no schema change, no save. Cloud sources
/// are never live_current=true, so this list never implies a cloud option
/// outranks a real LAN station.
async fn get_source_catalog(
    State(ConfigApiState { store, runtime }): State<ConfigApiState>,
) -> impl IntoResponse {
    let cfg = store.load().await.unwrap_or_default();
    let lat = cfg.deployment.location.lat;
    let lon = cfg.deployment.location.lon;

    // Live per-field ownership for the honest source-status taxonomy. Read off
    // the SAME live `field_sources` surface /api/health uses (here via the shared
    // TempestStore on the runtime handles): canonical field name -> the DISPLAY
    // LABEL of the source currently driving it. Empty in the store-only / demo
    // posture (no runtime wired), in which case no kind owns a field; a
    // configured+enabled kind then reads `watching` (calm), an unconfigured kind
    // reads `offline` and the row UI shows it with the off / add-key words.
    let field_sources = runtime
        .as_ref()
        .map(|h| h.tempest_store.field_source_map())
        .unwrap_or_default();
    // The COMPLETE set of writer labels the merge currently attributes an owned
    // field, across ALL fields (not just the headline subset `field_source_map`
    // surfaces). The SAME owner set /api/health reads, so a source owning only a
    // non-headline field (e.g. an Ecowitt gateway owning soil) is recognized as
    // `active` on both surfaces.
    let owner_labels = runtime
        .as_ref()
        .map(|h| h.tempest_store.current_owner_labels())
        .unwrap_or_default();
    // The source priority map (writer label -> priority), the SAME map /api/health
    // passes and the SAME map the merge ranks with, so the catalog's
    // standby-vs-watching decision is priority-aware and congruent with health. A
    // reachable non-owner is `standby` ONLY when a strictly HIGHER-priority source
    // owns a field it could provide; a field held only by a LOWER-or-equal source
    // (e.g. priority-75 MRMS quiet while priority-50 Open-Meteo covers rain) reads
    // the calm `watching`, never `standby`.
    let source_priorities = crate::runtime::source_priority_map(&cfg);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    // The SAME live reachability map /api/health reads (threaded onto the runtime
    // handles, recorded by the bus recorder on every successful fetch). Reading
    // it here makes the catalog status and /api/health congruent off ONE set of
    // reachability facts: a reachable-but-quiet rain source reads `watching` on
    // both surfaces. None in the store-only / demo posture (no runtime wired).
    let source_reachable = runtime.as_ref().map(|h| h.source_reachable.clone());
    // The SAME live observation last-seen map /api/health reads (also threaded
    // onto the runtime handles, recorded by the bus recorder on every
    // Observation). This is the CONGRUENCE FIX for the catalog-vs-health
    // disagreement: the adapters publish `Reachability` only on state CHANGE
    // (noaa_mrms.rs run loop), so a stably-reachable source carries a STALE
    // reachability epoch even while it OBSERVES every few minutes; reading
    // reachability ALONE then read MRMS `offline` in the catalog (>30 min stale)
    // while /api/health, which ALSO accepts a recent Observation as a liveness
    // proof, read it calm. Feeding the SAME `last_obs_epoch` +
    // kind-aware `obs_alive_window_s` here makes a recently-observing source read
    // its calm status (active/standby/watching/falling_through), NOT offline,
    // exactly as /api/health does. None in the store-only / demo posture.
    let source_last_seen = runtime.as_ref().and_then(|h| h.source_last_seen.clone());

    let cloud_sources = crate::sources::cloud_catalog::cloud_kinds()
        .into_iter()
        .filter_map(|kind| {
            // Static honest facts. `cloud_kinds()` yields only cloud forecast
            // kinds, so this is always Some; filter_map keeps the loop total.
            let meta = crate::sources::cloud_catalog::cloud_meta(&kind)?;

            // Live current-field list. Reuse the user's already-configured entry
            // for this kind when present (so the list reflects their config), or
            // a synthetic entry otherwise. `source_field_names` keys off the kind
            // for the cloud kinds, so both yield the same canonical set; reusing
            // the real entry keeps a future config-sensitive field set honest.
            let configured = cfg
                .sources
                .iter()
                .find(|e| crate::config::kind_labels::source_kind_label(&e.source) == meta.kind);
            let live_current_fields = match configured {
                Some(entry) => crate::runtime::source_field_names(&cfg, entry),
                None => {
                    let synthetic = crate::config::schema::SourceEntry {
                        id: format!("catalog_{}", meta.kind),
                        priority: 50,
                        enabled: true,
                        max_age_s: None,
                        source: kind.clone(),
                    };
                    crate::runtime::source_field_names(&cfg, &synthetic)
                }
            };

            // The honest per-FIELD nature for the matrix: one (key, nature) pair
            // per field this kind actually emits, over EXACTLY `live_current_fields`
            // (same canonical keys, same order). The Panel lights a cell from
            // `live_current_fields` and tints it from the matching nature here, so
            // Pirate's wind cell reads `nowcast` while its rain cell reads
            // `forecast` in the same row. `field_nature` (rain keys -> `rain_nature`,
            // the rest -> `data_nature` plus the per-kind overrides) is the single
            // source of truth, so the wire can never disagree with the catalog.
            let field_natures: Vec<(&'static str, crate::sources::cloud_catalog::CloudDataNature)> =
                live_current_fields
                    .iter()
                    .map(|&field| (field, meta.field_nature(field)))
                    .collect();

            // Region recommendation, single-sourced from `config::region` so it
            // matches EXACTLY the keyless authority `finalize_sources` /
            // `synthesize` auto-seed here (Open-Meteo everywhere, NWS in the US,
            // Met.no in the Nordics). Keyed/paid providers and an out-of-region
            // keyless kind read false, so "Recommended here" never lights on a
            // service the install does not actually seed.
            let recommended_here =
                crate::config::region::is_region_keyless_authority(&kind, lat, lon);
            let region_priority = crate::config::region::default_priority_for(&kind, lat, lon);

            // Region-appropriateness: the softer UI-collapse signal (false only
            // for Met.no outside the Nordics), single-sourced from `region` so it
            // never disagrees with the enablement / recommendation predicates.
            let region_appropriate = crate::config::region::is_region_appropriate(&kind, lat, lon);

            // Upgrade marker: true when the kind carries an honest upgrade note
            // (Pirate's CONUS temp/wind free-key upgrade). Lets the UI promote
            // the option without auto-enabling it or implying its rain is
            // measured. Derived straight from the catalog `upgrade_reason` so the
            // marker and the line it gates can never drift apart.
            let upgrade_available = meta.upgrade_reason.is_some();

            // Already configured: a source of this kind present AND enabled.
            let already_configured = cfg.sources.iter().any(|e| {
                e.enabled && crate::config::kind_labels::source_kind_label(&e.source) == meta.kind
            });
            // Present at all (enabled or disabled): the device-card list owns it,
            // so discovery must exclude it either way.
            let configured_present = cfg
                .sources
                .iter()
                .any(|e| crate::config::kind_labels::source_kind_label(&e.source) == meta.kind);

            // Honest source-status taxonomy, via the SHARED ownership helper that
            // also drives /api/health, so the row UI and the health rollup are
            // congruent. Ownership is matched by the configured source's WRITER
            // LABEL (the label the merge actually stamps into field_provenance: the
            // config id for a cloud source), tested against the COMPLETE owner set
            // and the per-field owner map, EXACTLY as /api/health does. A NOT-yet-
            // configured kind has no writer in the merge, so it owns nothing here
            // (the empty-string label matches no owner). `live_current_fields` is
            // exactly the set this kind could provide as a current scalar.
            let label = configured
                .map(crate::runtime::writer_label)
                .unwrap_or_default();
            // This kind's own priority for the priority-aware standby gate. The
            // configured entry's priority when present; an unconfigured kind has no
            // writer in the merge (and reads `offline` via `already_configured ==
            // false` regardless), so its priority is immaterial -> the region
            // default for the kind keeps the lookup honest.
            let own_priority = configured
                .map(|e| e.priority)
                .unwrap_or_else(|| region_priority);
            let providable: std::collections::HashSet<&str> =
                live_current_fields.iter().copied().collect();
            let crate::api::health::OwnershipFacts {
                owns_field,
                other_owns_a_field_it_could_provide,
                outranked_by_higher_priority_owner,
            } = crate::api::health::source_ownership_facts(
                &label,
                own_priority,
                &owner_labels,
                &field_sources,
                &providable,
                &source_priorities,
            );
            // Reachability, read off the SAME live map /api/health reads (threaded
            // onto the runtime handles, recorded by the bus recorder on every
            // successful fetch), keyed by the configured entry's id. A
            // configured-but-faulting kind whose last successful fetch has aged
            // past the hard-offline window now reports `offline` honestly, exactly
            // as it does on /api/health. Fallback when no epoch is recorded yet
            // (a freshly-configured source that has not completed its first fetch,
            // or the store-only / demo posture with no runtime wired): an enabled
            // kind is assumed reachable NOW so it reads the calm default rather
            // than flashing `offline` before its first poll lands; a NOT-enabled
            // kind has no reachability and reads `offline`, which the row UI
            // overrides with the off / add-key / not-in-area words off the flags
            // above.
            let recorded_reachable = configured
                .and_then(|entry| source_reachable.as_ref().and_then(|m| m.get(&entry.id)));
            let last_reachable_epoch =
                recorded_reachable.or_else(|| already_configured.then_some(now));
            // OBSERVATION-LIVENESS PROOF (the congruence fix). The SAME input
            // /api/health feeds `compute_source_status`: this kind's configured
            // entry's last-Observation epoch, judged against the SAME kind-aware
            // `obs_alive_window_s`. A source that OBSERVED within its window reads
            // its calm status even when its Reachability epoch has gone stale
            // (the adapters publish Reachability only on state CHANGE, so a
            // stably-reachable MRMS observing every few minutes would otherwise
            // read `offline` here while /api/health, accepting the obs proof, read
            // it calm). With this, MRMS observing every few minutes never reads
            // offline in the catalog. `None` for an unconfigured kind (no entry,
            // so no recorded observation) or the store-only posture (no handle).
            let last_obs_epoch = configured
                .and_then(|entry| source_last_seen.as_ref().and_then(|m| m.get(&entry.id)));
            let obs_alive_window_s = catalog_obs_alive_window_s(&kind);
            let status =
                crate::api::health::compute_source_status(crate::api::health::SourceStatusInputs {
                    enabled: already_configured,
                    owns_field,
                    other_owns_a_field_it_could_provide,
                    outranked_by_higher_priority_owner,
                    // The catalog has no prior-owner history, so it never asserts
                    // `falling_through` here; a contested field reads `standby` ONLY
                    // when a strictly HIGHER-priority source owns it, else the calm
                    // `watching` (all calm). /api/health, which sees the live
                    // Observation flow, is where `falling_through` is surfaced.
                    was_owner_now_fell_through: false,
                    last_reachable_epoch,
                    // CONGRUENCE: feed the SAME Observation-liveness proof
                    // /api/health uses, so a recently-observing source reads its
                    // calm status (not offline) even with a stale reachability
                    // epoch. The kind-aware window mirrors /api/health's
                    // `obs_alive_window_s` (a slow cloud / MRMS gets the wide
                    // window so it is not false-faulted between polls).
                    last_obs_epoch,
                    obs_alive_window_s,
                    now,
                })
                .as_str();

            Some(CloudCatalogEntry {
                meta,
                live_current_fields,
                field_natures,
                recommended_here,
                region_priority,
                region_appropriate,
                upgrade_available,
                already_configured,
                configured_present,
                status,
            })
        })
        .collect();

    Json(SourceCatalogResponse {
        lat,
        lon,
        cloud_sources,
    })
}

#[derive(Debug, Deserialize, Default)]
struct RawQuery {
    /// Opt in to full-fidelity (unredacted) TOML. Honored only for an
    /// authenticated owner identity; ignored otherwise.
    #[serde(default)]
    reveal: Option<bool>,
}

/// Return the TOML of /data/localsky.toml as text/plain so the Advanced
/// settings page can render a textarea editor.
///
/// REDACTION + GATING (security wave 3): secrets are redacted to the
/// sentinel by default, matching GET / and the backup/draft read paths, so
/// this endpoint never leaks a cleartext token even in the shipped default
/// posture (AuthMode::Disabled). The route itself is additionally treated
/// as PRIVILEGED in `auth::middleware`: an unauthenticated, non-trusted
/// caller is refused BEFORE reaching this handler, even with auth disabled.
///
/// Full fidelity (real secrets) is opt-in via `?reveal=1` AND only for a
/// caller the privileged gate already vouched for: an authenticated owner
/// (session/API-token User) OR a trusted-network caller. The latter is a
/// LAN owner the operator trusts (loopback / RFC1918 / ULA / an explicit
/// trusted_networks match in the disabled-default posture); honoring reveal
/// for them lets a LAN owner in Disabled mode (who has no session) read
/// their own raw config in the Advanced editor. A bare public/anonymous
/// caller never reaches this handler (the gate refuses it). Redaction is
/// still the DEFAULT; reveal must be explicitly requested. The editor PUT
/// also round-trips the sentinel via `unredact_secrets`, so saving a
/// redacted edit preserves untouched secrets.
///
/// Empty 200 when the file hasn't been written yet so the wizard can
/// pre-populate via PUT.
async fn get_raw_toml(
    State(ConfigApiState { store, .. }): State<ConfigApiState>,
    Query(q): Query<RawQuery>,
    req: axum::extract::Request,
) -> impl IntoResponse {
    // Full fidelity is granted on an explicit opt-in to a caller the
    // privileged gate already vouched for: an authenticated owner (User) OR
    // a trusted-network caller. The privileged gate in auth::middleware
    // refuses a bare public/anonymous caller before reaching this handler in
    // BOTH auth modes, so a TrustedNetwork here is a LAN owner the operator
    // trusts (loopback / RFC1918 / ULA / trusted_networks in the disabled
    // default). Honoring ?reveal=1 for them lets a LAN owner in Disabled
    // mode (who has no session) read their own raw config in the Advanced
    // editor. Redacted stays the default; reveal is strictly opt-in.
    let is_owner = matches!(
        req.extensions().get::<crate::auth::RequestIdentity>(),
        Some(crate::auth::RequestIdentity::User(_) | crate::auth::RequestIdentity::TrustedNetwork)
    );
    let reveal = q.reveal.unwrap_or(false) && is_owner;
    match tokio::fs::read_to_string(store.path()).await {
        Ok(s) => {
            let body = if reveal {
                s
            } else {
                // Withhold (empty) rather than ship raw bytes if the file
                // somehow fails to parse for redaction: never leak.
                redact_toml_str(&s).unwrap_or_default()
            };
            (
                StatusCode::OK,
                [(
                    axum::http::header::CONTENT_TYPE,
                    "text/plain; charset=utf-8",
                )],
                body,
            )
                .into_response()
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => (
            StatusCode::OK,
            [(
                axum::http::header::CONTENT_TYPE,
                "text/plain; charset=utf-8",
            )],
            String::new(),
        )
            .into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(ApiError {
                error: "raw_read_failed".into(),
                detail: Some(e.to_string()),
            }),
        )
            .into_response(),
    }
}

/// Replace /data/localsky.toml with the supplied TOML body, after parsing
/// + validating it against the schema invariants.
///
/// REDACTION ROUND-TRIP (security wave 3): GET /config/raw now returns
/// REDACTED TOML by default (the Advanced editor textarea shows the
/// sentinel for each secret, exactly like the form-based settings UI). So
/// the body that comes back here may contain the sentinel for any secret
/// the operator did not retype. We restore those from the stored config
/// (same unredact_secrets pass as PUT /api/config) before saving, and
/// reject any sentinel that has no stored counterpart so the literal
/// "***redacted***" is never persisted as a secret. An operator who opened
/// the editor with ?reveal=1 and typed real secrets simply has no sentinels
/// to restore, so this is a no-op for them.
async fn put_raw_toml(
    State(ConfigApiState { store, runtime }): State<ConfigApiState>,
    body: String,
) -> impl IntoResponse {
    // Validate by parsing through the same path as the loader. Reuses
    // the Validate step in src/config/loader.rs::validate.
    let parsed: Config = match toml::from_str(&body) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(ApiError {
                    error: "toml_parse_error".into(),
                    detail: Some(e.to_string()),
                }),
            )
                .into_response();
        }
    };

    // Restore redacted secrets from the stored config, then reject any
    // unmatched sentinel (a new secret left as the placeholder).
    let mut candidate_json = match serde_json::to_value(&parsed) {
        Ok(v) => v,
        Err(e) => {
            return store_err(ConfigStoreError::Io(format!("serialize candidate: {e}")))
                .into_response();
        }
    };
    let original = match store.load().await {
        Ok(cfg) => serde_json::to_value(&cfg).ok(),
        Err(ConfigStoreError::NotFound) => None,
        Err(e) => return store_err(e).into_response(),
    };
    if let Some(orig) = original.as_ref() {
        unredact_secrets(&mut candidate_json, orig);
    }
    let mut leftover = Vec::new();
    remaining_sentinels(&candidate_json, "$", &mut leftover);
    if !leftover.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "unmatched_redacted_secret".into(),
                detail: Some(format!(
                    "redacted placeholder(s) with no stored value at: {}; supply the real secret",
                    leftover.join(", ")
                )),
            }),
        )
            .into_response();
    }
    let mut parsed: Config = match serde_json::from_value(candidate_json) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(ApiError {
                    error: "config_decode_error".into(),
                    detail: Some(e.to_string()),
                }),
            )
                .into_response();
        }
    };

    if let Err(e) = crate::config::loader::validate(&parsed) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(ApiError {
                error: "config_validation_error".into(),
                detail: Some(format!("{e}")),
            }),
        )
            .into_response();
    }
    // Same structural validation the wizard preflight + PUT / run:
    // errors block the save, warnings ride along in the success body.
    let report = crate::config::validate::validate(&parsed);
    if !report.ok() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": "config_invalid",
                "validation": report,
            })),
        )
            .into_response();
    }
    // The previous on-disk config, for the restart-required diff (the
    // hot-reload re-applies regardless; this just reports the boot-only
    // residue). `original` is the unredacted stored config we loaded above.
    let prev_cfg: Option<Config> = original
        .as_ref()
        .and_then(|v| serde_json::from_value(v.clone()).ok());
    // Region-aware seeding for sources ADDED on this write (same normalize PUT
    // /api/config runs). Idempotent: only newly-added cloud forecast sources are
    // touched, so a raw-TOML edit of an existing source keeps its hand-set rank.
    crate::config::region::normalize_new_cloud_sources(prev_cfg.as_ref(), &mut parsed);
    // Store-managed typed write: snapshots the previous file + fsyncs. We
    // save the unredacted Config (not the raw text) so restored secrets land
    // on disk; the store serializes via to_string_pretty exactly like the
    // form-based PUT, so the on-disk shape is identical either way.
    match store.save(&parsed).await {
        Ok(_) => {
            // Genuine hot-reload: re-apply the engine-tunable subset to the
            // LIVE running system so source priorities, per-field overrides,
            // the forecast provider, and the watering policy take effect now,
            // not at the next restart. `restart_required` flags any change that
            // only a boot can wire (see runtime::apply_runtime_config).
            let outcome = apply_runtime_config_if_live(&runtime, prev_cfg.as_ref(), &parsed);
            Json(serde_json::json!({
                "ok": true,
                "validation": report,
                "restart_required": outcome.restart_required,
                "restart_reasons": outcome.restart_reasons,
            }))
            .into_response()
        }
        Err(e) => store_err(e).into_response(),
    }
}

#[derive(Debug, Serialize)]
struct ApiError {
    error: String,
    detail: Option<String>,
}

fn store_err(e: ConfigStoreError) -> (StatusCode, Json<ApiError>) {
    let code = match &e {
        ConfigStoreError::NotFound => StatusCode::NOT_FOUND,
        ConfigStoreError::Validation(_) => StatusCode::UNPROCESSABLE_ENTITY,
        ConfigStoreError::RollbackTargetMissing(_) => StatusCode::NOT_FOUND,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (
        code,
        Json(ApiError {
            error: "config_store_error".into(),
            detail: Some(e.to_string()),
        }),
    )
}

async fn get_config(
    State(ConfigApiState { store, .. }): State<ConfigApiState>,
) -> impl IntoResponse {
    match store.load().await {
        Ok(cfg) => {
            // Redact secrets before returning. The JSON wire format
            // never exposes API keys, bearer tokens, MD5 passwords, or
            // VAPID privates; clients display a sentinel and PUT-side
            // logic on the operator's edit-form preserves the existing
            // value when the sentinel is sent back unchanged.
            let mut v = match serde_json::to_value(&cfg) {
                Ok(v) => v,
                Err(e) => {
                    return store_err(ConfigStoreError::Io(format!("serialize: {e}")))
                        .into_response();
                }
            };
            redact_secrets(&mut v);
            Json(v).into_response()
        }
        Err(e) => store_err(e).into_response(),
    }
}

/// In-place mutation that replaces every known secret-bearing string
/// with a SECRET_REDACTED_SENTINEL. Conservative: false positives are
/// preferable to leaking a token. The PUT handler accepts the sentinel
/// and preserves the existing stored value.
pub const SECRET_REDACTED_SENTINEL: &str = "***redacted***";

pub(crate) fn redact_secrets(v: &mut serde_json::Value) {
    use serde_json::Value;
    match v {
        Value::Object(map) => {
            for (k, val) in map.iter_mut() {
                let lk = k.to_lowercase();
                let is_secret = lk == "password_md5"
                    || lk == "bearer_token"
                    || lk == "api_key"
                    || lk == "api_token"
                    || lk == "password"
                    || lk == "auth_token"
                    || lk == "vapid_private_path"
                    || lk == "vapid_private"
                    || lk == "webhook_url"
                    || lk == "token"
                    || lk == "shared_secret"
                    || lk == "access_token"
                    || lk == "app_key"
                    || lk == "client_secret"
                    || lk == "refresh_token"
                    // WeatherKit signing key: the Apple `.p8` ES256 private
                    // key (WeatherKitConfig.private_key_pem). Treated like a
                    // password, the adapter signs JWTs with it locally, so it
                    // must never ride the GET /api/config wire (which is
                    // non-privileged + anonymous in the default Disabled
                    // posture). `private_key`/`key_pem` are covered too so a
                    // future PEM-bearing field can't slip the net.
                    || lk == "private_key_pem"
                    || lk == "private_key"
                    || lk == "key_pem"
                    // SMTP credential (EmailConfig.username); the MQTT
                    // source/command/notification username fields are the
                    // other half of a broker credential pair, so redacting
                    // every `username` is both correct and conservative.
                    || lk == "username"
                    // Cloud-controller / cloud-source ACCOUNT EMAIL: the
                    // username half of a credential pair whose password half
                    // is already redacted above. B-hyve (BhyveConfig.email),
                    // Rain Bird (RainbirdConfig.email) and LaCrosse
                    // (LacrosseConfig.email) all authenticate with
                    // account-email + password; leaving the email in the
                    // clear half-leaked the credential. The notification
                    // EmailConfig uses `from_address`/`to_address` (not
                    // `email`) and `vapid_subject` is a mailto: contact, so
                    // those legitimate addresses are untouched. The `email`
                    // KEY on the notifications struct points at an OBJECT
                    // (EmailConfig), not a string. We only redact a secret-named
                    // key when its value is a STRING leaf (the cloud-controller
                    // account emails are strings); when it is an object/array we
                    // must still RECURSE into it, or marking `email` secret would
                    // skip the whole notifications.email subtree and leak its
                    // smtp username/password. See the string-vs-recurse handling
                    // below.
                    || lk == "email";
                // Redact only when the secret-named key holds a STRING value;
                // otherwise (object/array under a secret-named key, e.g. the
                // notifications `email` EmailConfig object) fall through to
                // recursion so nested secrets are still redacted.
                if is_secret {
                    if let Value::String(s) = val {
                        if !s.is_empty() {
                            *s = SECRET_REDACTED_SENTINEL.to_string();
                        }
                        continue;
                    }
                }
                redact_secrets(val);
            }
        }
        Value::Array(arr) => {
            for v in arr.iter_mut() {
                redact_secrets(v);
            }
        }
        _ => {}
    }
}

/// Redact secrets in a localsky.toml TEXT blob, returning sanitized TOML.
///
/// Used by the sibling read paths (GET /backup, GET /config/raw) that ship
/// the on-disk config instead of the JSON-serialized one. The file is
/// always store-written via `toml::to_string_pretty(&Config)`, so parsing
/// it back into `Config`, running the SAME `redact_secrets()` pass over its
/// JSON form, and re-serializing to TOML preserves every field the loader
/// and restore path read while replacing each secret with the sentinel.
/// The wizard/config PUT side already round-trips the sentinel back to the
/// stored value via `unredact_secrets`, so a redacted backup re-imports
/// without losing secrets when restored onto the SAME instance.
///
/// Parse/serialize failures return `None`; the caller decides whether to
/// withhold the field rather than risk shipping raw bytes.
pub(crate) fn redact_toml_str(raw: &str) -> Option<String> {
    let cfg: Config = toml::from_str(raw).ok()?;
    let mut v = serde_json::to_value(&cfg).ok()?;
    redact_secrets(&mut v);
    let redacted: Config = serde_json::from_value(v).ok()?;
    toml::to_string_pretty(&redacted).ok()
}

/// Inverse of redact_secrets: walks the candidate config alongside the
/// stored config, and any place the candidate contains the sentinel,
/// substitutes the original value back in. Lets clients PUT a redacted
/// JSON without losing the secret.
///
/// Arrays whose elements carry an `id` field (sources, controllers) are
/// matched BY ID, not by index: a reorder or delete in the candidate
/// must not attach one entry's stored secret to a different entry.
/// Id-less arrays still match positionally.
pub(crate) fn unredact_secrets(candidate: &mut serde_json::Value, original: &serde_json::Value) {
    use serde_json::Value;
    match (candidate, original) {
        (Value::Object(c), Value::Object(o)) => {
            for (k, c_val) in c.iter_mut() {
                if let Some(o_val) = o.get(k) {
                    if let Value::String(s) = c_val {
                        if s == SECRET_REDACTED_SENTINEL {
                            *c_val = o_val.clone();
                            continue;
                        }
                    }
                    unredact_secrets(c_val, o_val);
                }
            }
        }
        (Value::Array(c), Value::Array(o)) => {
            // The stored side decides the matching mode: it is always
            // server-serialized, so sources/controllers reliably carry
            // string ids there. Candidate entries without an id (or
            // with an unknown id) simply get nothing restored; any
            // sentinel left in them is rejected by the caller.
            let id_keyed = !o.is_empty()
                && o.iter()
                    .all(|v| v.get("id").map(|id| id.is_string()).unwrap_or(false));
            if id_keyed {
                for c_v in c.iter_mut() {
                    let id = c_v.get("id").and_then(|v| v.as_str()).map(str::to_owned);
                    let Some(id) = id else { continue };
                    if let Some(o_v) = o
                        .iter()
                        .find(|ov| ov.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))
                    {
                        unredact_secrets(c_v, o_v);
                    }
                }
            } else {
                for (i, c_v) in c.iter_mut().enumerate() {
                    if let Some(o_v) = o.get(i) {
                        unredact_secrets(c_v, o_v);
                    }
                }
            }
        }
        _ => {}
    }
}

/// Before the by-id unredact pass, restore a RENAMED entry's secrets from its
/// OLD stored counterpart. `renames` maps new_id -> old_id (the client sends it
/// as a transient `__renames` hint when a source/controller id is renamed).
/// Without this, a renamed entry has no stored counterpart under its new id, so
/// its redacted-secret sentinel would survive `unredact_secrets` and the PUT
/// would be rejected as `unmatched_redacted_secret`, breaking rename for any
/// keyed source (api keys, tokens, passwords, private keys).
pub(crate) fn apply_rename_unredact(
    candidate: &mut serde_json::Value,
    original: &serde_json::Value,
    renames: &std::collections::HashMap<String, String>,
) {
    for key in ["sources", "controllers"] {
        let Some(c_arr) = candidate.get_mut(key).and_then(|v| v.as_array_mut()) else {
            continue;
        };
        let Some(o_arr) = original.get(key).and_then(|v| v.as_array()) else {
            continue;
        };
        for c_v in c_arr.iter_mut() {
            let Some(new_id) = c_v.get("id").and_then(|v| v.as_str()).map(str::to_owned) else {
                continue;
            };
            let Some(old_id) = renames.get(&new_id) else {
                continue;
            };
            if let Some(o_v) = o_arr
                .iter()
                .find(|ov| ov.get("id").and_then(|v| v.as_str()) == Some(old_id.as_str()))
            {
                unredact_secrets(c_v, o_v);
            }
        }
    }
}

/// JSON paths of every string still equal to the sentinel. A non-empty
/// result after unredact_secrets means a redacted placeholder had no
/// stored counterpart (new/renamed entry); saving it would persist the
/// literal sentinel as the secret, so the PUT handler rejects instead.
pub(crate) fn remaining_sentinels(v: &serde_json::Value, path: &str, out: &mut Vec<String>) {
    use serde_json::Value;
    match v {
        Value::String(s) if s == SECRET_REDACTED_SENTINEL => out.push(path.to_string()),
        Value::Object(map) => {
            for (k, val) in map {
                remaining_sentinels(val, &format!("{path}.{k}"), out);
            }
        }
        Value::Array(arr) => {
            for (i, val) in arr.iter().enumerate() {
                // Prefer the element id in the path when present.
                let seg = val
                    .get("id")
                    .and_then(|id| id.as_str())
                    .map(|id| format!("{path}[id={id}]"))
                    .unwrap_or_else(|| format!("{path}[{i}]"));
                remaining_sentinels(val, &seg, out);
            }
        }
        _ => {}
    }
}

async fn put_config(
    State(ConfigApiState { store, runtime }): State<ConfigApiState>,
    Json(mut candidate_json): Json<serde_json::Value>,
) -> impl IntoResponse {
    // Load the current Config so we can restore any redacted secrets.
    let original = match store.load().await {
        Ok(cfg) => match serde_json::to_value(&cfg) {
            Ok(v) => v,
            Err(e) => {
                return store_err(ConfigStoreError::Io(format!("serialize current: {e}")))
                    .into_response();
            }
        },
        Err(ConfigStoreError::NotFound) => serde_json::Value::Null,
        Err(e) => return store_err(e).into_response(),
    };
    // Extract + strip the client's transient rename hint (new_id -> old_id) so a
    // renamed source/controller can resolve its redacted secrets from the entry
    // stored under its OLD id. Not part of Config; removed before deserialize.
    let renames: std::collections::HashMap<String, String> = candidate_json
        .as_object_mut()
        .and_then(|o| o.remove("__renames"))
        .and_then(|v| serde_json::from_value(v).ok())
        .unwrap_or_default();
    if !original.is_null() {
        if !renames.is_empty() {
            apply_rename_unredact(&mut candidate_json, &original, &renames);
        }
        unredact_secrets(&mut candidate_json, &original);
    }
    // Any sentinel that survived has no stored counterpart (new entry,
    // renamed id, or no config on disk). Saving would persist the
    // literal "***redacted***" as the secret; reject instead.
    let mut leftover = Vec::new();
    remaining_sentinels(&candidate_json, "$", &mut leftover);
    if !leftover.is_empty() {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "unmatched_redacted_secret".into(),
                detail: Some(format!(
                    "redacted placeholder(s) with no stored value at: {}; supply the real secret",
                    leftover.join(", ")
                )),
            }),
        )
            .into_response();
    }
    let mut cfg: Config = match serde_json::from_value(candidate_json) {
        Ok(c) => c,
        Err(e) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(ApiError {
                    error: "config_decode_error".into(),
                    detail: Some(e.to_string()),
                }),
            )
                .into_response();
        }
    };
    // Auto-mark the sole controller as default when none is set, exactly as the
    // wizard's finalize_for_apply does before its own save. Without this, a
    // settings editor that PUTs a single controller left at `default = false`
    // (the editor just showed it valid) 422s here on the "at least one
    // controller must have default = true" gate. Idempotent: a no-op when a
    // default already exists or there are 0 / 2+ controllers.
    crate::config::loader::auto_default_controller(&mut cfg);
    // Structural validation: errors block the save (the report rides in
    // the 422 body so the UI can show field-level issues); warnings are
    // returned alongside the success body.
    let report = crate::config::validate::validate(&cfg);
    if !report.ok() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": "config_invalid",
                "validation": report,
            })),
        )
            .into_response();
    }
    // The previous on-disk config, for the restart-required diff. `original`
    // is the unredacted stored config (Null when no config existed yet, e.g.
    // a fresh install applying its first config via PUT).
    let prev_cfg: Option<Config> = if original.is_null() {
        None
    } else {
        serde_json::from_value(original).ok()
    };
    // Region-aware seeding for sources ADDED on this write. The UI "add source"
    // path only has the kind string client-side, so it seeds a flat priority
    // (50) for every cloud and never disables NWS outside the US. Normalize the
    // newly-added cloud forecast sources to their researched region rank +
    // enablement here, before persist. Idempotent: only touches ids absent from
    // `prev_cfg`, so a user's customized existing source is preserved on re-save.
    crate::config::region::normalize_new_cloud_sources(prev_cfg.as_ref(), &mut cfg);
    match store.save(&cfg).await {
        Ok(v) => {
            // Genuine hot-reload: re-apply the engine-tunable subset to the
            // LIVE running system (source priorities, per-field overrides,
            // forecast provider, watering policy) so the save takes effect now
            // rather than at the next restart. `restart_required` flags the
            // residue only a boot can wire (new source connection, zone set,
            // listen address, ...) for the Wave-2 "restart required" banner.
            let outcome = apply_runtime_config_if_live(&runtime, prev_cfg.as_ref(), &cfg);
            Json(serde_json::json!({
                "saved": v,
                "validation": report,
                "restart_required": outcome.restart_required,
                "restart_reasons": outcome.restart_reasons,
            }))
            .into_response()
        }
        Err(e) => store_err(e).into_response(),
    }
}

/// GET /api/v1/config/validate -> the structured report for the config
/// as currently on disk. The settings overview surfaces warnings.
async fn get_validate(
    State(ConfigApiState { store, .. }): State<ConfigApiState>,
) -> impl IntoResponse {
    match store.load().await {
        Ok(cfg) => Json(serde_json::json!({
            "validation": crate::config::validate::validate(&cfg)
        }))
        .into_response(),
        Err(ConfigStoreError::NotFound) => Json(serde_json::json!({
            "validation": { "errors": [], "warnings": [] },
            "note": "no config yet (wizard pending)",
        }))
        .into_response(),
        Err(e) => store_err(e).into_response(),
    }
}

async fn get_schema() -> impl IntoResponse {
    let schema = schema_for!(Config);
    Json(schema)
}

#[derive(Debug, Deserialize)]
struct PreviewBody {
    candidate: Config,
}

#[derive(Debug, Serialize)]
struct PreviewResult {
    ok: bool,
    errors: Vec<String>,
}

async fn preview_config(
    State(_state): State<ConfigApiState>,
    Json(body): Json<PreviewBody>,
) -> impl IntoResponse {
    let mut errors = Vec::new();
    if let Err(e) = crate::config::loader::validate(&body.candidate) {
        errors.push(e.to_string());
    }
    Json(PreviewResult {
        ok: errors.is_empty(),
        errors,
    })
}

/// GET /api/v1/config/snapshots -> the on-disk snapshot history
/// (<config_dir>/snapshots/<ts>.toml), newest first.
async fn get_snapshots(
    State(ConfigApiState { store, .. }): State<ConfigApiState>,
) -> impl IntoResponse {
    match store.list_snapshots().await {
        Ok(list) => {
            let snapshots: Vec<_> = list
                .into_iter()
                .map(|v| {
                    serde_json::json!({
                        "ts": v.version,
                        "applied_at_epoch": v.applied_at_epoch,
                        "schema_version": v.schema_version,
                        "note": v.note,
                    })
                })
                .collect();
            Json(serde_json::json!({ "snapshots": snapshots })).into_response()
        }
        Err(e) => store_err(e).into_response(),
    }
}

#[derive(Debug, Deserialize, Default)]
struct RollbackQuery {
    to: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct RollbackBody {
    ts: u32,
}

/// POST /api/v1/config/rollback with {"ts": <snapshot ts>} (or the
/// legacy ?to=<ts> query). Validates the snapshot parses before the
/// swap; the pre-rollback config is snapshotted first.
async fn post_rollback(
    State(ConfigApiState { store, runtime }): State<ConfigApiState>,
    Query(q): Query<RollbackQuery>,
    body: Option<Json<RollbackBody>>,
) -> impl IntoResponse {
    let Some(ts) = body.map(|Json(b)| b.ts).or(q.to) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "rollback_target_missing".into(),
                detail: Some("send {\"ts\": <snapshot ts>} or ?to=<ts>".into()),
            }),
        )
            .into_response();
    };
    // Pre-rollback config for the restart-required diff (best-effort; a missing
    // current config simply skips the diff and reloads only the tunables).
    let prev_cfg = store.load().await.ok();
    match store.rollback(ts).await {
        Ok(cfg) => {
            // A rollback REPLACES the live config, so hot-reload the tunables to
            // the restored values and flag any boot-only residue, exactly like a
            // PUT. Without this a rollback would also "apply on next restart".
            let outcome = apply_runtime_config_if_live(&runtime, prev_cfg.as_ref(), &cfg);
            // Same redaction contract as GET /: secrets never ride the
            // JSON wire format.
            let mut v = serde_json::to_value(&cfg).unwrap_or(serde_json::Value::Null);
            redact_secrets(&mut v);
            Json(serde_json::json!({
                "ok": true,
                "restored_ts": ts,
                "config": v,
                "restart_required": outcome.restart_required,
                "restart_reasons": outcome.restart_reasons,
            }))
            .into_response()
        }
        Err(e) => store_err(e).into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cloud_source_emitting_current_is_tier_cloud_not_forecast() {
        // The taxonomy fix: a cloud weather service that emits a CURRENT scalar
        // for a field must classify as tier "cloud" (a usable current source),
        // NOT "forecast". Open-Meteo (live_current=false) supplies a current
        // wind/temp/etc. scalar into the per-field merge, so for a field it
        // covers it is "cloud".
        assert_eq!(
            source_field_tier(false, true),
            "cloud",
            "a cloud source emitting a current scalar for a field is tier cloud"
        );
        // A real local physical sensor is tier "device".
        assert_eq!(
            source_field_tier(true, true),
            "device",
            "a live local station is tier device"
        );
        // A source that emits no current scalar for the field is "forecast".
        assert_eq!(
            source_field_tier(false, false),
            "forecast",
            "a source with no current scalar for the field is tier forecast"
        );
    }

    #[test]
    fn open_meteo_classifies_as_cloud_for_wind_current() {
        // End-to-end of the data path: an enabled Open-Meteo source declares a
        // live current WIND scalar (source_field_names returns wind_mph), is not
        // a live local station (live_current=false), so the picker tier for it is
        // "cloud", proving "pick Open-Meteo for wind" reads as a current source.
        use crate::config::schema::*;
        let mut cfg = Config::default();
        cfg.sources.push(SourceEntry {
            id: "om".into(),
            priority: 50,
            enabled: true,
            max_age_s: None,
            source: SourceKind::OpenMeteo(OpenMeteoConfig {
                forecast_days: 7,
                forecast_hours: 48,
                past_days: 1,
                include_radar: false,
                model: "best_match".into(),
            }),
        });
        let entry = &cfg.sources[0];
        let fields = crate::runtime::source_field_names(&cfg, entry);
        assert!(
            fields.contains(&"wind_mph"),
            "Open-Meteo emits a live current wind scalar: {fields:?}"
        );
        // Not a live local station -> live_current is false for a cloud service.
        let live_current =
            !entry.source.is_forecast() && matches!(entry.source, SourceKind::TempestUdp(_));
        assert!(!live_current, "Open-Meteo is not a local physical sensor");
        assert_eq!(
            source_field_tier(live_current, !fields.is_empty()),
            "cloud",
            "Open-Meteo owning wind is tier cloud, never forecast"
        );
    }

    fn cfg_with_secrets() -> serde_json::Value {
        serde_json::json!({
            "schema_version": 1,
            "deployment": {
                "location": { "lat": 28.5, "lon": -81.4 },
                "units": "imperial",
                "display_name": "Yard"
            },
            "sources": [{
                "id": "ha_pass",
                "priority": 30,
                "enabled": true,
                "kind": "ha_passthrough",
                "config": {
                    "base_url": "http://ha.local:8123",
                    "bearer_token": "supersecret_ha_token_xyz",
                    "field_map": {}
                }
            }, {
                "id": "mqtt_sensors",
                "priority": 80,
                "enabled": true,
                "kind": "mqtt",
                "config": {
                    "broker_host": "broker.local",
                    "broker_port": 1883,
                    "username": "user1",
                    "password": "mqtt_password_123",
                    "subscriptions": [{
                        "topic": "soil/+",
                        "field": "soil_moisture",
                        "scale": 1.0,
                        "offset": 0.0
                    }]
                }
            }],
            "controllers": [{
                "id": "os_main",
                "default": true,
                "enabled": true,
                "kind": "opensprinkler_direct",
                "config": {
                    "host": "10.0.0.10",
                    "port": 80,
                    "password_md5": "abc123md5hash",
                    "poll_interval_s": 10
                }
            }],
            "zones": {},
            "llm": {
                "provider": "openai_compat",
                "config": {
                    "base_url": "https://api.openai.com",
                    "model": "gpt-4o-mini",
                    "api_key": "sk-proj-very-real-looking-key"
                },
                "timeout_s": 20,
                "explanation_ttl_s": 300,
                "anomaly_ttl_s": 3600
            },
            "notifications": {
                "web_push": {
                    "vapid_public": "BPublicKey",
                    "vapid_private_path": "/keys/vapid-private.pem",
                    "vapid_subject": "mailto:ops@example.com"
                },
                "slack": {
                    "webhook_url": "https://hooks.slack.com/services/SECRET"
                }
            },
            "features": {},
            "engine": {}
        })
    }

    #[test]
    fn redact_replaces_every_known_secret() {
        let mut v = cfg_with_secrets();
        redact_secrets(&mut v);
        let s = serde_json::to_string(&v).unwrap();
        // Sanitize-grep: no secret value should survive
        assert!(!s.contains("supersecret_ha_token_xyz"), "HA bearer leaked");
        assert!(!s.contains("mqtt_password_123"), "MQTT password leaked");
        assert!(!s.contains("abc123md5hash"), "OS password_md5 leaked");
        assert!(
            !s.contains("sk-proj-very-real-looking-key"),
            "API key leaked"
        );
        assert!(
            !s.contains("hooks.slack.com/services/SECRET"),
            "Slack webhook leaked"
        );
        assert!(
            !s.contains("/keys/vapid-private.pem"),
            "VAPID private path leaked"
        );
        // Sentinel should appear
        assert!(s.contains(SECRET_REDACTED_SENTINEL));
        // Non-secret fields should remain
        assert!(
            s.contains("ha.local:8123"),
            "base_url unexpectedly redacted"
        );
        assert!(s.contains("os_main"), "controller id unexpectedly redacted");
        assert!(s.contains("28.5"), "lat unexpectedly redacted");
    }

    #[test]
    fn redact_covers_smtp_username_and_password() {
        // EmailConfig.username is an SMTP credential; it must be redacted
        // alongside the password. (The MQTT username fields ride the same
        // `username` rule for free, which is correct: it's half of a
        // broker credential pair.)
        let mut v = serde_json::json!({
            "notifications": {
                "email": {
                    "smtp_host": "smtp.example.com",
                    "smtp_port": 587,
                    "username": "smtp_user_secret",
                    "password": "smtp_pass_secret",
                    "from_address": "alerts@example.com",
                    "to_address": "me@example.com",
                    "starttls": true
                }
            }
        });
        redact_secrets(&mut v);
        let s = serde_json::to_string(&v).unwrap();
        assert!(!s.contains("smtp_user_secret"), "SMTP username leaked");
        assert!(!s.contains("smtp_pass_secret"), "SMTP password leaked");
        // Non-secret SMTP fields stay visible so the form still renders.
        assert!(
            s.contains("smtp.example.com"),
            "smtp_host unexpectedly redacted"
        );
        assert!(s.contains("alerts@example.com"), "from_address redacted");
    }

    #[test]
    fn redact_toml_str_sanitizes_a_real_config_file() {
        // The backup + raw read paths re-serialize the on-disk TOML through
        // this helper. Build a full Config, write it the same way the store
        // does, then prove the redacted TOML still parses AND carries no
        // cleartext secret.
        use crate::config::schema::*;
        let mut cfg = Config::default();
        cfg.deployment.location = Location {
            lat: 28.5,
            lon: -81.4,
            elevation_m: None,
        };
        cfg.sources.push(SourceEntry {
            id: "ha_pass".into(),
            priority: 30,
            enabled: true,
            max_age_s: None,
            source: SourceKind::HaPassthrough(HaPassthroughConfig {
                base_url: "http://ha.local:8123".into(),
                bearer_token: "supersecret_ha_token_xyz".into(),
                field_map: Default::default(),
                soil_zone_map: Default::default(),
            }),
        });
        cfg.controllers.push(ControllerEntry {
            id: "os_main".into(),
            default: true,
            enabled: true,
            controller: ControllerKind::OpensprinklerDirect(OpenSprinklerDirectConfig {
                host: "10.0.0.10".into(),
                port: 80,
                password_md5: "abc123md5hash".into(),
                poll_interval_s: 10,
            }),
        });
        cfg.notifications.email = Some(EmailConfig {
            smtp_host: "smtp.example.com".into(),
            smtp_port: 587,
            username: "smtp_user_secret".into(),
            password: "smtp_pass_secret".into(),
            from_address: "a@example.com".into(),
            to_address: "b@example.com".into(),
            starttls: true,
        });

        // Store-style serialization (matches FileConfigStore::save).
        let raw = toml::to_string_pretty(&cfg).unwrap();
        // Sanity: the RAW file does contain the secrets (this is the leak
        // the backup/raw paths used to ship).
        assert!(raw.contains("supersecret_ha_token_xyz"));

        let redacted = redact_toml_str(&raw).expect("redaction parses + re-serializes");
        // No cleartext secret survives.
        assert!(
            !redacted.contains("supersecret_ha_token_xyz"),
            "HA token leaked in backup TOML"
        );
        assert!(
            !redacted.contains("abc123md5hash"),
            "OS password_md5 leaked in backup TOML"
        );
        assert!(
            !redacted.contains("smtp_user_secret"),
            "SMTP username leaked in backup TOML"
        );
        assert!(
            !redacted.contains("smtp_pass_secret"),
            "SMTP password leaked in backup TOML"
        );
        assert!(
            redacted.contains(SECRET_REDACTED_SENTINEL),
            "sentinel present"
        );
        // The redacted output is still valid, restorable TOML.
        let reparsed: Config =
            toml::from_str(&redacted).expect("redacted TOML re-parses to Config");
        assert_eq!(reparsed.controllers[0].id, "os_main");
    }

    #[test]
    fn redact_covers_cloud_controller_account_email() {
        // The cloud controllers (B-hyve, Rain Bird) and the LaCrosse cloud
        // source authenticate with account-email + password. The password half
        // was already redacted; this proves the email (the username half) is
        // too, while a legitimate notification address (from_address /
        // to_address / vapid_subject mailto:) is NOT redacted.
        let mut v = serde_json::json!({
            "controllers": [{
                "id": "bhyve_main",
                "kind": "bhyve",
                "config": {
                    "email": "owner.account@example.com",
                    "password": "bhyve_pw_secret",
                    "device_id": "dev-123"
                }
            }, {
                "id": "rainbird_main",
                "kind": "rainbird",
                "config": {
                    "email": "rainbird.account@example.com",
                    "password": "rb_pw_secret",
                    "controller_id": "ctl-9"
                }
            }],
            "sources": [{
                "id": "lacrosse_main",
                "kind": "lacrosse",
                "config": {
                    "email": "lacrosse.account@example.com",
                    "password": "lc_pw_secret",
                    "device_id": "LTV-WSDTH04"
                }
            }],
            "notifications": {
                "email": {
                    "smtp_host": "smtp.example.com",
                    "from_address": "alerts@example.com",
                    "to_address": "me@example.com",
                    "username": "smtp_user",
                    "password": "smtp_pw"
                },
                "web_push": {
                    "vapid_subject": "mailto:ops@example.com"
                }
            }
        });
        redact_secrets(&mut v);
        let s = serde_json::to_string(&v).unwrap();
        // Account emails (the credential username half) must NOT survive.
        assert!(
            !s.contains("owner.account@example.com"),
            "B-hyve account email leaked"
        );
        assert!(
            !s.contains("rainbird.account@example.com"),
            "Rain Bird account email leaked"
        );
        assert!(
            !s.contains("lacrosse.account@example.com"),
            "LaCrosse account email leaked"
        );
        // The password halves stay redacted as before.
        assert!(!s.contains("bhyve_pw_secret"), "B-hyve password leaked");
        assert!(!s.contains("rb_pw_secret"), "Rain Bird password leaked");
        assert!(!s.contains("lc_pw_secret"), "LaCrosse password leaked");
        // Legitimate NOTIFICATION addresses are untouched: from/to_address are
        // not credentials, and vapid_subject is a contact mailto:.
        assert!(
            s.contains("alerts@example.com"),
            "from_address must NOT be redacted"
        );
        assert!(
            s.contains("me@example.com"),
            "to_address must NOT be redacted"
        );
        assert!(
            s.contains("mailto:ops@example.com"),
            "vapid_subject must NOT be redacted"
        );
        // Non-secret device identifiers stay visible so the forms render.
        assert!(s.contains("dev-123"), "device_id unexpectedly redacted");
        assert!(s.contains("LTV-WSDTH04"), "device_id unexpectedly redacted");
    }

    #[test]
    fn redact_covers_weatherkit_private_key() {
        // The Apple WeatherKit `.p8` ES256 signing key
        // (WeatherKitConfig.private_key_pem) is a credential: it must never
        // ride the GET /api/config wire (non-privileged + anonymous in the
        // default Disabled posture). This is the BLOCKER from the audit.
        let mut v = serde_json::json!({
            "sources": [{
                "id": "wk_main",
                "kind": "weather_kit",
                "config": {
                    "key_id": "ABC123KEYID",
                    "team_id": "TEAM456",
                    "service_id": "com.example.weather",
                    "private_key_pem": "-----BEGIN PRIVATE KEY-----\nMIGHsuperSECRETp8KEYbytes\n-----END PRIVATE KEY-----",
                    "language": "en"
                }
            }]
        });
        redact_secrets(&mut v);
        let s = serde_json::to_string(&v).unwrap();
        assert!(
            !s.contains("MIGHsuperSECRETp8KEYbytes"),
            "WeatherKit private_key_pem leaked"
        );
        assert!(
            !s.contains("BEGIN PRIVATE KEY"),
            "WeatherKit PEM body leaked"
        );
        // The public WeatherKit identifiers stay visible so the form renders.
        assert!(s.contains("ABC123KEYID"), "key_id unexpectedly redacted");
        assert!(s.contains("TEAM456"), "team_id unexpectedly redacted");
        assert!(
            s.contains("com.example.weather"),
            "service_id unexpectedly redacted"
        );
    }

    #[test]
    fn redact_leaves_no_marked_secret_in_a_full_config() {
        // Future-proofing invariant (the audit's ask): build a Config that
        // sets EVERY known secret-bearing field to a recognizable marker,
        // run it through the GET /api/config redactor, and assert NO marker
        // survives. Whoever adds a new credential field (a new token / PEM /
        // password) must populate it here, so the day a redactor allowlist
        // misses it, this test fails LOUD instead of leaking the secret on
        // the anonymous default-posture wire. The marker prefix is the
        // tripwire: any cleartext secret betrays itself by carrying it.
        use crate::config::schema::*;
        const M: &str = "SECRETMARKER";

        let mut cfg = Config::default();

        // Weather sources carrying credentials.
        cfg.sources.push(SourceEntry {
            id: "ha_pass".into(),
            priority: 30,
            enabled: true,
            max_age_s: None,
            source: SourceKind::HaPassthrough(HaPassthroughConfig {
                base_url: "http://ha.local:8123".into(),
                bearer_token: format!("{M}_ha_bearer"),
                field_map: Default::default(),
                soil_zone_map: Default::default(),
            }),
        });
        cfg.sources.push(SourceEntry {
            id: "wk_main".into(),
            priority: 40,
            enabled: true,
            max_age_s: None,
            source: SourceKind::WeatherKit(WeatherKitConfig {
                key_id: "ABC123KEYID".into(),
                team_id: "TEAM456".into(),
                service_id: "com.example.weather".into(),
                private_key_pem: format!("{M}_apple_p8_key"),
                language: "en".into(),
            }),
        });

        // Controller carrying a credential.
        cfg.controllers.push(ControllerEntry {
            id: "os_main".into(),
            default: true,
            enabled: true,
            controller: ControllerKind::OpensprinklerDirect(OpenSprinklerDirectConfig {
                host: "10.0.0.10".into(),
                port: 80,
                password_md5: format!("{M}_os_md5"),
                poll_interval_s: 10,
            }),
        });

        // LLM provider api_key.
        cfg.llm = Some(LlmConfig {
            provider: LlmProviderKind::OpenaiCompat(OpenaiCompatConfig {
                base_url: "https://api.openai.com".into(),
                model: "gpt-4o-mini".into(),
                api_key: Some(format!("{M}_openai_key")),
            }),
            timeout_s: 20,
            explanation_ttl_s: 300,
            anomaly_ttl_s: 3600,
        });

        // Notification credentials.
        cfg.notifications.email = Some(EmailConfig {
            smtp_host: "smtp.example.com".into(),
            smtp_port: 587,
            username: format!("{M}_smtp_user"),
            password: format!("{M}_smtp_pass"),
            from_address: "a@example.com".into(),
            to_address: "b@example.com".into(),
            starttls: true,
        });
        cfg.notifications.slack = Some(SlackConfig {
            webhook_url: format!("https://hooks.slack.com/services/{M}_slack"),
        });
        cfg.notifications.web_push = Some(WebPushConfig {
            vapid_public: "BPublicKeyNotSecret".into(),
            vapid_private_path: format!("/keys/{M}_vapid.pem"),
            vapid_subject: "mailto:ops@example.com".into(),
        });
        cfg.notifications.mqtt = Some(MqttConfig {
            host: "broker.local".into(),
            port: 1883,
            username: Some(format!("{M}_mqtt_user")),
            password: Some(format!("{M}_mqtt_pass")),
            discovery_prefix: "homeassistant".into(),
            publish_enabled: true,
            subscribe_enabled: false,
        });
        cfg.notifications.ntfy = Some(NtfyConfig {
            base_url: "https://ntfy.sh".into(),
            topic: "localsky".into(),
            auth_token: Some(format!("{M}_ntfy_token")),
        });

        // Redact through the same path GET /api/config uses.
        let mut v = serde_json::to_value(&cfg).expect("serialize full config");
        redact_secrets(&mut v);
        let s = serde_json::to_string(&v).expect("serialize redacted");

        assert!(
            !s.contains(M),
            "a secret-bearing field survived redaction (carrying the {M} \
             tripwire). If you added a new credential field, add its key \
             name to redact_secrets::is_secret. Leaked JSON: {s}"
        );
        // The redactor actually ran (sentinel present, public ids intact).
        assert!(s.contains(SECRET_REDACTED_SENTINEL), "sentinel present");
        assert!(s.contains("ABC123KEYID"), "public WK key_id must survive");
        assert!(
            s.contains("BPublicKeyNotSecret"),
            "public VAPID key must survive"
        );
    }

    #[test]
    fn redact_empty_strings_left_alone() {
        let mut v = serde_json::json!({
            "config": {
                "api_key": ""
            }
        });
        redact_secrets(&mut v);
        // Empty stays empty (so the UI can distinguish "no token set" from "redacted")
        assert_eq!(v["config"]["api_key"], "");
    }

    #[test]
    fn unredact_restores_original_secret_when_sentinel_present() {
        let original = cfg_with_secrets();
        let mut redacted = original.clone();
        redact_secrets(&mut redacted);
        // Simulate the user submitting the redacted form unchanged
        let mut candidate = redacted.clone();
        unredact_secrets(&mut candidate, &original);
        // The candidate now matches the original
        assert_eq!(candidate, original, "unredact failed to restore secrets");
    }

    #[test]
    fn unredact_keeps_user_edit() {
        let original = cfg_with_secrets();
        let mut candidate = original.clone();
        candidate["llm"]["config"]["api_key"] = serde_json::json!("new-api-key");
        unredact_secrets(&mut candidate, &original);
        // Edited value preserved (it wasn't the sentinel)
        assert_eq!(candidate["llm"]["config"]["api_key"], "new-api-key");
    }

    #[test]
    fn unredact_reordered_sources_keeps_secrets_on_the_right_id() {
        let original = cfg_with_secrets();
        let mut candidate = original.clone();
        redact_secrets(&mut candidate);
        // User reordered the sources array in the settings UI.
        let arr = candidate["sources"].as_array_mut().unwrap();
        arr.reverse();
        unredact_secrets(&mut candidate, &original);
        let mqtt = candidate["sources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == "mqtt_sensors")
            .unwrap();
        assert_eq!(
            mqtt["config"]["password"], "mqtt_password_123",
            "mqtt entry must get the mqtt password, not the HA token"
        );
        let ha = candidate["sources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == "ha_pass")
            .unwrap();
        assert_eq!(ha["config"]["bearer_token"], "supersecret_ha_token_xyz");
    }

    #[test]
    fn unredact_after_delete_does_not_shift_secrets() {
        let original = cfg_with_secrets();
        let mut candidate = original.clone();
        redact_secrets(&mut candidate);
        // User deleted the FIRST source; index 0 is now mqtt_sensors.
        candidate["sources"].as_array_mut().unwrap().remove(0);
        unredact_secrets(&mut candidate, &original);
        let sources = candidate["sources"].as_array().unwrap();
        assert_eq!(sources.len(), 1);
        assert_eq!(sources[0]["id"], "mqtt_sensors");
        assert_eq!(
            sources[0]["config"]["password"], "mqtt_password_123",
            "deletion must not hand mqtt the deleted entry's secret"
        );
        // And nothing still carries the sentinel.
        let mut leftover = Vec::new();
        remaining_sentinels(&candidate, "$", &mut leftover);
        assert!(leftover.is_empty(), "leftover sentinels: {leftover:?}");
    }

    #[test]
    fn rename_unredact_restores_secret_from_old_id() {
        // Renaming a keyed source must recover its redacted secret from the OLD
        // stored id via the __renames hint, or the PUT would 400 on the surviving
        // sentinel (the bug that broke rename for every keyed source).
        let original = cfg_with_secrets();
        let mut candidate = original.clone();
        redact_secrets(&mut candidate);
        // User renamed source "ha_pass" -> "ha_backup" in the editor.
        {
            let entry = candidate["sources"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|s| s["id"] == "ha_pass")
                .unwrap();
            entry["id"] = serde_json::json!("ha_backup");
            assert_eq!(entry["config"]["bearer_token"], SECRET_REDACTED_SENTINEL);
        }
        let renames: std::collections::HashMap<String, String> =
            [("ha_backup".to_string(), "ha_pass".to_string())].into();
        apply_rename_unredact(&mut candidate, &original, &renames);
        unredact_secrets(&mut candidate, &original);
        let renamed = candidate["sources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["id"] == "ha_backup")
            .unwrap();
        assert_eq!(
            renamed["config"]["bearer_token"], "supersecret_ha_token_xyz",
            "renamed entry must recover its secret from the old id"
        );
        let mut leftover = Vec::new();
        remaining_sentinels(&candidate, "$", &mut leftover);
        assert!(
            leftover.is_empty(),
            "leftover sentinels after rename: {leftover:?}"
        );
    }

    #[test]
    fn redact_and_roundtrip_new_source_oauth_secrets() {
        // The OAuth-style source secrets (Ambient Weather app_key, Netatmo /
        // YoLink / Tuya client_secret + refresh_token) must be redacted on the
        // GET path and round-trip back on a PUT that sends the sentinel
        // unchanged. client_id is a PUBLIC identifier and must NOT be redacted.
        let original = serde_json::json!({
            "schema_version": 1,
            "sources": [{
                "id": "netatmo_main",
                "priority": 40,
                "enabled": true,
                "kind": "netatmo",
                "config": {
                    "client_id": "63abc_public_client_id",
                    "client_secret": "very_secret_client_secret_value",
                    "refresh_token": "rt_super_secret_refresh_token",
                    "device_id": "70:ee:50:00:11:22"
                }
            }, {
                "id": "ambient_main",
                "priority": 50,
                "enabled": true,
                "kind": "ambient_weather",
                "config": {
                    "app_key": "ambient_secret_app_key_zzz",
                    "api_key": "ambient_secret_api_key_yyy",
                    "mac_address": "AA:BB:CC:DD:EE:FF"
                }
            }]
        });

        // GET path: redaction hides every new secret but leaves client_id +
        // non-secret fields visible.
        let mut redacted = original.clone();
        redact_secrets(&mut redacted);
        let s = serde_json::to_string(&redacted).unwrap();
        assert!(
            !s.contains("very_secret_client_secret_value"),
            "client_secret leaked"
        );
        assert!(
            !s.contains("rt_super_secret_refresh_token"),
            "refresh_token leaked"
        );
        assert!(!s.contains("ambient_secret_app_key_zzz"), "app_key leaked");
        assert!(!s.contains("ambient_secret_api_key_yyy"), "api_key leaked");
        // client_id is public: it must survive verbatim.
        assert!(
            s.contains("63abc_public_client_id"),
            "client_id must NOT be redacted (public identifier)"
        );
        assert!(
            s.contains("70:ee:50:00:11:22"),
            "device_id unexpectedly redacted"
        );

        // PUT path: client sends the redacted JSON unchanged; unredact restores
        // every stored secret by sentinel match, leaving no sentinel behind.
        let mut candidate = redacted.clone();
        unredact_secrets(&mut candidate, &original);
        assert_eq!(
            candidate, original,
            "sentinel round-trip failed to restore new source secrets"
        );
        let mut leftover = Vec::new();
        remaining_sentinels(&candidate, "$", &mut leftover);
        assert!(leftover.is_empty(), "leftover sentinels: {leftover:?}");
    }

    /// Drive the real GET /api/config/source_catalog handler against a store
    /// seeded at a US point and return the parsed JSON body. Keeps the catalog
    /// assertions below exercising the actual handler (region recommendation,
    /// the flattened honest facts, and the runtime annotations) end to end.
    async fn source_catalog_json_at(lat: f64, lon: f64) -> serde_json::Value {
        let dir = std::env::temp_dir().join(format!(
            "localsky-source-catalog-test-{}-{}-{}",
            std::process::id(),
            (lat * 1000.0) as i64,
            (lon * 1000.0) as i64
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(FileConfigStore::new(dir.join("localsky.toml")));
        let mut cfg = Config::default();
        cfg.deployment.location.lat = lat;
        cfg.deployment.location.lon = lon;
        store.save(&cfg).await.unwrap();

        let state = ConfigApiState::store_only(store);
        let resp = get_source_catalog(State(state)).await.into_response();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// Build a `RuntimeHandles` with empty/default live state plus the two
    /// observation/reachability maps the catalog reads, so a test can wire a live
    /// runtime into `ConfigApiState` and exercise the honest-status path end to
    /// end (the store-only helper above has no runtime, so the obs-liveness branch
    /// is inert there). Mirrors the boot-time construction in main.rs.
    fn test_runtime_handles() -> (
        crate::runtime::RuntimeHandles,
        crate::sources::SourceReachability,
        crate::sources::SourceLastSeen,
    ) {
        use arc_swap::ArcSwap;
        let source_reachable = crate::sources::SourceReachability::default();
        let source_last_seen = crate::sources::SourceLastSeen::default();
        let handles = crate::runtime::RuntimeHandles {
            tempest_store: Arc::new(crate::tempest::state::TempestStore::new()),
            forecast_priority: Arc::new(ArcSwap::from_pointee(std::collections::HashMap::new())),
            watering_policy: Arc::new(ArcSwap::from_pointee(
                crate::refresher::WateringPolicy::default(),
            )),
            manual_schedules: Arc::new(ArcSwap::from_pointee(Vec::new())),
            source_reachable: source_reachable.clone(),
            source_last_seen: Some(source_last_seen.clone()),
        };
        (handles, source_reachable, source_last_seen)
    }

    #[tokio::test]
    async fn catalog_recently_observed_mrms_with_stale_reachability_is_not_offline() {
        // THE CONGRUENCE FIX (the OWNER-reported bug): a configured + enabled
        // NOAA MRMS that OBSERVED 280s ago but whose REACHABILITY epoch is stale
        // (the adapter sends Reachability only on state CHANGE, so a stably-
        // reachable MRMS carries a stale reachability epoch) must NOT read
        // `offline` in the catalog. Before the fix the catalog judged status off
        // the reachability epoch alone (>30 min stale -> offline) while /api/health
        // accepted the recent Observation as a liveness proof and read it calm, so
        // the two surfaces DISAGREED. With the obs-liveness input threaded in, the
        // catalog reads the SAME calm status as /api/health: `watching` (reachable
        // via the obs proof, owns nothing, not outranked).
        let dir = std::env::temp_dir().join(format!(
            "localsky-catalog-congruence-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(FileConfigStore::new(dir.join("localsky.toml")));

        // A US deployment with an enabled MRMS source.
        let mut cfg = Config::default();
        cfg.deployment.location.lat = 28.5;
        cfg.deployment.location.lon = -81.4;
        cfg.sources.push(crate::config::schema::SourceEntry {
            id: "mrms_main".into(),
            priority: 75,
            enabled: true,
            max_age_s: None,
            source: crate::config::schema::SourceKind::NoaaMrms(
                crate::config::schema::NoaaMrmsConfig::default(),
            ),
        });
        store.save(&cfg).await.unwrap();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let (handles, reachable, last_seen) = test_runtime_handles();
        // Reachability is STALE (1 hour ago, past the 30-min hard-offline window):
        // judged alone, this reads `offline`.
        reachable.record("mrms_main", now - 3600);
        // But the source OBSERVED 280s ago: within the kind-aware MRMS obs window
        // (10800s), this proves it is alive, exactly as /api/health treats it.
        last_seen.record("mrms_main", now - 280);

        let state = ConfigApiState {
            store,
            runtime: Some(handles),
        };
        let resp = get_source_catalog(State(state)).await.into_response();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let cloud = v["cloud_sources"].as_array().expect("cloud_sources array");
        let mrms = cloud
            .iter()
            .find(|e| e["kind"] == "noaa_mrms")
            .expect("NOAA MRMS in the catalog");
        let status = mrms["status"].as_str().expect("status string");
        assert_ne!(
            status, "offline",
            "a recently-observed MRMS with a stale reachability epoch must NOT read offline"
        );
        // It owns nothing in this store-only-merge posture and is not outranked, so
        // the obs proof lands it on the calm `watching`, congruent with /api/health.
        assert_eq!(
            status, "watching",
            "the obs-liveness proof reads the calm watching status, not offline"
        );
    }

    #[tokio::test]
    async fn source_catalog_exposes_noaa_mrms_radar_qpe_recommended_in_us() {
        // The honest catalog JSON at a US point (Orlando) must carry NOAA MRMS
        // with its per-rain nature RadarQpe (the flattened catalog fact) AND the
        // US region recommendation, and the new runtime annotations the UI reads
        // (region_appropriate + the upgrade marker). This is the data-model
        // contract for the rain-honesty UI, exercised through the real handler.
        let v = source_catalog_json_at(28.5, -81.4).await;
        let cloud = v["cloud_sources"].as_array().expect("cloud_sources array");

        // NOAA MRMS is present, with the honest RadarQpe rain nature + US recommend.
        let mrms = cloud
            .iter()
            .find(|e| e["kind"] == "noaa_mrms")
            .expect("NOAA MRMS in the catalog");
        assert_eq!(
            mrms["rain_nature"], "radar_qpe",
            "NOAA MRMS rain_nature is radar_qpe (flattened from cloud_meta)"
        );
        assert_eq!(
            mrms["data_nature"], "radar_qpe",
            "NOAA MRMS overall data_nature is radar_qpe too"
        );
        assert_eq!(
            mrms["recommended_here"], true,
            "NOAA MRMS is a keyless US authority, recommended at a US point"
        );
        assert_eq!(
            mrms["region_appropriate"], true,
            "NOAA MRMS is region-appropriate everywhere (US coverage gates recommend, not this)"
        );
        assert_eq!(
            mrms["region_priority"], 75,
            "NOAA MRMS seeds the US radar-QPE rank 75 (above NWS 70)"
        );
        // It carries no upgrade note (only Pirate does).
        assert_eq!(mrms["upgrade_available"], false);
        assert!(
            mrms.get("upgrade_reason")
                .map(|u| u.is_null())
                .unwrap_or(true),
            "NOAA MRMS has no upgrade_reason"
        );

        // Every cloud entry carries the honest source-status taxonomy string
        // (spec 1.6), congruent with /api/health (same `compute_source_status`).
        // In this store-only posture nothing is configured + nothing owns a
        // field, so each reads a valid enum string off the shared fn. CONTRACT
        // OUT for the UI agents: the field name is `status` and the value is one
        // of these five snake_case strings.
        const STATUS_WORDS: [&str; 5] = [
            "active",
            "watching",
            "standby",
            "falling_through",
            "offline",
        ];
        for e in cloud {
            let s = e["status"]
                .as_str()
                .unwrap_or_else(|| panic!("cloud entry {} missing string status field", e["kind"]));
            assert!(
                STATUS_WORDS.contains(&s),
                "status {s:?} on {} is not a taxonomy enum string",
                e["kind"]
            );
        }

        // NWS is recommended in the US and its rain is an honest Observation.
        let nws = cloud
            .iter()
            .find(|e| e["kind"] == "nws")
            .expect("NWS in the catalog");
        assert_eq!(nws["rain_nature"], "observation");
        assert_eq!(nws["recommended_here"], true);

        // Pirate carries the CONUS upgrade marker (so the UI can PROMOTE it
        // without auto-enabling), but its rain is honestly a Forecast and it is
        // never auto-recommended.
        let pirate = cloud
            .iter()
            .find(|e| e["kind"] == "pirate_weather")
            .expect("Pirate in the catalog");
        assert_eq!(
            pirate["rain_nature"], "forecast",
            "Pirate rain is a model forecast (the mislabel fix)"
        );
        assert_eq!(
            pirate["upgrade_available"], true,
            "Pirate carries the CONUS temp/wind upgrade marker"
        );
        assert!(
            pirate["upgrade_reason"].is_string(),
            "Pirate carries the honest upgrade line"
        );
        assert_eq!(
            pirate["recommended_here"], false,
            "a keyed provider is never auto-recommended"
        );

        // Met.no at a US point is NOT region-appropriate (coarse grid for a US
        // yard): the softer UI-collapse signal, distinct from recommend/enable.
        let metno = cloud
            .iter()
            .find(|e| e["kind"] == "met_norway")
            .expect("Met.no in the catalog");
        assert_eq!(
            metno["region_appropriate"], false,
            "Met.no is not region-appropriate at a US point"
        );
        assert_eq!(metno["recommended_here"], false);
    }

    #[tokio::test]
    async fn source_catalog_marks_metno_region_appropriate_in_the_nordics() {
        // The same Met.no entry IS region-appropriate at a Nordic point (Oslo),
        // and is recommended there (its keyless authority region), proving the
        // collapse signal is a function of the deployment location.
        let v = source_catalog_json_at(59.9, 10.75).await;
        let cloud = v["cloud_sources"].as_array().expect("cloud_sources array");
        let metno = cloud
            .iter()
            .find(|e| e["kind"] == "met_norway")
            .expect("Met.no in the catalog");
        assert_eq!(
            metno["region_appropriate"], true,
            "Met.no is region-appropriate in the Nordics"
        );
        assert_eq!(
            metno["recommended_here"], true,
            "Met.no is the recommended keyless authority in the Nordics"
        );
        // NOAA MRMS is NOT recommended outside the US.
        let mrms = cloud
            .iter()
            .find(|e| e["kind"] == "noaa_mrms")
            .expect("NOAA MRMS in the catalog");
        assert_eq!(
            mrms["recommended_here"], false,
            "NOAA MRMS is US-only, not recommended in the Nordics"
        );
    }

    #[tokio::test]
    async fn source_catalog_carries_per_field_natures_for_the_matrix() {
        // THE SEAM the capability-matrix Panel reads: each cloud entry carries a
        // `field_natures` array of [canonical_key, nature_string] pairs, one per
        // field in `live_current_fields` (same keys), tinting each LIT cell by its
        // own honesty. Exercised end to end through the real handler at a US point.
        // A distinct US point (Austin, not the Orlando the MRMS test uses) so the
        // location-keyed temp-dir harness never collides with a parallel test.
        let v = source_catalog_json_at(30.27, -97.74).await;
        let cloud = v["cloud_sources"].as_array().expect("cloud_sources array");

        // Helper: collect an entry's field_natures into a (key -> nature) map.
        let natures = |kind: &str| -> std::collections::HashMap<String, String> {
            let entry = cloud
                .iter()
                .find(|e| e["kind"] == kind)
                .unwrap_or_else(|| panic!("{kind} in the catalog"));
            let arr = entry["field_natures"]
                .as_array()
                .unwrap_or_else(|| panic!("{kind} field_natures is an array"));
            arr.iter()
                .map(|pair| {
                    let p = pair
                        .as_array()
                        .expect("field_natures entry is a 2-tuple array");
                    (
                        p[0].as_str().expect("field key string").to_string(),
                        p[1].as_str().expect("nature string").to_string(),
                    )
                })
                .collect()
        };

        // field_natures keys EXACTLY match live_current_fields (presence axis and
        // nature axis are the same key set), so the Panel never tints a cell it
        // cannot light or lights a cell it cannot tint.
        for e in cloud {
            let lit: std::collections::HashSet<&str> = e["live_current_fields"]
                .as_array()
                .expect("live_current_fields array")
                .iter()
                .map(|f| f.as_str().expect("field key"))
                .collect();
            let fnat: std::collections::HashSet<&str> = e["field_natures"]
                .as_array()
                .expect("field_natures array")
                .iter()
                .map(|p| p.as_array().unwrap()[0].as_str().unwrap())
                .collect();
            assert_eq!(
                lit, fnat,
                "{} field_natures keys match live_current_fields exactly",
                e["kind"]
            );
        }

        // Pirate: the per-field truth the single rain badge buries. Its wind reads
        // a live `nowcast` while its rain reads a model `forecast` in the SAME row.
        // Pirate emits the rain RATE (`rain_intensity_in_hr`) + POP, not a today
        // total, so assert the nature on the rain cells it actually lights.
        let pirate = natures("pirate_weather");
        assert_eq!(
            pirate.get("wind_mph").map(String::as_str),
            Some("nowcast"),
            "Pirate wind is a live nowcast cell"
        );
        assert_eq!(
            pirate.get("rain_intensity_in_hr").map(String::as_str),
            Some("forecast"),
            "Pirate rain rate is a model forecast cell, never a nowcast"
        );
        assert_eq!(
            pirate.get("pop").map(String::as_str),
            Some("forecast"),
            "Pirate POP is a model forecast cell"
        );

        // The cloud weather STATION tier is present and every emitted field is an
        // `observation` cell (a real station the user owns, cloud-routed).
        for kind in ["ambient_weather", "netatmo", "lacrosse"] {
            let m = natures(kind);
            assert!(!m.is_empty(), "{kind} emits matrix fields");
            for (key, nature) in &m {
                assert_eq!(
                    nature, "observation",
                    "{kind} {key} is a station observation cell"
                );
            }
            // Its overall data_nature is the honest Observation headline too.
            let entry = cloud.iter().find(|e| e["kind"] == kind).unwrap();
            assert_eq!(
                entry["data_nature"], "observation",
                "{kind} is an Observation tier"
            );
        }
    }

    #[tokio::test]
    async fn field_sources_carries_per_field_natures_pirate_split() {
        // The per-field nature badge (deferred #10): a chain candidate carries a
        // `field_natures` array of [field, nature] pairs so the client badges each
        // ROW by the FIELD it renders, not one source-level badge on every field.
        // A Pirate Weather source is a live NOWCAST for temp/wind but a model
        // FORECAST for rain, so its candidate must split accordingly. Exercised end
        // to end through the real GET /api/config/field_sources handler at a US
        // point (CONUS, where Pirate emits current scalars).
        let dir = std::env::temp_dir().join(format!(
            "localsky-field-sources-natures-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(FileConfigStore::new(dir.join("localsky.toml")));
        let mut cfg = Config::default();
        cfg.deployment.location.lat = 30.27;
        cfg.deployment.location.lon = -97.74;
        cfg.sources.push(crate::config::schema::SourceEntry {
            id: "pirate_main".into(),
            priority: 60,
            enabled: true,
            max_age_s: None,
            source: crate::config::schema::SourceKind::PirateWeather(
                crate::config::schema::PirateWeatherConfig {
                    api_key: "test".into(),
                },
            ),
        });
        store.save(&cfg).await.unwrap();

        let state = ConfigApiState::store_only(store);
        let resp = get_field_sources(State(state)).await.into_response();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let sources = v["sources"].as_array().expect("sources array");
        let pirate = sources
            .iter()
            .find(|s| s["id"] == "pirate_main")
            .expect("Pirate candidate present");

        // The flat source-level nature is the headline Nowcast (the fallback).
        assert_eq!(
            pirate["nature"], "nowcast",
            "the flat fallback nature is Pirate's headline nowcast"
        );

        // field_natures keys EXACTLY match the candidate `fields` (same key set),
        // so the client always resolves the field it is rendering.
        let fields: std::collections::HashSet<&str> = pirate["fields"]
            .as_array()
            .expect("fields array")
            .iter()
            .map(|f| f.as_str().expect("field key"))
            .collect();
        let fnat_map: std::collections::HashMap<String, String> = pirate["field_natures"]
            .as_array()
            .expect("field_natures array")
            .iter()
            .map(|pair| {
                let p = pair
                    .as_array()
                    .expect("field_natures entry is a 2-tuple array");
                (
                    p[0].as_str().expect("field key").to_string(),
                    p[1].as_str().expect("nature string").to_string(),
                )
            })
            .collect();
        let fnat_keys: std::collections::HashSet<&str> =
            fnat_map.keys().map(String::as_str).collect();
        assert_eq!(
            fields, fnat_keys,
            "field_natures keys match the candidate fields exactly"
        );

        // The SPLIT: Pirate under Temperature/Wind is a live nowcast; Pirate under
        // Rain is a model forecast, in the SAME candidate.
        assert_eq!(
            fnat_map.get("air_temp_f").map(String::as_str),
            Some("nowcast"),
            "Pirate temp is a live nowcast"
        );
        assert_eq!(
            fnat_map.get("wind_mph").map(String::as_str),
            Some("nowcast"),
            "Pirate wind is a live nowcast"
        );
        assert_eq!(
            fnat_map.get("rain_intensity_in_hr").map(String::as_str),
            Some("forecast"),
            "Pirate rain rate is a model forecast, never a nowcast"
        );
        assert_eq!(
            fnat_map.get("pop").map(String::as_str),
            Some("forecast"),
            "Pirate POP is a model forecast"
        );
    }

    #[tokio::test]
    async fn field_sources_device_natures_are_all_device() {
        // A live LAN station MEASURES every field, so its candidate's field_natures
        // are uniformly "device" (and the flat fallback is "device" too). This is
        // the trivial-but-load-bearing invariant: the per-field split never demotes
        // a real sensor's badge.
        let dir = std::env::temp_dir().join(format!(
            "localsky-field-sources-device-natures-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(FileConfigStore::new(dir.join("localsky.toml")));
        let mut cfg = Config::default();
        cfg.deployment.location.lat = 30.27;
        cfg.deployment.location.lon = -97.74;
        cfg.sources.push(crate::config::schema::SourceEntry {
            id: "tempest_lan".into(),
            priority: 100,
            enabled: true,
            max_age_s: None,
            source: crate::config::schema::SourceKind::TempestUdp(
                crate::config::schema::TempestUdpConfig {
                    bind_addr: "0.0.0.0:50222".into(),
                    hub_serial: None,
                },
            ),
        });
        store.save(&cfg).await.unwrap();

        let state = ConfigApiState::store_only(store);
        let resp = get_field_sources(State(state)).await.into_response();
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        let sources = v["sources"].as_array().expect("sources array");
        let tempest = sources
            .iter()
            .find(|s| s["id"] == "tempest_lan")
            .expect("Tempest candidate present");
        assert_eq!(tempest["nature"], "device", "a LAN station is a device");
        let fnat = tempest["field_natures"]
            .as_array()
            .expect("field_natures array");
        assert!(!fnat.is_empty(), "a station emits fields");
        for pair in fnat {
            let p = pair.as_array().expect("2-tuple");
            assert_eq!(
                p[1].as_str(),
                Some("device"),
                "every station field is a measured device reading"
            );
        }
    }

    #[test]
    fn new_entry_with_sentinel_is_flagged_not_silently_saved() {
        let original = cfg_with_secrets();
        let mut candidate = original.clone();
        redact_secrets(&mut candidate);
        // User added a brand-new source but left the secret field as
        // the redaction placeholder.
        candidate["sources"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "id": "brand_new",
                "priority": 10,
                "enabled": true,
                "kind": "mqtt",
                "config": { "broker_host": "x", "broker_port": 1883,
                            "username": "u", "password": SECRET_REDACTED_SENTINEL,
                            "subscriptions": [] }
            }));
        unredact_secrets(&mut candidate, &original);
        let mut leftover = Vec::new();
        remaining_sentinels(&candidate, "$", &mut leftover);
        assert_eq!(leftover.len(), 1, "exactly the new entry's secret flagged");
        assert!(
            leftover[0].contains("brand_new"),
            "path names the entry: {leftover:?}"
        );
    }
}
