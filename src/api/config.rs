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
        // Tuning-report Apply: a single-field ZoneConfig write. Lives under
        // /api/config so it inherits the privileged gate, the CSRF check,
        // and the body cap with zero per-handler auth code.
        .route("/zones/apply", post(post_zones_apply))
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
    /// authenticated owner identity; ignored otherwise. Read as a STRING,
    /// see [`query_flag`]: a bare `bool` here rejected the documented
    /// `?reveal=1` outright.
    #[serde(default)]
    reveal: Option<String>,
}

/// True for the spellings an operator actually types into a URL.
///
/// Axum's `Query` deserializes through `serde_urlencoded`, whose `bool`
/// impl is `str::parse::<bool>()`, and Rust's `FromStr for bool` accepts
/// ONLY "true"/"false". So a query flag typed as `=1` does not merely read
/// as false: it fails the whole extraction and answers `400` before the
/// handler runs. That is fatal for `allow_zone_key_change`, whose own `422`
/// refusal message tells the user to type `=1`. Taking the value as a
/// string and deciding truthiness here accepts every spelling the docs, the
/// changelog, and the error message promise, plus a bare flag with no `=`.
fn query_flag(v: Option<&String>) -> bool {
    match v {
        // `?flag` with no `=` arrives as an empty value: treat it as set.
        Some(s) => {
            let s = s.trim().to_ascii_lowercase();
            s.is_empty() || matches!(s.as_str(), "1" | "true" | "yes" | "on")
        }
        None => false,
    }
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
    let reveal = query_flag(q.reveal.as_ref()) && is_owner;
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

/// Query flags shared by both whole-config write paths, `PUT /api/config`
/// and `PUT /api/config/raw`.
#[derive(Debug, Default, Deserialize)]
struct ConfigSaveParams {
    /// Save even though the zone key set changed in a way that looks like a
    /// rename. The refusal message explains what a rename breaks and names
    /// this flag; nothing sets it automatically.
    ///
    /// A STRING, not a `bool`: see [`query_flag`]. The `422` detail tells
    /// the user to type `?allow_zone_key_change=1`, and a bare `bool` would
    /// answer `400` to exactly that.
    #[serde(default)]
    allow_zone_key_change: Option<String>,
}

impl ConfigSaveParams {
    fn allows_zone_key_change(&self) -> bool {
        query_flag(self.allow_zone_key_change.as_ref())
    }
}

/// Describe a zone-key change that looks like a RENAME, or `None` when the
/// change is safe to save.
///
/// A zone slug is the primary key of everything per-zone: run history, the
/// sticky auto/skip/run override, the commanded-valve ledger the deadline
/// reaper disarms against, tuning dismissals, the soil channel id inside
/// sensor_history, nine Home Assistant entity ids, three RETAINED MQTT
/// discovery topics with no clear path, and the /zones/<slug> URL every push
/// notification has ever linked to. The structured editor makes the slug
/// read-only for that reason, and BOTH whole-config write paths run this
/// guard: `PUT /api/config`, which any API client or restore script can
/// reshape, and `PUT /api/config/raw`.
///
/// A rename is indistinguishable from "deleted one zone and added another"
/// by definition, so the guard fires on that shape: at least one key gone
/// AND at least one key new. Pure deletion and pure addition both pass.
fn zone_key_rename_detail(
    stored: &std::collections::BTreeSet<String>,
    incoming: &std::collections::BTreeSet<String>,
) -> Option<String> {
    let removed: Vec<&str> = stored.difference(incoming).map(String::as_str).collect();
    let added: Vec<&str> = incoming.difference(stored).map(String::as_str).collect();
    if removed.is_empty() || added.is_empty() {
        return None;
    }
    Some(format!(
        "this save drops the zone key(s) {} and adds {}. A zone's slug is permanent: it is \
         the key its run history, its auto/skip/run override, its in-flight run ledger, its \
         tuning dismissals, its soil sensor channel, its Home Assistant entity ids, its \
         retained MQTT discovery topics, and its /zones/<slug> links are all stored under. \
         Renaming it here orphans every one of them with no way back. To change what a zone \
         is CALLED, edit its display_name and leave the key alone. If these really are two \
         unrelated zones (one deleted, one added), save the removal and the addition as two \
         separate saves, or repeat this one with ?allow_zone_key_change=1.",
        removed
            .iter()
            .map(|k| format!("\"{k}\""))
            .collect::<Vec<_>>()
            .join(", "),
        added
            .iter()
            .map(|k| format!("\"{k}\""))
            .collect::<Vec<_>>()
            .join(", ")
    ))
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
    Query(params): Query<ConfigSaveParams>,
    body: String,
) -> impl IntoResponse {
    // Same read-modify-write guard as PUT /: the stored-config load below
    // feeds the unredaction, so a concurrent writer must queue.
    let _write_guard = store.begin_write().await;
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
        // Unlike PUT /api/config, the raw path has NO __renames hint (the
        // editor is free text; neither it nor we can tell a renamed id from
        // a brand-new entry), so renaming a source/controller id here
        // always strands that entry's sentinels and lands in this branch.
        // Adding rename logic is not wanted; instead the message must be
        // honest about WHY and name the two paths that actually work.
        return (
            StatusCode::BAD_REQUEST,
            Json(ApiError {
                error: "unmatched_redacted_secret".into(),
                detail: Some(format!(
                    "redacted placeholder(s) with no stored value at: {}. If you renamed a \
                     source or controller id in the raw TOML, its stored secrets cannot be \
                     matched from the redacted text. Either rename it in Settings > Devices \
                     (which migrates its secrets and references automatically), or paste the \
                     real secret value in place of each {} placeholder under the renamed \
                     entry.",
                    leftover.join(", "),
                    SECRET_REDACTED_SENTINEL
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

    // The migration ledger is server-owned on this path too: raw TOML with no
    // `[[ha_adoption]]` block would otherwise un-retire every read. See the
    // same restore in `put_config`.
    parsed.ha_adoption = original
        .as_ref()
        .and_then(|v| v.get("ha_adoption"))
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    // `seeded_source_ids` is the same class of ledger, with a milder failure:
    // dropping an id lets the next boot re-seed a forecast authority the owner
    // deleted. It rides across as a UNION rather than an overwrite, because
    // this editor CAN legitimately add one by hand, and matching
    // `post_rollback` here means every whole-config write path treats both
    // ledgers the same way.
    if let Some(prev) = original
        .as_ref()
        .and_then(|v| v.get("seeded_source_ids"))
        .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
    {
        for id in prev {
            if !parsed.seeded_source_ids.contains(&id) {
                parsed.seeded_source_ids.push(id);
            }
        }
    }

    // A zone slug is a primary key, not a label, and the raw editor is the
    // one path that can change one. Refuse a save that drops a zone key and
    // introduces another in the same write, which is what a rename looks
    // like from here. `?allow_zone_key_change=1` is the deliberate override
    // for the case where the removal and the addition really are unrelated.
    if !params.allows_zone_key_change() {
        let stored_zone_keys: std::collections::BTreeSet<String> = original
            .as_ref()
            .and_then(|v| v.get("zones"))
            .and_then(|z| z.as_object())
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        let incoming_zone_keys: std::collections::BTreeSet<String> =
            parsed.zones.keys().cloned().collect();
        if let Some(detail) = zone_key_rename_detail(&stored_zone_keys, &incoming_zone_keys) {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(ApiError {
                    error: "zone_key_renamed".into(),
                    detail: Some(detail),
                }),
            )
                .into_response();
        }
    }

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
                "detail": report.error_summary(),
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
        // A fresh install has no config file yet. Serve the DEFAULT config
        // (200) instead of a 404: the settings panes are read-modify-write,
        // so a 404 here locked every pane ("editing disabled until the load
        // succeeds") and made first-time configuration through Settings
        // impossible; editing the served defaults and saving materializes
        // the file, which is exactly what a fresh-install settings edit
        // should do. "Is this instance set up yet" remains answerable via
        // /api/wizard/state; nothing should infer it from a 404 here.
        Err(ConfigStoreError::NotFound) => {
            let cfg = crate::config::Config::default();
            match serde_json::to_value(&cfg) {
                Ok(v) => Json(v).into_response(),
                Err(e) => store_err(ConfigStoreError::Io(format!("serialize default: {e}")))
                    .into_response(),
            }
        }
        Err(e) => store_err(e).into_response(),
    }
}

/// In-place mutation that replaces every known secret-bearing string
/// with a SECRET_REDACTED_SENTINEL. Conservative: false positives are
/// preferable to leaking a token. The PUT handler accepts the sentinel
/// and preserves the existing stored value.
pub const SECRET_REDACTED_SENTINEL: &str = "***redacted***";

/// A configured source/controller URL carries a credential when it has
/// userinfo (`user:pass@host`) or a secret-looking query parameter (RestPoll's
/// docs say "put query-param API keys in the url"). Used to decide whether GET
/// /api/config must redact the WHOLE url. Conservative: a substring match on the
/// query is fine, a false positive only hides a non-secret url, which still
/// round-trips through PUT via the by-id unredact.
fn url_carries_secret(url: &str) -> bool {
    if let Some((_, rest)) = url.split_once("://") {
        // Userinfo is everything before '@' within the authority (before the
        // first '/').
        let authority = rest.split('/').next().unwrap_or("");
        if authority.contains('@') {
            return true;
        }
    }
    if let Some((_, query)) = url.split_once('?') {
        let q = query.to_ascii_lowercase();
        const SECRET_PARAMS: &[&str] = &[
            "api_key",
            "apikey",
            "app_key",
            "appid",
            "access_token",
            "token",
            "auth",
            "secret",
            "password",
            "passwd",
            "pwd",
            "sig",
            "signature",
            "key=",
        ];
        return SECRET_PARAMS.iter().any(|p| q.contains(p));
    }
    false
}

/// A request-header name whose VALUE should be treated as secret (Authorization,
/// X-Api-Key, *-Token, Cookie, ...). Innocuous headers (Content-Type, Accept)
/// stay visible. Conservative substring matching (false positives acceptable).
fn header_name_is_secret(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.contains("authorization")
        || n.contains("cookie")
        || n.contains("token")
        || n.contains("secret")
        || n.contains("auth")
        || n.contains("password")
        || n.contains("credential")
        || n.contains("key")
}

pub(crate) fn redact_secrets(v: &mut serde_json::Value) {
    use serde_json::Value;
    match v {
        Value::Object(map) => {
            for (k, val) in map.iter_mut() {
                let lk = k.to_lowercase();
                // Value/context-aware redaction for the generic-HTTP source
                // configs (RestPoll / HttpGeneric / Prometheus / InfluxDb): a
                // secret can ride inside the URL query string, a request HEADER
                // value, or a request BODY, none of which the key-name allowlist
                // below catches. Whole-value redaction (never partial) so the
                // by-id unredact round-trip on PUT restores it cleanly.
                match lk.as_str() {
                    "url" => {
                        if let Value::String(s) = val {
                            if url_carries_secret(s) {
                                *s = SECRET_REDACTED_SENTINEL.to_string();
                            }
                            continue;
                        }
                    }
                    "headers" => {
                        if let Value::Object(hmap) = val {
                            for (hk, hv) in hmap.iter_mut() {
                                if header_name_is_secret(hk) {
                                    if let Value::String(s) = hv {
                                        if !s.is_empty() {
                                            *s = SECRET_REDACTED_SENTINEL.to_string();
                                        }
                                    }
                                }
                            }
                            continue;
                        }
                    }
                    "body" => {
                        // A POST body can carry a credential (OAuth
                        // client_secret, a form password) and cannot be safely
                        // parsed, so redact it wholesale when non-empty.
                        if let Value::String(s) = val {
                            if !s.is_empty() {
                                *s = SECRET_REDACTED_SENTINEL.to_string();
                            }
                            continue;
                        }
                    }
                    _ => {}
                }
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
                    || lk == "email"
                    // Cloud OAuth/API-key PAIR IDENTIFIER: YoLink (the UAID),
                    // Tuya (the access_id) and Netatmo all authenticate with a
                    // client_id + client_secret pair. The secret half is
                    // already redacted above; leaving client_id in the clear
                    // half-leaks the credential, the same gap the account
                    // `email` entry closes for the password-based cloud
                    // controllers. The MQTT source/command client_id (a broker
                    // session name, not a credential) is swept up too; that is
                    // the same conservative trade `username` already makes, and
                    // the id-keyed unredact restores it losslessly on PUT.
                    || lk == "client_id";
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
    Query(params): Query<ConfigSaveParams>,
    Json(mut candidate_json): Json<serde_json::Value>,
) -> impl IntoResponse {
    // Serialize the whole read-modify-write against every other config
    // writer (raw PUT, rollback, soil removal, tuning apply): the load
    // below feeds the redaction round-trip and the restart diff, so a
    // concurrent writer landing between this load and the save would
    // otherwise be silently clobbered.
    let _write_guard = store.begin_write().await;
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
    // `ha_adoption` is SERVER-OWNED: only the one-time adoption pass writes it.
    // It carries `#[serde(default)]`, so a body that omits the key (a restore
    // script, an older UI build, any API client, and the settings pages
    // themselves if they ever drop it) deserializes to an empty vec, persists
    // a config with no markers, and arc-swaps a WateringPolicy whose
    // `ha_read_retired` is false for all seven. Every retired read goes live
    // again against helpers the migration notice invited the owner to delete:
    // the vacation pause falls to `.unwrap_or(0)`, both toggles to false, and
    // the pass is disarmed for the life of the process, so nothing repairs it
    // until a restart. Restore it from the stored config, the same treatment
    // secrets already get from `original` above. A straight overwrite rather
    // than a union: only the pass appends, and a union would let a client
    // inject a marker that retires a read no pass ever handled.
    cfg.ha_adoption = original
        .get("ha_adoption")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    // `seeded_source_ids` is the same class of ledger, with a milder failure:
    // dropping an id lets the next boot re-seed a forecast authority the owner
    // deleted. Union rather than overwrite, matching `post_rollback`, so every
    // whole-config write path treats both ledgers the same way.
    if let Some(prev) = original
        .get("seeded_source_ids")
        .and_then(|v| serde_json::from_value::<Vec<String>>(v.clone()).ok())
    {
        for id in prev {
            if !cfg.seeded_source_ids.contains(&id) {
                cfg.seeded_source_ids.push(id);
            }
        }
    }
    // Auto-mark the sole controller as default when none is set, exactly as the
    // wizard's finalize_for_apply does before its own save. Without this, a
    // settings editor that PUTs a single controller left at `default = false`
    // (the editor just showed it valid) 422s here on the "at least one
    // controller must have default = true" gate. Idempotent: a no-op when a
    // default already exists or there are 0 / 2+ controllers.
    // The same zone-key guard the raw path runs. This is a documented,
    // privileged, WHOLE-CONFIG write, so an API client, a restore script, or
    // a front-end regression that reshapes `zones` renames a key here just as
    // easily as a hand edit does in the raw editor. `original` was already
    // loaded above for unredaction; it is Null on a first install, whose
    // `.get("zones")` yields None and reads correctly as an empty stored set
    // (a pure addition, which passes).
    if !params.allows_zone_key_change() {
        let stored_zone_keys: std::collections::BTreeSet<String> = original
            .get("zones")
            .and_then(|z| z.as_object())
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        let incoming_zone_keys: std::collections::BTreeSet<String> =
            cfg.zones.keys().cloned().collect();
        if let Some(detail) = zone_key_rename_detail(&stored_zone_keys, &incoming_zone_keys) {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(ApiError {
                    error: "zone_key_renamed".into(),
                    detail: Some(detail),
                }),
            )
                .into_response();
        }
    }
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
                "detail": report.error_summary(),
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

/// POST /api/config/zones/apply body: one tuning recommendation to write.
#[derive(Debug, Deserialize)]
pub(crate) struct ZoneApplyBody {
    pub zone_slug: String,
    /// The recommendation id the client is acting on; the server
    /// regenerates the zone's recommendation and refuses (409) when the
    /// id no longer derives from current data.
    pub recommendation_id: String,
    /// ZoneConfig field the client believes it is applying; must match
    /// the regenerated recommendation.
    pub field: String,
    /// The value the client is applying (JSON; null clears an override).
    #[serde(default)]
    pub value: serde_json::Value,
    /// The report window (days) the recommendation was served at; the
    /// server re-derives at this window (clamped 7..=30 like the GET).
    /// Absent = the default window. Window-dependent checks produce
    /// different suggestions at different windows, so a client viewing a
    /// non-default window must echo the report's `window_days` or its
    /// apply can 409 forever.
    #[serde(default)]
    pub window_days: Option<u32>,
}

/// JSON value equality with numeric tolerance: serde_json distinguishes
/// integer 3 from float 3.0, but a client echoing a suggestion back must
/// not go stale over the representation.
fn json_values_match(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    if a == b {
        return true;
    }
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) => (x - y).abs() < 1e-9,
        _ => false,
    }
}

/// Find the zone's CURRENT recommendation in a freshly generated report
/// and check the client's claim (id + field + value) still derives.
/// Err carries the plain detail for the 409 body.
pub(crate) fn verify_recommendation(
    report: &crate::history::types::TuningReport,
    body: &ZoneApplyBody,
) -> Result<crate::history::types::TuningRecommendation, String> {
    let want = body.zone_slug.replace('-', "_");
    let zone = report
        .zones
        .iter()
        .find(|z| z.slug.replace('-', "_") == want)
        .ok_or_else(|| {
            format!(
                "zone '{}' is not in the current tuning report",
                body.zone_slug
            )
        })?;
    let rec = zone.recommendation.as_ref().ok_or_else(|| {
        "this zone no longer has a recommendation; refresh the tuning report".to_string()
    })?;
    if rec.id != body.recommendation_id
        || rec.field != body.field
        || !json_values_match(&rec.suggested_value, &body.value)
    {
        return Err(
            "the recommendation changed since this page loaded; refresh the tuning report"
                .to_string(),
        );
    }
    Ok(rec.clone())
}

/// Write one recommendation field into a ZoneConfig. Returns the field's
/// previous value as JSON. Only the fields the tuning checks recommend
/// are writable here; anything else is refused (the full editor is the
/// PUT /api/config path).
pub(crate) fn apply_zone_field(
    zone: &mut crate::config::schema::ZoneConfig,
    field: &str,
    value: &serde_json::Value,
) -> Result<serde_json::Value, String> {
    use serde_json::json;
    fn opt_f64(value: &serde_json::Value, field: &str) -> Result<Option<f64>, String> {
        match value {
            serde_json::Value::Null => Ok(None),
            v => v
                .as_f64()
                .map(Some)
                .ok_or_else(|| format!("{field} expects a number or null")),
        }
    }
    match field {
        "weekly_budget_in" => {
            let old = json!(zone.weekly_budget_in);
            zone.weekly_budget_in = opt_f64(value, field)?;
            Ok(old)
        }
        "sessions_per_week" => {
            let old = json!(zone.sessions_per_week);
            zone.sessions_per_week = match value {
                serde_json::Value::Null => None,
                v => {
                    let n = v
                        .as_u64()
                        .and_then(|n| u32::try_from(n).ok())
                        .ok_or_else(|| format!("{field} expects a whole number or null"))?;
                    // 1..=7 is the range the spacing gate can resolve. The
                    // allocator paces sessions at floor(7/sessions) days, so
                    // 8 or more collapses that to 0 days and the gate stops
                    // holding a zone that already watered today: it re-plans
                    // it on the next tick. Told, not silently coerced: a
                    // number the user typed and cannot see changed is its own
                    // kind of wrong answer.
                    if !(1..=7).contains(&n) {
                        return Err(format!(
                            "{field} must be between 1 and 7; a zone cannot water more \
                             often than once a day"
                        ));
                    }
                    Some(n)
                }
            };
            Ok(old)
        }
        // Future-proofing, the mad_pct_override arm's precedent: no
        // tuning check emits a `rain_credit_cap_in` recommendation yet,
        // so `verify_recommendation` never lets the tuning report's
        // Apply reach this arm today. It stands ready for a check that
        // suggests a cap override, with the band already enforced.
        "rain_credit_cap_in" => {
            let old = json!(zone.rain_credit_cap_in);
            zone.rain_credit_cap_in = match opt_f64(value, field)? {
                None => None,
                Some(v) => {
                    // Same band the validator enforces; refusing here gives
                    // the apply caller a field-named message instead of a
                    // report. Null clears the override back to the cap
                    // derived from soil texture and root depth.
                    if !(0.05..=5.0).contains(&v) {
                        return Err(format!("{field} must be between 0.05 and 5.0 inches"));
                    }
                    Some(v)
                }
            };
            Ok(old)
        }
        "root_depth_mm" => {
            let old = json!(zone.root_depth_mm);
            zone.root_depth_mm = opt_f64(value, field)?;
            Ok(old)
        }
        "mad_pct_override" => {
            let old = json!(zone.mad_pct_override);
            zone.mad_pct_override = opt_f64(value, field)?;
            Ok(old)
        }
        "precip_rate_mm_hr" => {
            let old = json!(zone.precip_rate_mm_hr);
            zone.precip_rate_mm_hr = opt_f64(value, field)?;
            Ok(old)
        }
        "precip_rate_source" => {
            let old = serde_json::to_value(zone.precip_rate_source).unwrap_or_default();
            zone.precip_rate_source =
                serde_json::from_value(value.clone()).map_err(|e| format!("{field}: {e}"))?;
            Ok(old)
        }
        "soil_texture" => {
            let old = serde_json::to_value(zone.soil_texture).unwrap_or_default();
            zone.soil_texture =
                serde_json::from_value(value.clone()).map_err(|e| format!("{field}: {e}"))?;
            Ok(old)
        }
        // The per-zone scheduling-model pin. Null clears the override so
        // the engine default governs again; a string must parse as one of
        // the model variants ("weekly" | "soil"), same enum gate as
        // `soil_texture`. No tuning check recommends this field yet; the
        // arm stands ready (the rain_credit_cap_in precedent) and serves
        // the flip banner's per-zone "Keep weekly" writeback.
        "scheduling_model" => {
            let old = serde_json::to_value(zone.scheduling_model).unwrap_or_default();
            zone.scheduling_model = match value {
                serde_json::Value::Null => None,
                v => Some(serde_json::from_value(v.clone()).map_err(|e| format!("{field}: {e}"))?),
            };
            Ok(old)
        }
        "max_run_minutes" => {
            let old = json!(zone.max_run_minutes);
            zone.max_run_minutes = match value {
                serde_json::Value::Null => None,
                v => {
                    let m = v
                        .as_u64()
                        .and_then(|n| u32::try_from(n).ok())
                        .ok_or_else(|| format!("{field} expects a whole number or null"))?;
                    // Same band the validator enforces; refusing here gives the
                    // apply caller a field-named message instead of a report.
                    if !(5..=360).contains(&m) {
                        return Err(format!("{field} must be between 5 and 360 minutes"));
                    }
                    Some(m)
                }
            };
            Ok(old)
        }
        other => Err(format!(
            "field '{other}' cannot be applied from the tuning report"
        )),
    }
}

/// POST /api/config/zones/apply: write one tuning recommendation through
/// the validated config path. The WHOLE sequence runs under the store's
/// read-modify-write guard with a SINGLE config load: the recommendation
/// is re-derived against the exact config about to be mutated (409 when
/// the client's claim no longer derives), then mutate -> validate ->
/// save -> hot-reload, the same pipeline as PUT / minus the redaction
/// round-trip (the real config is loaded here, no sentinels involved).
/// The report window comes from the client (the window it fetched the
/// recommendation at); window-dependent checks derive different values
/// at different windows, so regenerating at any other window would 409
/// forever.
async fn post_zones_apply(
    State(ConfigApiState { store, runtime }): State<ConfigApiState>,
    Json(body): Json<ZoneApplyBody>,
) -> impl IntoResponse {
    let Some(handles) = crate::tuning::handles() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "tuning_unavailable",
                "detail": "tuning report requires the history database",
            })),
        )
            .into_response();
    };
    // Serialize against every other config read-modify-write sequence
    // (PUT /, PUT /raw, rollback, soil removal): held from the single
    // load through the save, so a concurrent whole-config save can
    // neither be clobbered by this apply nor invalidate the verification.
    let _write_guard = store.begin_write().await;
    let mut cfg = match store.load().await {
        Ok(c) => c,
        Err(e) => return store_err(e).into_response(),
    };
    let window_days = body
        .window_days
        .unwrap_or(crate::engine::tuning::DEFAULT_WINDOW_DAYS);
    let report = match crate::tuning::generate_report_with(handles, &cfg, window_days).await {
        Ok(r) => r,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": "tuning_generation_failed",
                    "detail": e.to_string(),
                })),
            )
                .into_response();
        }
    };
    let rec = match verify_recommendation(&report, &body) {
        Ok(r) => r,
        Err(detail) => {
            return (
                StatusCode::CONFLICT,
                Json(serde_json::json!({
                    "error": "stale_recommendation",
                    "detail": detail,
                })),
            )
                .into_response();
        }
    };
    let prev_cfg = cfg.clone();
    // Config keys may be dashed while runtime slugs are underscored; try
    // the slug as given, then the dashed variant (build_cycle_plan's dual
    // lookup, mirrored).
    let zone_key = if cfg.zones.contains_key(&body.zone_slug) {
        Some(body.zone_slug.clone())
    } else {
        let dashed = body.zone_slug.replace('_', "-");
        cfg.zones.contains_key(&dashed).then_some(dashed)
    };
    let Some(zone_key) = zone_key else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "zone_not_found",
                "detail": format!("no configured zone matches '{}'", body.zone_slug),
            })),
        )
            .into_response();
    };
    let zone = cfg
        .zones
        .get_mut(&zone_key)
        .expect("key existence checked above");
    let old_value = match apply_zone_field(zone, &rec.field, &rec.suggested_value) {
        Ok(v) => v,
        Err(detail) => {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(serde_json::json!({
                    "error": "config_invalid",
                    "detail": detail,
                })),
            )
                .into_response();
        }
    };
    // Companion fields ride the same Apply server-side (a measured precip
    // rate also stamps precip_rate_source), regardless of what the client
    // sent: the recommendation is the contract.
    for comp in &rec.companion_fields {
        if let Err(detail) = apply_zone_field(zone, &comp.field, &comp.value) {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(serde_json::json!({
                    "error": "config_invalid",
                    "detail": detail,
                })),
            )
                .into_response();
        }
    }
    let validation = crate::config::validate::validate(&cfg);
    if !validation.ok() {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({
                "error": "config_invalid",
                "detail": validation.error_summary(),
                "validation": validation,
            })),
        )
            .into_response();
    }
    match store.save(&cfg).await {
        Ok(saved) => {
            let outcome = apply_runtime_config_if_live(&runtime, Some(&prev_cfg), &cfg);
            tracing::info!(
                zone = %zone_key,
                field = %rec.field,
                old = %old_value,
                new = %rec.suggested_value,
                recommendation = %rec.id,
                "tuning recommendation applied"
            );
            Json(serde_json::json!({
                "applied": true,
                "zone": zone_key,
                "field": rec.field,
                "old_value": old_value,
                "new_value": rec.suggested_value,
                "saved": saved,
                "validation": validation,
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
    // Same read-modify-write guard as PUT /: the pre-rollback load feeds
    // the restart diff, and the swap itself must not interleave with a
    // concurrent writer's load-mutate-save.
    let _write_guard = store.begin_write().await;
    // Pre-rollback config for the restart-required diff (best-effort; a missing
    // current config simply skips the diff and reloads only the tunables).
    let prev_cfg = store.load().await.ok();
    match store.rollback(ts).await {
        Ok(mut cfg) => {
            // `ha_adoption` and `seeded_source_ids` are migration LEDGERS, not
            // tunables, so they ride across a rollback rather than being
            // restored to whatever the snapshot happened to hold. For the
            // adoption record that is a watering matter: the notice invites
            // the owner to delete the helpers, so un-retiring a read points it
            // at nothing, and a pause set through LocalSky after the migration
            // reads as no pause. The pass is disarmed for the life of the
            // process, so nothing re-marks until a restart.
            let mut carried = false;
            if let Some(prev) = prev_cfg.as_ref() {
                for h in &prev.ha_adoption {
                    if !cfg.ha_adoption.iter().any(|x| x.entity == h.entity) {
                        cfg.ha_adoption.push(h.clone());
                        carried = true;
                    }
                }
                for id in &prev.seeded_source_ids {
                    if !cfg.seeded_source_ids.contains(id) {
                        cfg.seeded_source_ids.push(id.clone());
                        carried = true;
                    }
                }
            }
            if carried {
                // Best effort: the in-memory cfg below is what the policy swap
                // and the response use either way, so a failed write degrades
                // to the pre-carry behavior on the NEXT boot rather than on
                // this request.
                if let Err(e) = crate::ports::config_store::ConfigStore::save(&*store, &cfg).await {
                    tracing::warn!(error = %e, "rollback: could not persist the carried migration ledger");
                }
            }
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

    // ─────────────────────────────────────────────────────────────
    // `ha_adoption` is a migration LEDGER, not a tunable. Every whole-config
    // write path has to carry it forward: a body that omits the key would
    // otherwise persist a config with no markers, arc-swap a policy whose
    // `ha_read_retired` is false for all seven, and put every retired read
    // back on helpers the migration notice invited the owner to delete. The
    // pass is disarmed for the life of the process, so nothing repairs it
    // until a restart.
    // ─────────────────────────────────────────────────────────────

    fn ledger_dir(tag: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("localsky-cfg-ledger-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn marker(entity: &str) -> crate::ha::snapshot::HaAdoptedHelper {
        crate::ha::snapshot::HaAdoptedHelper {
            entity: entity.to_string(),
            outcome: "adopted".into(),
            target: crate::ha_adopt::target_of(entity).into(),
            adopted_value: Some("on".into()),
            observed_value: None,
            previous_value: Some("off".into()),
            epoch: 1_780_000_000,
        }
    }

    /// A minimally valid config with the pass already committed.
    async fn adopted_store(tag: &str) -> (Arc<FileConfigStore>, Config) {
        let store = Arc::new(FileConfigStore::new(ledger_dir(tag).join("localsky.toml")));
        let mut cfg = Config::default();
        cfg.deployment.location.lat = 30.07;
        cfg.deployment.location.lon = -81.47;
        cfg.deployment.display_name = "Test".into();
        for id in crate::ha_adopt::ENTITIES {
            cfg.ha_adoption.push(marker(id));
        }
        cfg.seeded_source_ids.push("nws".into());
        crate::ports::config_store::ConfigStore::save(&*store, &cfg)
            .await
            .unwrap();
        (store, cfg)
    }

    #[tokio::test]
    async fn a_settings_save_that_omits_the_adoption_ledger_does_not_erase_it() {
        let (store, cfg) = adopted_store("put-json").await;
        // The hazard shape: a client that round-trips the config through its
        // own model and drops the key it does not know about.
        let mut body = serde_json::to_value(&cfg).unwrap();
        body.as_object_mut().unwrap().remove("ha_adoption");
        body.as_object_mut().unwrap().remove("seeded_source_ids");
        assert!(body.get("ha_adoption").is_none());

        let resp = put_config(
            State(ConfigApiState::store_only(store.clone())),
            Query(ConfigSaveParams::default()),
            Json(body),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);

        let after = crate::ports::config_store::ConfigStore::load(&*store)
            .await
            .unwrap();
        assert_eq!(
            after.ha_adoption.len(),
            crate::ha_adopt::ENTITIES.len(),
            "the ledger is server-owned and survives a body that omits it"
        );
        let policy = crate::ha::WateringPolicy::from_config(&after);
        for id in crate::ha_adopt::ENTITIES {
            assert!(policy.ha_read_retired(id), "{id} went live again");
        }
        assert!(
            after.seeded_source_ids.contains(&"nws".to_string()),
            "the seeding ledger is server-owned too, or the next boot re-adds \
             a source the owner deleted"
        );
    }

    #[tokio::test]
    async fn a_raw_toml_save_with_no_adoption_block_does_not_erase_it() {
        let (store, cfg) = adopted_store("put-raw").await;
        let mut stripped = cfg.clone();
        stripped.ha_adoption.clear();
        stripped.seeded_source_ids.clear();
        let text = toml::to_string_pretty(&stripped).unwrap();
        assert!(!text.contains("ha_adoption"));
        assert!(!text.contains("seeded_source_ids"));

        let resp = put_raw_toml(
            State(ConfigApiState::store_only(store.clone())),
            Query(ConfigSaveParams::default()),
            text,
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);

        let after = crate::ports::config_store::ConfigStore::load(&*store)
            .await
            .unwrap();
        assert_eq!(after.ha_adoption.len(), crate::ha_adopt::ENTITIES.len());
        assert!(after.seeded_source_ids.contains(&"nws".to_string()));
    }

    // A rollback to a pre-adoption snapshot must not un-retire the reads: the
    // notice tells the owner the helpers are inert and invites deleting them,
    // so pointing a read back at one points it at nothing. The values it
    // rolls back are restored; the ledger rides across.
    #[tokio::test]
    async fn a_rollback_to_a_pre_adoption_snapshot_keeps_the_ledger() {
        let store = Arc::new(FileConfigStore::new(
            ledger_dir("rollback").join("localsky.toml"),
        ));
        let mut before = Config::default();
        before.deployment.location.lat = 30.07;
        before.deployment.location.lon = -81.47;
        before.deployment.display_name = "Test".into();
        before.engine.skip_rules.max_wind_mph = 10.0;
        crate::ports::config_store::ConfigStore::save(&*store, &before)
            .await
            .unwrap();

        // The pass runs: thresholds move, markers land, and the save
        // snapshots the pre-adoption file.
        let mut after = before.clone();
        after.engine.skip_rules.max_wind_mph = 12.0;
        for id in crate::ha_adopt::ENTITIES {
            after.ha_adoption.push(marker(id));
        }
        after.seeded_source_ids.push("nws".into());
        crate::ports::config_store::ConfigStore::save(&*store, &after)
            .await
            .unwrap();

        let snaps = crate::ports::config_store::ConfigStore::list_snapshots(&*store)
            .await
            .unwrap();
        let ts = snaps
            .first()
            .expect("a pre-adoption snapshot exists")
            .version;

        let resp = post_rollback(
            State(ConfigApiState::store_only(store.clone())),
            Query(RollbackQuery { to: None }),
            Some(Json(RollbackBody { ts })),
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::OK);

        let restored = crate::ports::config_store::ConfigStore::load(&*store)
            .await
            .unwrap();
        assert_eq!(
            restored.engine.skip_rules.max_wind_mph, 10.0,
            "the rollback still restores the values it was asked for"
        );
        assert_eq!(
            restored.ha_adoption.len(),
            crate::ha_adopt::ENTITIES.len(),
            "but the migration ledger is not a value to roll back"
        );
        assert!(restored.seeded_source_ids.contains(&"nws".to_string()));
    }

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
                endpoint: None,
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
    fn redact_covers_cloud_oauth_client_id() {
        // YoLink (UAID), Tuya (access_id) and Netatmo authenticate with a
        // client_id + client_secret pair. The secret half was already
        // redacted; this proves the client_id (the app/account identifier
        // half) is too, that non-secret device ids survive, and that the
        // id-keyed unredact restores the pair on the PUT round-trip.
        let original = serde_json::json!({
            "sources": [{
                "id": "yolink_main",
                "kind": "yolink",
                "config": {
                    "client_id": "ua_0123456789abcdef",
                    "client_secret": "sec_fedcba9876543210",
                    "device_field_map": {}
                }
            }, {
                "id": "netatmo_main",
                "kind": "netatmo",
                "config": {
                    "client_id": "netatmo-app-5f3c",
                    "client_secret": "netatmo_secret_x",
                    "refresh_token": "netatmo_refresh_y",
                    "device_id": "70:ee:50:00:11:22"
                }
            }]
        });
        let mut v = original.clone();
        redact_secrets(&mut v);
        let s = serde_json::to_string(&v).unwrap();
        assert!(!s.contains("ua_0123456789abcdef"), "YoLink UAID leaked");
        assert!(!s.contains("netatmo-app-5f3c"), "Netatmo client_id leaked");
        assert!(!s.contains("sec_fedcba9876543210"), "client_secret leaked");
        assert!(!s.contains("netatmo_refresh_y"), "refresh_token leaked");
        // Non-credential device identifier stays visible for the form.
        assert!(
            s.contains("70:ee:50:00:11:22"),
            "device_id unexpectedly redacted"
        );

        // Round-trip: re-submitting the redacted config restores the pair
        // by id, so a settings save that did not touch them is lossless.
        let mut candidate = v.clone();
        unredact_secrets(&mut candidate, &original);
        assert_eq!(
            candidate["sources"][0]["config"]["client_id"],
            "ua_0123456789abcdef"
        );
        assert_eq!(
            candidate["sources"][1]["config"]["client_id"],
            "netatmo-app-5f3c"
        );
        assert_eq!(
            candidate["sources"][1]["config"]["client_secret"],
            "netatmo_secret_x"
        );
    }

    #[tokio::test]
    async fn raw_put_rename_rejection_names_the_working_paths() {
        // The raw-TOML editor has no __renames hint (free text; a renamed id
        // is indistinguishable from a new entry), so renaming a source id in
        // the redacted text ALWAYS strands that entry's sentinels and 400s.
        // The rejection must be honest and actionable: name the
        // rename-in-Settings path (which migrates secrets automatically) and
        // the paste-the-real-secret alternative.
        use crate::config::schema::*;
        let dir =
            std::env::temp_dir().join(format!("localsky-raw-rename-reject-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = Arc::new(FileConfigStore::new(dir.join("localsky.toml")));

        // Stored config: one source with a real secret under id "ha_pass".
        let mut cfg = Config::default();
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
        store.save(&cfg).await.unwrap();

        // Simulate the Advanced-editor round-trip after an id rename: the
        // fetched (redacted) TOML with the id changed and the secret still
        // sitting as the sentinel.
        let mut renamed = cfg.clone();
        renamed.sources[0].id = "ha_pass_renamed".into();
        let SourceKind::HaPassthrough(ha) = &mut renamed.sources[0].source else {
            panic!("expected ha_passthrough source");
        };
        ha.bearer_token = SECRET_REDACTED_SENTINEL.to_string();
        let body = toml::to_string_pretty(&renamed).unwrap();

        let resp = put_raw_toml(
            State(ConfigApiState::store_only(store)),
            Query(ConfigSaveParams::default()),
            body,
        )
        .await
        .into_response();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["error"], "unmatched_redacted_secret");
        let detail = v["detail"].as_str().expect("detail is a string");
        // The stranded entry is still named by path...
        assert!(
            detail.contains("ha_pass_renamed"),
            "detail must name the stranded entry: {detail}"
        );
        // ...and both working paths are spelled out.
        assert!(
            detail.contains("Settings > Devices"),
            "detail must point at the Settings rename path: {detail}"
        );
        assert!(
            detail.contains(SECRET_REDACTED_SENTINEL),
            "detail must tell the user to paste the real value over the placeholder: {detail}"
        );

        let _ = std::fs::remove_dir_all(&dir);
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
        // OAuth pair where the client_id is the account/app identifier half
        // of the credential (Netatmo; same shape as YoLink UAID + Tuya
        // access_id). Both halves must be swallowed by the redactor.
        cfg.sources.push(SourceEntry {
            id: "netatmo_main".into(),
            priority: 35,
            enabled: true,
            max_age_s: None,
            source: SourceKind::Netatmo(NetatmoConfig {
                client_id: format!("{M}_netatmo_client_id"),
                client_secret: format!("{M}_netatmo_client_secret"),
                refresh_token: format!("{M}_netatmo_refresh"),
                device_id: "70:ee:50:00:11:22".into(),
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
        assert!(
            s.contains("70:ee:50:00:11:22"),
            "public Netatmo device_id must survive"
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
    fn redact_http_source_url_headers_and_body() {
        use serde_json::json;
        // RestPoll: api_key in the URL query, an Authorization header value, a
        // benign Content-Type header, and a credential-bearing body. Plus a
        // Prometheus source whose URL carries no credential.
        let original = json!({
            "sources": [
                {
                    "id": "restpoll1",
                    "config": {
                        "url": "https://api.example.com/v1/obs?station=x&api_key=SUPERSECRET",
                        "headers": {
                            "Authorization": "Bearer SECRETBEARER",
                            "Content-Type": "application/json"
                        },
                        "body": "client_secret=HUSH"
                    }
                },
                {
                    "id": "prom1",
                    "config": { "url": "http://prometheus.lan:9090/api/v1/query" }
                }
            ]
        });
        let mut v = original.clone();
        redact_secrets(&mut v);
        let s = serde_json::to_string(&v).unwrap();
        assert!(!s.contains("SUPERSECRET"), "url api_key leaked: {s}");
        assert!(!s.contains("SECRETBEARER"), "auth header value leaked: {s}");
        assert!(!s.contains("HUSH"), "body credential leaked: {s}");
        // Benign header and the clean Prometheus URL stay visible.
        assert_eq!(
            v["sources"][0]["config"]["headers"]["Content-Type"],
            "application/json"
        );
        assert_eq!(
            v["sources"][1]["config"]["url"],
            "http://prometheus.lan:9090/api/v1/query"
        );
        // Round-trip: re-submitting the redacted config restores every secret
        // by id, so a settings save that did not touch them is lossless.
        let mut candidate = v.clone();
        unredact_secrets(&mut candidate, &original);
        assert_eq!(
            candidate["sources"][0]["config"]["url"],
            "https://api.example.com/v1/obs?station=x&api_key=SUPERSECRET"
        );
        assert_eq!(
            candidate["sources"][0]["config"]["headers"]["Authorization"],
            "Bearer SECRETBEARER"
        );
        assert_eq!(
            candidate["sources"][0]["config"]["body"],
            "client_secret=HUSH"
        );
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
        // unchanged. client_id is redacted too: it is the account/app
        // identifier HALF of the credential pair (YoLink UAID, Tuya
        // access_id), so leaving it clear half-leaks the credential, the
        // same reasoning as the account-email redaction.
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

        // GET path: redaction hides every new secret (both credential
        // halves) but leaves non-secret fields visible.
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
        // client_id is the username half of the OAuth pair: redacted too.
        assert!(
            !s.contains("63abc_public_client_id"),
            "client_id (credential pair identifier) leaked"
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
            push: None,
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

    // ---- tuning-report Apply (POST /config/zones/apply) components ----

    fn apply_zone_fixture() -> crate::config::schema::ZoneConfig {
        use crate::config::schema::*;
        ZoneConfig {
            scheduling_model: None,
            display_name: "Front".into(),
            area_sqft: 1000.0,
            species: GrassSpecies::Bermuda,
            soil_texture: SoilTexture::SandyLoam,
            slope_pct: 0.0,
            sun_exposure: Default::default(),
            sprinkler_type: SprinklerType::Spray,
            precip_rate_mm_hr: None,
            precip_rate_source: PrecipRateSource::Catalog,
            root_depth_mm: Some(120.0),
            mad_pct_override: None,
            controller_id: "os_main".into(),
            controller_station: "1".into(),
            controller_zone_name: None,
            soil_sensor_id: None,
            target_min_pct_soil: 30.0,
            saturation_pct_soil: 70.0,
            photo_url: None,
            weekly_budget_in: Some(1.0),
            sessions_per_week: Some(2),
            rain_credit_cap_in: None,
            max_run_minutes: None,
        }
    }

    #[test]
    fn apply_zone_field_writes_each_recommended_field() {
        use crate::config::schema::{PrecipRateSource, SoilTexture};
        let mut z = apply_zone_fixture();
        // Measured rate + provenance pair (the check D apply).
        let old = apply_zone_field(&mut z, "precip_rate_mm_hr", &serde_json::json!(18.4)).unwrap();
        assert_eq!(old, serde_json::Value::Null);
        assert_eq!(z.precip_rate_mm_hr, Some(18.4));
        apply_zone_field(&mut z, "precip_rate_source", &serde_json::json!("measured")).unwrap();
        assert_eq!(z.precip_rate_source, PrecipRateSource::Measured);
        // Texture step.
        let old = apply_zone_field(&mut z, "soil_texture", &serde_json::json!("loam")).unwrap();
        assert_eq!(old, serde_json::json!("sandy_loam"));
        assert_eq!(z.soil_texture, SoilTexture::Loam);
        // Null clears an override (the restore-species-default apply).
        let old = apply_zone_field(&mut z, "root_depth_mm", &serde_json::Value::Null).unwrap();
        assert_eq!(old, serde_json::json!(120.0));
        assert_eq!(z.root_depth_mm, None);
        // Sessions + budget.
        apply_zone_field(&mut z, "sessions_per_week", &serde_json::json!(3)).unwrap();
        assert_eq!(z.sessions_per_week, Some(3));
        // 1..=7 is the range the allocator's spacing gate can resolve: it
        // paces at floor(7/sessions) days, so 8 gives a 0 day interval and
        // stops holding a zone that already watered today. Both ends of the
        // range are valid; outside it the write is refused and the stored
        // value is untouched.
        apply_zone_field(&mut z, "sessions_per_week", &serde_json::json!(1)).unwrap();
        apply_zone_field(&mut z, "sessions_per_week", &serde_json::json!(7)).unwrap();
        assert_eq!(z.sessions_per_week, Some(7));
        assert!(apply_zone_field(&mut z, "sessions_per_week", &serde_json::json!(8)).is_err());
        assert!(apply_zone_field(&mut z, "sessions_per_week", &serde_json::json!(0)).is_err());
        assert_eq!(
            z.sessions_per_week,
            Some(7),
            "a refused write changes nothing"
        );
        // Null still clears the override.
        apply_zone_field(&mut z, "sessions_per_week", &serde_json::Value::Null).unwrap();
        assert_eq!(z.sessions_per_week, None);
        apply_zone_field(&mut z, "sessions_per_week", &serde_json::json!(3)).unwrap();
        apply_zone_field(&mut z, "weekly_budget_in", &serde_json::json!(1.9)).unwrap();
        assert_eq!(z.weekly_budget_in, Some(1.9));
        // Run limit (the check A cap-primary apply); null restores the default.
        let old = apply_zone_field(&mut z, "max_run_minutes", &serde_json::json!(90)).unwrap();
        assert_eq!(old, serde_json::Value::Null);
        assert_eq!(z.max_run_minutes, Some(90));
        let old = apply_zone_field(&mut z, "max_run_minutes", &serde_json::Value::Null).unwrap();
        assert_eq!(old, serde_json::json!(90));
        assert_eq!(z.max_run_minutes, None);
        // The per-zone scheduling-model pin (the flip banner's per-zone
        // "Keep weekly" writeback path); null clears back to the engine
        // default.
        use crate::config::schema::SchedulingModel;
        let old = apply_zone_field(&mut z, "scheduling_model", &serde_json::json!("soil")).unwrap();
        assert_eq!(old, serde_json::Value::Null);
        assert_eq!(z.scheduling_model, Some(SchedulingModel::Soil));
        apply_zone_field(&mut z, "scheduling_model", &serde_json::json!("weekly")).unwrap();
        assert_eq!(z.scheduling_model, Some(SchedulingModel::Weekly));
        let old = apply_zone_field(&mut z, "scheduling_model", &serde_json::Value::Null).unwrap();
        assert_eq!(old, serde_json::json!("weekly"));
        assert_eq!(z.scheduling_model, None);
    }

    #[test]
    fn apply_zone_field_refuses_unknown_fields_and_bad_values() {
        let mut z = apply_zone_fixture();
        assert!(apply_zone_field(&mut z, "controller_station", &serde_json::json!("9")).is_err());
        assert!(apply_zone_field(&mut z, "soil_texture", &serde_json::json!("granite")).is_err());
        assert!(apply_zone_field(&mut z, "sessions_per_week", &serde_json::json!(1.5)).is_err());
        // Run limit: the arm enforces the same 5..=360 band the validator does.
        assert!(apply_zone_field(&mut z, "max_run_minutes", &serde_json::json!(4)).is_err());
        assert!(apply_zone_field(&mut z, "max_run_minutes", &serde_json::json!(400)).is_err());
        assert!(apply_zone_field(&mut z, "max_run_minutes", &serde_json::json!(90.5)).is_err());
        // An unknown model variant is refused by the enum parse, and the
        // stored pin is untouched.
        assert!(apply_zone_field(&mut z, "scheduling_model", &serde_json::json!("lunar")).is_err());
        assert_eq!(z.scheduling_model, None);
    }

    // ---- raw-TOML zone rename guard ----

    fn keys(list: &[&str]) -> std::collections::BTreeSet<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_raw_zone_key_rename_is_refused_and_says_why() {
        let detail = zone_key_rename_detail(
            &keys(&["front_yard", "back_yard"]),
            &keys(&["front_lawn", "back_yard"]),
        )
        .expect("a dropped key plus a new key reads as a rename");
        assert!(
            detail.contains("front_yard"),
            "names the key that would be lost"
        );
        assert!(
            detail.contains("front_lawn"),
            "names the key that would appear"
        );
        assert!(
            detail.contains("permanent"),
            "states the rule instead of just refusing: {detail}"
        );
        assert!(
            detail.contains("display_name"),
            "points at the field that IS the zone's name: {detail}"
        );
        assert!(
            detail.contains("allow_zone_key_change=1"),
            "names the override so a genuine delete-plus-add is not a dead end: {detail}"
        );
    }

    /// THE ESCAPE THE REFUSAL ITSELF TELLS THE USER TO TYPE. Axum's `Query`
    /// deserializes through serde_urlencoded, whose bool impl is
    /// `str::parse::<bool>()`, and Rust's `FromStr for bool` accepts only
    /// "true"/"false". A bare `bool` field therefore does not read `=1` as
    /// false: it fails the WHOLE extraction and answers 400 before the
    /// handler runs, so the documented escape was unreachable. Drive the
    /// real query layer, not a constructed value, or this regresses silently.
    #[test]
    fn the_documented_allow_zone_key_change_1_actually_parses() {
        use axum::extract::Query;
        for q in [
            "?allow_zone_key_change=1",
            "?allow_zone_key_change=true",
            "?allow_zone_key_change=TRUE",
            "?allow_zone_key_change=yes",
            "?allow_zone_key_change=on",
            // A bare flag with no value is what a person types by hand.
            "?allow_zone_key_change",
        ] {
            let uri: axum::http::Uri = format!("/api/v1/config/raw{q}").parse().unwrap();
            let Query(p) = Query::<ConfigSaveParams>::try_from_uri(&uri)
                .unwrap_or_else(|e| panic!("{q} must parse, got {e:?}"));
            assert!(p.allows_zone_key_change(), "{q} must read as set");
        }
        // Absent, and an explicit off, both leave the guard armed.
        for q in [
            "",
            "?allow_zone_key_change=0",
            "?allow_zone_key_change=false",
        ] {
            let uri: axum::http::Uri = format!("/api/v1/config/raw{q}").parse().unwrap();
            let Query(p) = Query::<ConfigSaveParams>::try_from_uri(&uri).unwrap();
            assert!(!p.allows_zone_key_change(), "{q} must leave the guard on");
        }
    }

    /// The same defect sat in the pre-existing `?reveal=1`, which is what
    /// made the bare-bool pattern look safe to copy.
    #[test]
    fn the_documented_reveal_1_actually_parses() {
        use axum::extract::Query;
        let uri: axum::http::Uri = "/api/v1/config/raw?reveal=1".parse().unwrap();
        let Query(q) = Query::<RawQuery>::try_from_uri(&uri).expect("?reveal=1 must parse");
        assert!(query_flag(q.reveal.as_ref()));
        let uri: axum::http::Uri = "/api/v1/config/raw".parse().unwrap();
        let Query(q) = Query::<RawQuery>::try_from_uri(&uri).unwrap();
        assert!(!query_flag(q.reveal.as_ref()), "redacted stays the default");
    }

    #[test]
    fn query_flag_reads_the_spellings_people_type() {
        assert!(query_flag(Some(&"1".to_string())));
        assert!(query_flag(Some(&" TrUe ".to_string())));
        assert!(query_flag(Some(&String::new())), "a bare ?flag is set");
        assert!(!query_flag(None));
        assert!(!query_flag(Some(&"0".to_string())));
        assert!(!query_flag(Some(&"maybe".to_string())));
    }

    #[test]
    fn adding_or_deleting_a_zone_in_raw_toml_stays_allowed() {
        // Pure addition: a new zone alongside the existing ones.
        assert_eq!(
            zone_key_rename_detail(&keys(&["front_yard"]), &keys(&["front_yard", "orchard"])),
            None
        );
        // Pure deletion: the delete button's own path writes this shape.
        assert_eq!(
            zone_key_rename_detail(&keys(&["front_yard", "orchard"]), &keys(&["front_yard"])),
            None
        );
        // No change at all, including the first-ever save with no zones.
        assert_eq!(
            zone_key_rename_detail(&keys(&["front_yard"]), &keys(&["front_yard"])),
            None
        );
        assert_eq!(zone_key_rename_detail(&keys(&[]), &keys(&[])), None);
        assert_eq!(
            zone_key_rename_detail(&keys(&[]), &keys(&["front_yard"])),
            None
        );
    }

    #[test]
    fn a_hyphen_to_underscore_zone_key_edit_is_still_a_rename() {
        // Dispatch normalizes hyphens to underscores, so "back-yard" and
        // "back_yard" water the same valve. The STORED key does not
        // normalize: history, entity ids and MQTT topics all carry the
        // literal config key, so swapping one for the other orphans them.
        assert!(
            zone_key_rename_detail(&keys(&["back-yard"]), &keys(&["back_yard"])).is_some(),
            "a hyphen swap changes the stored key and must be refused too"
        );
    }

    #[test]
    fn applied_out_of_range_rate_fails_structural_validation() {
        // The 422 path: a rate outside 0..200 must be rejected by
        // validate::validate before any save.
        let mut cfg = Config::default();
        cfg.zones.insert("front".into(), apply_zone_fixture());
        let z = cfg.zones.get_mut("front").unwrap();
        apply_zone_field(z, "precip_rate_mm_hr", &serde_json::json!(500.0)).unwrap();
        let report = crate::config::validate::validate(&cfg);
        assert!(
            !report.ok(),
            "precip_rate 500 must fail the zone_precip_rate_range gate"
        );
    }

    #[test]
    fn verify_recommendation_accepts_current_and_refuses_stale() {
        use crate::history::types::{TuningRecommendation, TuningReport, ZoneTuning};
        let rec = TuningRecommendation {
            id: "abc123".into(),
            field: "soil_texture".into(),
            current_value: serde_json::json!("sandy_loam"),
            suggested_value: serde_json::json!("loam"),
            companion_fields: vec![],
            headline: "try loam".into(),
            evidence: vec![],
            confidence: "medium".into(),
        };
        let report = TuningReport {
            generated_epoch: 0,
            window_days: 14,
            zones: vec![ZoneTuning {
                slug: "front_yard".into(),
                display_name: "Front".into(),
                status: "recommendation".into(),
                lines: vec![],
                recommendation: Some(rec.clone()),
                ..Default::default()
            }],
            scorecard: Default::default(),
        };
        let ok_body = ZoneApplyBody {
            zone_slug: "front-yard".into(), // dashed form must still match
            recommendation_id: "abc123".into(),
            field: "soil_texture".into(),
            value: serde_json::json!("loam"),
            window_days: None,
        };
        assert_eq!(verify_recommendation(&report, &ok_body).unwrap().id, rec.id);
        // Stale id -> refused with a plain detail (the 409 body).
        let stale = ZoneApplyBody {
            recommendation_id: "outdated".into(),
            ..ok_body
        };
        let err = verify_recommendation(&report, &stale).unwrap_err();
        assert!(err.contains("refresh the tuning report"), "{err}");
        // A zone with no recommendation any more -> refused.
        let no_rec = ZoneApplyBody {
            zone_slug: "missing".into(),
            recommendation_id: "abc123".into(),
            field: "soil_texture".into(),
            value: serde_json::json!("loam"),
            window_days: None,
        };
        assert!(verify_recommendation(&report, &no_rec).is_err());
    }

    /// The apply body accepts the report's window (absent = default), so
    /// clients viewing a non-default window can echo it.
    #[test]
    fn zone_apply_body_window_days_is_optional() {
        let with: ZoneApplyBody = serde_json::from_value(serde_json::json!({
            "zone_slug": "front_yard",
            "recommendation_id": "abc123",
            "field": "soil_texture",
            "value": "loam",
            "window_days": 30,
        }))
        .unwrap();
        assert_eq!(with.window_days, Some(30));
        let without: ZoneApplyBody = serde_json::from_value(serde_json::json!({
            "zone_slug": "front_yard",
            "recommendation_id": "abc123",
            "field": "soil_texture",
            "value": "loam",
        }))
        .unwrap();
        assert_eq!(without.window_days, None);
    }
}
