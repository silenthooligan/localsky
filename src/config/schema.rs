// Config schema. The single source of truth for /data/localsky.toml.
//
// Every field is serde-typed and JsonSchema-derivable so the settings UI
// can fetch /api/config/schema and render form widgets without hand-rolled
// TS interfaces. Polymorphic adapter blocks (sources, controllers, llm)
// use #[serde(tag = "kind", content = "config")] for clean TOML shape:
//
//   [[sources]]
//   id = "tempest_lan"
//   priority = 100
//   kind = "tempest_udp"
//   [sources.config]
//   bind_addr = "0.0.0.0:50222"

use std::collections::BTreeMap;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub const CURRENT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Config {
    /// Bumped on every breaking schema change. Migrations live in
    /// src/config/migrate.rs and run once at boot if `schema_version`
    /// is below CURRENT_SCHEMA_VERSION.
    pub schema_version: u32,
    pub deployment: Deployment,
    #[serde(default)]
    pub features: Features,
    #[serde(default)]
    pub sources: Vec<SourceEntry>,
    #[serde(default)]
    pub controllers: Vec<ControllerEntry>,
    #[serde(default)]
    pub zones: BTreeMap<String, ZoneConfig>,
    #[serde(default)]
    pub llm: Option<LlmConfig>,
    #[serde(default)]
    pub notifications: Notifications,
    #[serde(default)]
    pub engine: EngineParams,
    /// Operator-defined cron-style schedules that fire zone runs at
    /// fixed weekdays + times, optionally overriding the smart engine
    /// for the matching zone. See `ManualSchedule` for semantics.
    #[serde(default)]
    pub manual_schedules: Vec<ManualSchedule>,
    /// User-defined Rhai skip rules (augment-only). See `ScriptingConfig`.
    #[serde(default)]
    pub scripting: ScriptingConfig,
    /// Structured, user-configurable per-zone trigger rules (augment-only).
    /// A no-code complement to `scripting`. See `ConditionsConfig`.
    #[serde(default)]
    pub conditions: ConditionsConfig,
    /// Built-in authentication policy. Identity (accounts, sessions,
    /// API tokens) lives in SQLite; this block is policy only. Absent
    /// on existing configs -> Disabled -> behavior unchanged.
    #[serde(default)]
    pub auth: AuthConfig,
    /// LAN presence (mDNS announce). Default on; announce-only.
    #[serde(default)]
    pub network: NetworkConfig,
    /// Opt-in update check (GitHub releases poll, off by default).
    #[serde(default)]
    pub updates: UpdatesConfig,
    /// Local history retention knobs (SQLite sensor_history pruning).
    #[serde(default)]
    pub persistence: PersistenceConfig,
    /// UI presentation defaults (radar layer set today). Server-side
    /// defaults only; per-browser overrides persist in localStorage.
    #[serde(default)]
    pub ui: UiConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            deployment: Deployment::default(),
            features: Features::default(),
            sources: Vec::new(),
            controllers: Vec::new(),
            zones: BTreeMap::new(),
            llm: None,
            notifications: Notifications::default(),
            engine: EngineParams::default(),
            manual_schedules: Vec::new(),
            scripting: ScriptingConfig::default(),
            conditions: ConditionsConfig::default(),
            auth: AuthConfig::default(),
            network: NetworkConfig::default(),
            updates: UpdatesConfig::default(),
            persistence: PersistenceConfig::default(),
            ui: UiConfig::default(),
        }
    }
}

// ----- UI preferences -----

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct UiConfig {
    #[serde(default)]
    pub radar: RadarUiConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RadarUiConfig {
    /// Radar tile providers offered in the layer menu, by catalog id
    /// (see radar_catalog::providers()). Empty (the default) means
    /// Auto: the region-smart recommended set for the configured
    /// station location. Non-empty means exactly this menu, in this
    /// order; ANY catalog provider is allowed anywhere, so a user in
    /// Europe can deliberately switch on a US source to compare.
    #[serde(default)]
    pub providers: Vec<String>,
    /// Radar overlays enabled by default for a browser with no stored
    /// preference. Accepts provider ids AND feature ids from
    /// radar_catalog (legacy precip/nexrad/lightning ids are still
    /// accepted and normalized to their catalog successors). Once a
    /// user toggles layers, their choice persists per-browser in
    /// localStorage and wins over this list.
    #[serde(default = "default_radar_layers")]
    pub default_layers: Vec<String>,
}

impl Default for RadarUiConfig {
    fn default() -> Self {
        Self {
            providers: Vec::new(),
            default_layers: default_radar_layers(),
        }
    }
}

fn default_radar_layers() -> Vec<String> {
    // Catalog successors of the old hardcoded precip + NEXRAD +
    // strikes trio; sourced from the catalog so the radar panel's
    // non-ssr fallback can never drift from the config default.
    crate::radar_catalog::default_layer_ids()
        .iter()
        .map(|s| s.to_string())
        .collect()
}

// ----- Persistence retention -----

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PersistenceConfig {
    /// Days of sensor_history readings to keep. Rows older than this are
    /// pruned opportunistically as new readings are ingested. 0 disables
    /// pruning (keep everything forever).
    #[serde(default = "default_retention_days")]
    pub retention_days: u32,
    /// Days of run/skip/decision history to keep. 0 (the default) keeps
    /// everything forever, which is what makes year-over-year trends in
    /// History possible. Set a cap only if disk is genuinely tight.
    #[serde(default)]
    pub runs_retention_days: u32,
}

impl Default for PersistenceConfig {
    fn default() -> Self {
        Self {
            retention_days: default_retention_days(),
            runs_retention_days: 0,
        }
    }
}

pub fn default_retention_days() -> u32 {
    90
}

// ----- Update check -----

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct UpdatesConfig {
    /// When true, poll the GitHub releases API daily and surface
    /// "update available" in the UI. Plain GET, no telemetry attached;
    /// off by default so fresh installs phone nowhere.
    #[serde(default)]
    pub check_enabled: bool,
}

// ----- LAN presence -----

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NetworkConfig {
    /// Announce _localsky._tcp via mDNS so clients (HACS zeroconf)
    /// discover this instance. Announce-only; requires host networking
    /// under Docker to be visible beyond the container.
    #[serde(default = "default_true_network")]
    pub mdns_enabled: bool,
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self { mdns_enabled: true }
    }
}

fn default_true_network() -> bool {
    true
}

// ----- Authentication policy -----

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum AuthMode {
    /// No authentication. The pre-auth behavior; right for deployments
    /// already gated by a reverse proxy or an isolated trusted LAN.
    #[default]
    Disabled,
    /// Login required for the UI + API. Static assets, /api/v1/info,
    /// ingest receivers, and liveness stay public; see auth::middleware.
    Required,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AuthConfig {
    #[serde(default)]
    pub mode: AuthMode,
    /// Browser session lifetime. Rolling: activity extends it.
    #[serde(default = "default_session_ttl_days")]
    pub session_ttl_days: u32,
    /// CIDRs that skip auth while mode = required, e.g. "10.0.0.0/24".
    /// Lets an operator require login from the WAN/VPN side while the
    /// home LAN stays frictionless.
    #[serde(default)]
    pub trusted_networks: Vec<String>,
    /// CIDRs of reverse proxies whose X-Forwarded-For is believed,
    /// e.g. "172.18.0.0/16". When the socket peer is NOT in this list
    /// the peer address itself is the client IP and X-Forwarded-For is
    /// ignored (it is trivially spoofable). Empty = never trust XFF.
    #[serde(default)]
    pub trusted_proxies: Vec<String>,
    /// Extra Origins allowed to make state-changing API calls, e.g.
    /// "https://dash.example.com". Same-origin requests (Origin host
    /// matching the request Host) always pass; this list is for
    /// deliberate cross-origin frontends.
    #[serde(default)]
    pub trusted_origins: Vec<String>,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            mode: AuthMode::Disabled,
            session_ttl_days: default_session_ttl_days(),
            trusted_networks: Vec::new(),
            trusted_proxies: Vec::new(),
            trusted_origins: Vec::new(),
        }
    }
}

fn default_session_ttl_days() -> u32 {
    30
}

// ----- Structured trigger rules (no-code) -----
//
// A complement to Rhai `scripting`: the same augment-only safety boundary,
// expressed as serde data the UI builds with dropdowns. The rule model +
// evaluator live in `crate::engine::conditions`; this is just the config
// container so every existing localsky.toml (which has no `[conditions]`
// block) still parses via `#[serde(default)]`.

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ConditionsConfig {
    #[serde(default)]
    pub rules: Vec<crate::engine::conditions::ConditionRule>,
}

// ----- User scripting (Rhai) -----
//
// Augment-only custom skip rules. The engine consults these ONLY when the
// built-in deterministic ladder already returned "run", so a script can
// add a skip but can never clear a freeze / wind / restriction gate. A
// rule returns `true` (skip with its name as the reason) or a non-empty
// string (skip with that reason); anything else (false, errors, invalid
// syntax) is a no-op (fail-safe). Scripts are sandboxed: no I/O, no
// imports, bounded operation count.

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ScriptingConfig {
    #[serde(default)]
    pub skip_rules: Vec<ScriptRule>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ScriptRule {
    /// snake_case id; shows in the Rule Lab trace.
    pub id: String,
    /// Display label (defaults to `id` if blank).
    #[serde(default)]
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Rhai script. Reads the current decision inputs as variables:
    /// temp_now_f, wind_now_mph, rain_today_in, rain_intensity_now_in_hr,
    /// humidity_now_pct, forecast_in, rain_tomorrow_prob_pct,
    /// rain_next_4h_in, wind_max_today_mph, temp_min_24h_f, temp_max_3day_f,
    /// days_since_significant_rain. Return `true` to skip, or a non-empty
    /// string for a custom skip reason.
    pub script: String,
}

// ----- Manual schedules -----
//
// Smart-irrigation auto-mode is the default, but operators can also
// define explicit per-zone schedules that fire at fixed weekday-and-
// time slots. Schedules respect Phase C watering restrictions exactly
// like smart-irrigation runs do: if a restriction would block the
// dispatch (wrong weekday, forbidden hour), the run is skipped with a
// reason logged to the runs table.
//
// `ManualMode::Override` is the default: smart-irrigation dispatch is
// suppressed for any zone with an enabled override schedule today.
// Smart math still computes for nerd visibility. `Floor` runs both
// useful for "minimum coverage" patterns where smart adds extra when
// the deficit grows large.

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ManualSchedule {
    /// Operator-set unique identifier (snake_case). Used by the
    /// scheduler dedupe key + the runs table source column.
    pub id: String,
    /// Display label. Defaults to `id` if blank.
    #[serde(default)]
    pub name: String,
    /// Zone slug this schedule fires. References a key in `Config.zones`.
    pub zone_slug: String,
    /// Disable to retain the entry but skip evaluation.
    #[serde(default = "default_true_schedule")]
    pub enabled: bool,
    /// Weekdays the schedule fires (`chrono::Weekday::num_days_from_sunday`:
    /// 0=Sun..6=Sat). Empty = never (effectively disabled).
    #[serde(default)]
    pub weekdays: Vec<u8>,
    /// Local-time hour (0..23) the schedule fires.
    pub start_hour: u8,
    /// Local-time minute (0..59) the schedule fires.
    pub start_minute: u8,
    /// Per-dispatch duration in whole minutes. Tightened further if a
    /// Phase C restriction caps run length for the zone.
    pub duration_minutes: u32,
    /// Override vs Floor; see module-level note above.
    #[serde(default)]
    pub mode: ManualMode,
}

fn default_true_schedule() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum ManualMode {
    /// Default: smart-irrigation dispatch for this zone is suppressed
    /// whenever an enabled Override schedule applies today. The smart
    /// engine's daily verdict + math still compute for visibility.
    #[default]
    Override,
    /// Schedule fires AND smart-irrigation may add additional runs if
    /// its deficit math justifies it. Useful for minimum-coverage
    /// patterns; risks overwatering if the smart deficit is already
    /// satisfied by the scheduled run.
    Floor,
}

// ----- Deployment -----

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct Deployment {
    pub location: Location,
    #[serde(default)]
    pub units: Units,
    /// IANA TZ name. None = derive from lat/lon at boot.
    pub timezone: Option<String>,
    #[serde(default = "default_display_name")]
    pub display_name: String,
    /// House-address parity for jurisdictions that gate watering days
    /// by odd vs even address (e.g. St. Johns River WMD in Florida, or
    /// the odd/even day schemes used by Australian and Spanish water
    /// utilities). Defaults to `NotApplicable`, which makes
    /// weekday-restriction gates a no-op even when configured.
    #[serde(default)]
    pub address_parity: AddressParity,
    /// Where the irrigation snapshot is sourced from. `Auto` (default)
    /// uses Home Assistant when HA env is configured, else builds the
    /// snapshot natively (standalone). `HomeAssistant`/`Standalone` force
    /// a path. See `resolve_snapshot_source`.
    #[serde(default)]
    pub mode: DeploymentMode,
    /// When true (and HA is authoritative), also run the native snapshot
    /// builder in shadow and expose its output + a diff for comparison,
    /// without it ever driving dispatch. Env: `LOCALSKY_SHADOW_NATIVE=1`.
    #[serde(default)]
    pub shadow_native: bool,
    /// HA-mode only: the entity-id prefix of the OpenSprinkler (or other)
    /// controller integration in Home Assistant. The snapshot builder reads
    /// `switch.<prefix>_enabled`, `sensor.<prefix>_water_level`, and
    /// `binary_sensor.<prefix>_<zone>_station_running` from it. Set this to
    /// match how the controller's device is named in your HA. Standalone
    /// mode ignores it (controllers are read directly).
    #[serde(default = "default_ha_sprinkler_prefix")]
    pub ha_sprinkler_prefix: String,
}

fn default_ha_sprinkler_prefix() -> String {
    "opensprinkler".to_string()
}

/// Snapshot-source mode. Standalone needs no Home Assistant; HA is one
/// optional source among many.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DeploymentMode {
    /// Native when no HA env is configured, otherwise Home Assistant.
    #[default]
    Auto,
    /// Always build the snapshot from Home Assistant (legacy path).
    HomeAssistant,
    /// Always build the snapshot natively (no HA required).
    Standalone,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AddressParity {
    #[default]
    NotApplicable,
    Odd,
    Even,
}

fn default_display_name() -> String {
    "LocalSky".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct Location {
    pub lat: f64,
    pub lon: f64,
    #[serde(default)]
    pub elevation_m: Option<f64>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Units {
    #[default]
    Imperial,
    Metric,
}

// ----- Feature toggles -----

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Features {
    #[serde(default)]
    pub demo_mode: bool,
    #[serde(default = "default_true")]
    pub enable_mqtt_publish: bool,
    #[serde(default = "default_true")]
    pub enable_advisor: bool,
    #[serde(default = "default_true")]
    pub enable_push: bool,
    #[serde(default)]
    pub nerd_mode_default: bool,
    /// Anonymous telemetry (version, OS family, controller types). Off by
    /// default; opt-in only.
    #[serde(default)]
    pub telemetry: bool,
}

impl Default for Features {
    fn default() -> Self {
        Self {
            demo_mode: false,
            enable_mqtt_publish: true,
            enable_advisor: true,
            enable_push: true,
            nerd_mode_default: false,
            telemetry: false,
        }
    }
}

fn default_true() -> bool {
    true
}

// ----- Weather sources -----

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SourceEntry {
    /// Stable identifier the engine + UI use to reference this source.
    pub id: String,
    /// Higher priority wins per-field merge. Conventional ranges:
    /// 100 = LAN station (truth), 50 = forecast model, 10 = fallback.
    #[serde(default = "default_source_priority")]
    pub priority: i32,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Maximum age (seconds) an observation from this source may have
    /// before it is considered stale: /api/health caps its status at
    /// "stale" and the merge layer excludes it from candidate values.
    /// None (default) keeps the kind-default freshness windows only.
    #[serde(default)]
    pub max_age_s: Option<u64>,
    #[serde(flatten)]
    pub source: SourceKind,
}

fn default_source_priority() -> i32 {
    50
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", content = "config", rename_all = "snake_case")]
pub enum SourceKind {
    TempestUdp(TempestUdpConfig),
    TempestWs(TempestWsConfig),
    OpenMeteo(OpenMeteoConfig),
    EcowittLocal(EcowittLocalConfig),
    /// Ecowitt gateway local-API poller (E1). Reads /get_livedata_info on
    /// the LAN; coexists with HA's push integration.
    EcowittGwPoll(EcowittGwPollConfig),
    DavisWll(DavisWllConfig),
    Nws(NwsConfig),
    // The whole codebase (kind_options, default_config_text, kind_pretty,
    // kind_icon, source_fields, health/sensors kind labels) keys on the string
    // `openweather`; the auto snake_case tag `open_weather` was the odd one out.
    // Serialize as `openweather` so a persisted source round-trips into the same
    // string every UI consumer expects (the labeled Connection form, icons, the
    // segmented picker). Keep `open_weather` as a deserialize alias for any
    // older on-disk config or engine-emitted value that used the snake_case tag.
    #[serde(rename = "openweather", alias = "open_weather")]
    OpenWeather(OpenWeatherConfig),
    PirateWeather(PirateWeatherConfig),
    MetNorway(MetNorwayConfig),
    AmbientWeather(AmbientWeatherConfig),
    Netatmo(NetatmoConfig),
    Yolink(YolinkConfig),
    Lacrosse(LacrosseConfig),
    TuyaCloud(TuyaCloudConfig),
    HaPassthrough(HaPassthroughConfig),
    /// Subscribe to MQTT topics from any publisher (Tasmota, ESPHome,
    /// Zigbee2MQTT, raw MQTT publishers). Works standalone (no HA
    /// required) when paired with a broker like Mosquitto.
    Mqtt(MqttSourceConfig),
    /// Generic HTTP webhook receiver. Any device that can POST JSON
    /// (Arduino, Pi script, custom commercial gateway) feeds LocalSky
    /// without needing MQTT or HA.
    HttpWebhook(HttpWebhookConfig),
    /// Blitzortung.org community lightning network (display-only map/
    /// dashboard layer). Opt-in, default OFF: see BlitzortungConfig.
    Blitzortung(BlitzortungConfig),
    DemoReplay(DemoReplayConfig),
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TempestUdpConfig {
    #[serde(default = "default_tempest_bind")]
    pub bind_addr: String,
    /// Optional: filter to a specific Tempest hub serial (`HB-00012345`).
    /// Null = accept any hub broadcasting on the LAN.
    #[serde(default)]
    pub hub_serial: Option<String>,
}

fn default_tempest_bind() -> String {
    "0.0.0.0:50222".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TempestWsConfig {
    pub access_token: String,
    pub station_id: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct OpenMeteoConfig {
    #[serde(default = "default_open_meteo_forecast_days")]
    pub forecast_days: u32,
    #[serde(default = "default_open_meteo_forecast_hours")]
    pub forecast_hours: u32,
    #[serde(default = "default_open_meteo_past_days")]
    pub past_days: u32,
    #[serde(default)]
    pub include_radar: bool,
    /// Open-Meteo weather model id (`&models=` parameter), from the
    /// forecast::model_catalog. The default "best_match" keeps the
    /// fetch URL byte-identical to the pre-model-selection one, so
    /// configs written before this field existed behave unchanged.
    #[serde(default = "default_open_meteo_model")]
    pub model: String,
}

fn default_open_meteo_forecast_days() -> u32 {
    7
}
fn default_open_meteo_forecast_hours() -> u32 {
    48
}
fn default_open_meteo_past_days() -> u32 {
    1
}
fn default_open_meteo_model() -> String {
    crate::forecast::model_catalog::DEFAULT_MODEL.to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EcowittLocalConfig {
    /// HTTP endpoint LocalSky exposes for the Ecowitt gateway to POST to.
    /// Default = /ingest/ecowitt under the main listener.
    #[serde(default = "default_ecowitt_path")]
    pub path: String,
    /// Optional shared secret in the GW URL config (`?key=...`).
    #[serde(default)]
    pub shared_secret: Option<String>,
}

fn default_ecowitt_path() -> String {
    "/ingest/ecowitt".to_string()
}

/// Native Ecowitt gateway POLLER (E1). Unlike `EcowittLocal` (which receives
/// the gateway's push), this polls the gateway's local HTTP API
/// `GET http://<host>/get_livedata_info` on the LAN and records every
/// reading (soil channels, temp/humidity, rain, wind) into sensor_history,
/// keyed the same way the push path keys them (soilmoisture1.., tempf, ...).
/// It coexists with Home Assistant's own Ecowitt integration because polling
/// the local API doesn't contend for the gateway's single push destination.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EcowittGwPollConfig {
    /// Gateway IP or hostname on the LAN (e.g. "192.0.2.12"). Found via
    /// native discovery (Phase E2) or entered manually.
    pub host: String,
    /// Poll cadence in seconds. The gateway samples roughly every 16s; 30s
    /// is plenty for irrigation and easy on the device.
    #[serde(default = "default_ecowitt_poll_s")]
    pub poll_interval_s: u32,
    /// Per-channel raw-AD soil calibration, keyed by channel ("1".."N"). When
    /// present, the poller reads /get_cli_soilad and computes moisture from the
    /// raw AD via (ad - ad_dry) / (ad_wet - ad_dry) * 100, clamped 0..100
    /// matching whatever dry/wet endpoints you captured rather than the
    /// gateway's own (often unset) % value. Lets LocalSky own calibrated soil
    /// natively instead of leaning on a Home Assistant template sensor.
    #[serde(default)]
    pub soil_calibration: std::collections::HashMap<String, SoilAdCalibration>,
}

/// Dry (probe in air) and wet (probe in water/saturated) raw-AD endpoints for
/// one soil channel. moisture% = (ad - ad_dry) / (ad_wet - ad_dry) * 100.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SoilAdCalibration {
    pub ad_dry: f64,
    pub ad_wet: f64,
}

fn default_ecowitt_poll_s() -> u32 {
    30
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NwsConfig {
    /// Required by api.weather.gov. Plain ASCII, identifies the operator
    /// for support contact. Example: "LocalSky (you@example.com)".
    pub user_agent: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct OpenWeatherConfig {
    pub api_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PirateWeatherConfig {
    pub api_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MetNorwayConfig {
    /// User-Agent required by api.met.no terms of service.
    pub user_agent: String,
}

/// Davis WeatherLink Live (WLL) LAN gateway. Polls the WLL's local
/// HTTP endpoint `http://{host}/v1/current_conditions` (no auth) on
/// the configured interval. Compatible with Vantage Pro 2 + Vantage
/// Vue + EnviroMonitor connected to a WLL hub.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DavisWllConfig {
    /// IP or hostname of the WLL device on the LAN.
    pub host: String,
    /// Transmitter ID (txid) of the ISS to read. Defaults to 1
    /// (the most common single-ISS install). Multi-station households
    /// should set this to the appropriate transmitter id.
    #[serde(default = "default_davis_wll_txid")]
    pub txid: u32,
}

fn default_davis_wll_txid() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AmbientWeatherConfig {
    pub app_key: String,
    pub api_key: String,
    pub mac_address: String,
}

/// YoLink (YoSmart) cloud sensor ecosystem at api.yosmart.com.
/// YoLink ships LoRa-based sensors that report via a B-LAN/M-LAN hub
/// up to YoSmart cloud. Auth is OAuth2 client_credentials (`UAID` +
/// `Secret Key` from the YoLink app developer portal).
///
/// Each entry in `device_field_map` maps a (LocalSky WeatherField name)
/// -> (yolink device_id + device_type + json path into its state). The
/// adapter polls each mapped device's state at the configured cadence
/// and emits an Observation per mapped field.
///
/// Most-relevant YoLink device types for LocalSky:
///   - THSensor (YS8003-UC outdoor temp/RH)
///   - LeakSensor (YS7903-UC), binary; map to a custom field via HA bridge if needed
///   - WaterMeterController, flow + total volume reads via state.waterFlow / state.waterReading
///   - GarageDoor / Hub / Switch, not weather/irrigation, skip
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct YolinkConfig {
    /// UAID from the YoLink app developer settings.
    pub client_id: String,
    /// Secret key paired with the UAID. Treat like a password.
    pub client_secret: String,
    /// Map LocalSky WeatherField name -> YoLink device + state path.
    #[serde(default)]
    pub device_field_map: Vec<YolinkFieldMap>,
    /// Optional base URL override; default api.yosmart.com.
    #[serde(default = "default_yolink_base_url")]
    pub base_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct YolinkFieldMap {
    /// WeatherField variant name (CamelCase or snake_case accepted).
    pub field: String,
    /// YoLink device id (deviceId from /api Home.getDeviceList).
    pub device_id: String,
    /// YoLink device type, used to compose the {Type}.getState method.
    /// Examples: "THSensor", "WaterMeterController", "LeakSensor".
    pub device_type: String,
    /// Dot-separated JSON path into the device's state object. The path
    /// is rooted at `data.state` of the API response. e.g. for THSensor
    /// the live temperature lives at `temperature`. For WaterMeter the
    /// rate lives at `waterFlow`, totalizer at `waterReading`.
    pub state_path: String,
    /// Linear scaling: out = raw * scale + offset. Defaults to identity.
    #[serde(default = "default_one")]
    pub scale: f64,
    #[serde(default)]
    pub offset: f64,
}

fn default_yolink_base_url() -> String {
    "https://api.yosmart.com".to_string()
}

fn default_one() -> f64 {
    1.0
}

/// Tuya cloud (openapi.tuya{us|eu|cn|in}.com), the OEM ecosystem behind
/// RainPoint, Smart Life-branded irrigation timers, dozens of consumer
/// soil moisture / leak / temperature sensors, and most cheap WiFi
/// flow meters. Auth is HMAC-SHA256 signed requests with an access_id
/// (client_id) + access_secret obtained from the Tuya IoT Platform.
///
/// User must:
///   1. Create a Cloud project at iot.tuya.com (free tier OK)
///   2. Link their Tuya/Smart Life app account ("Link Tuya App Account")
///   3. Copy access_id + access_secret into the wizard
///   4. Grab device_ids from the project's "Devices" tab
///
/// Same per-device-mapping shape as YoLink: list of WeatherField ->
/// (device_id + status code) pairs with linear scale + offset.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TuyaCloudConfig {
    pub client_id: String,
    pub client_secret: String,
    /// Regional base URL. Defaults to US (openapi.tuyaus.com); EU users
    /// switch to openapi.tuyaeu.com, China to openapi.tuyacn.com, India
    /// to openapi.tuyain.com.
    #[serde(default = "default_tuya_base_url")]
    pub base_url: String,
    /// Map LocalSky WeatherField -> Tuya (device_id, status_code) pair.
    #[serde(default)]
    pub device_field_map: Vec<TuyaFieldMap>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TuyaFieldMap {
    pub field: String,
    pub device_id: String,
    /// Tuya status code (DP code). Examples: "temp_current", "humi_current",
    /// "water_total", "water_current", "battery_percentage".
    pub status_code: String,
    #[serde(default = "default_one_tuya")]
    pub scale: f64,
    #[serde(default)]
    pub offset: f64,
}

fn default_tuya_base_url() -> String {
    "https://openapi.tuyaus.com".to_string()
}
fn default_one_tuya() -> f64 {
    1.0
}

/// LaCrosse "View" cloud (lacrossealerts.com / lacrosseview.com).
/// LaCrosse stations with the Gateway / View hub upload to the LaCrosse
/// cloud; the mobile + web apps read from a documented REST endpoint
/// (community-mapped by ha-lacrosseview and homeassistant-lacrosseview).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LacrosseConfig {
    pub email: String,
    pub password: String,
    /// Optional device id ("LTV-WSDTH04" etc). When set, only the
    /// matching device's sensors are emitted. When None, the first
    /// active device under the account is used.
    #[serde(default)]
    pub device_id: Option<String>,
}

/// Netatmo Weather Station cloud. Uses the OAuth2 refresh_token grant
/// (one-time browser auth + paste the refresh_token in the wizard;
/// the adapter handles access_token rotation internally). Each MAC
/// addresses a single station; the adapter walks all modules under
/// that station and merges their readings.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NetatmoConfig {
    pub client_id: String,
    pub client_secret: String,
    pub refresh_token: String,
    /// MAC of the main station (e.g. "70:ee:50:00:11:22"). Required;
    /// the GET /api/getstationsdata response is filtered to this MAC.
    pub device_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HaPassthroughConfig {
    pub base_url: String,
    pub bearer_token: String,
    /// Map of LocalSky WeatherField name -> HA entity_id. Lets users
    /// stitch sensors from arbitrary HA integrations into the engine.
    #[serde(default)]
    pub field_map: BTreeMap<String, String>,
}

/// MQTT sensor subscription. Each topic maps to a (WeatherField, optional
/// JSON path) pair. Standalone-friendly: works against any MQTT broker
/// (Mosquitto, EMQX, HiveMQ, the HA broker if you have HA, anything that
/// speaks MQTT 3.1.1 or 5.0). No HA required.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MqttSourceConfig {
    pub broker_host: String,
    #[serde(default = "default_mqtt_broker_port")]
    pub broker_port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    /// MQTT client id. Defaults to "localsky-source-<id>".
    #[serde(default)]
    pub client_id: Option<String>,
    /// One entry per (topic, field) mapping. The adapter subscribes to
    /// every topic and emits an Observation per matching message.
    pub subscriptions: Vec<MqttSubscription>,
}

fn default_mqtt_broker_port() -> u16 {
    1883
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MqttSubscription {
    /// MQTT topic. Wildcards supported per MQTT spec (+/# segments).
    pub topic: String,
    /// Which weather field this topic publishes.
    pub field: String, // serialized as snake_case of WeatherField variant
    /// Optional JSON path into the payload (e.g. "soil_moisture",
    /// "0.value"). When unset, the entire payload is parsed as a number.
    #[serde(default)]
    pub json_path: Option<String>,
    /// Optional zone slug for per-zone soil moisture. When set, the MQTT
    /// adapter does NOT emit a global typed Observation (which the merge bus
    /// keys by WeatherField and so cannot disambiguate per zone, and which
    /// would clobber the global humidity field). Instead it emits a per-zone
    /// soil CHANNEL recorded in sensor_history under the canonical key
    /// `soilmoisture_<zone_slug>` (see bus_recorder::zone_soil_key). A zone
    /// binds it via `soil_sensor_id = "source:<source_id>:soilmoisture_<zone_slug>"`,
    /// after which resolve_soil_pct, the /sensors/soil + /sensors/inventory
    /// discovery, and the Sensors view all treat it like a native Ecowitt
    /// `soilmoisture<N>` channel.
    #[serde(default)]
    pub zone_slug: Option<String>,
    /// Optional linear transform: published_value * scale + offset.
    /// Useful when a sensor reports raw ADC and you need percent, or
    /// when units don't match (e.g. C -> F: scale=1.8, offset=32).
    #[serde(default = "default_scale")]
    pub scale: f64,
    #[serde(default)]
    pub offset: f64,
}

fn default_scale() -> f64 {
    1.0
}

/// Generic HTTP webhook receiver. POSTs land at /ingest/<path>; field
/// mappings drill into the payload per the same json_path scheme MQTT
/// subscribe uses. Optional token gate.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HttpWebhookConfig {
    /// Path under /ingest/ where the device POSTs. e.g. "/ingest/lawn"
    /// or "/ingest/my-gateway". Must start with /.
    pub path: String,
    /// Optional shared secret. Devices include it via the
    /// X-LocalSky-Token header or ?token=<value> query parameter.
    #[serde(default)]
    pub token: Option<String>,
    /// Field mappings. Each entry: which WeatherField, optional JSON
    /// path drill-in, scale + offset for unit conversion.
    pub fields: Vec<HttpWebhookField>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HttpWebhookField {
    pub field: String,
    #[serde(default)]
    pub json_path: Option<String>,
    #[serde(default = "default_scale")]
    pub scale: f64,
    #[serde(default)]
    pub offset: f64,
}

/// Blitzortung.org community lightning feed: the volunteer-run global
/// detection network behind map.blitzortung.org. LocalSky connects to
/// the same public websocket firehose their map uses, keeps strikes
/// near the station, and renders them as a map/dashboard layer.
///
/// This is a DISPLAY-ONLY layer with hard boundaries set by the
/// project's terms (blitzortung.org contact page, captured 2026-06-12):
/// non-commercial private use, mandatory visible attribution
/// ("Lightning data: Blitzortung.org contributors, CC BY-SA 4.0"), and
/// explicitly NEVER a storm-warning/safety feature; LocalSky also never
/// feeds these strikes into irrigation or automation logic, and never
/// rebroadcasts them from project infrastructure.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BlitzortungConfig {
    /// Explicit opt-in, default FALSE, on top of the entry-level
    /// `enabled` flag (which defaults true for every source kind).
    /// Merely adding a blitzortung entry must not open a connection to
    /// the volunteer-run servers; the operator flips this consciously
    /// after reading the terms (config validation surfaces them).
    #[serde(default)]
    pub enabled: bool,
    /// Keep only strikes within this distance of the station, in miles.
    /// The feed is a global firehose with no server-side geo filter, so
    /// the radius is applied locally before buffering.
    #[serde(default = "default_blitzortung_radius_mi")]
    pub radius_mi: f64,
    /// WebSocket endpoints, rotated on failure. User-editable config
    /// rather than constants because the active host subset churns
    /// across Blitzortung web-client releases (the protocol is
    /// unversioned and serves their own map app first). An empty list
    /// falls back to these same defaults.
    #[serde(default = "default_blitzortung_hosts")]
    pub hosts: Vec<String>,
}

impl Default for BlitzortungConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            radius_mi: default_blitzortung_radius_mi(),
            hosts: default_blitzortung_hosts(),
        }
    }
}

fn default_blitzortung_radius_mi() -> f64 {
    100.0
}

/// The four hosts the blitzortung.org web client (V5.4) actually uses,
/// verified live 2026-06-12. ws1-ws8 all resolve in DNS but only this
/// subset is active in the current client release.
pub fn default_blitzortung_hosts() -> Vec<String> {
    [
        "wss://ws1.blitzortung.org/",
        "wss://ws2.blitzortung.org/",
        "wss://ws7.blitzortung.org/",
        "wss://ws8.blitzortung.org/",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct DemoReplayConfig {
    /// Replay rate. 1.0 = real-time; 10.0 = 10x; 60.0 = 1 hour per minute.
    #[serde(default = "default_demo_rate")]
    pub rate: f64,
    /// Optional path to a recorded JSONL packet stream. None = use the
    /// bundled assets/demo/tempest_replay.jsonl.
    #[serde(default)]
    pub replay_path: Option<String>,
}

fn default_demo_rate() -> f64 {
    10.0
}

// ----- Irrigation controllers -----

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ControllerEntry {
    pub id: String,
    /// When true, this controller is the default target for zones whose
    /// `controller_id` is unset. Exactly one controller should be default.
    #[serde(default)]
    pub default: bool,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(flatten)]
    pub controller: ControllerKind,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", content = "config", rename_all = "snake_case")]
pub enum ControllerKind {
    OpensprinklerDirect(OpenSprinklerDirectConfig),
    HaServiceCall(HaServiceCallConfig),
    EsphomeNative(EsphomeNativeConfig),
    Rachio(RachioConfig),
    Hydrawise(HydrawiseConfig),
    Bhyve(BhyveConfig),
    Rainbird(RainbirdConfig),
    MqttCommand(MqttCommandConfig),
    DryRun(DryRunConfig),
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct OpenSprinklerDirectConfig {
    pub host: String,
    #[serde(default = "default_os_port")]
    pub port: u16,
    /// OpenSprinkler uses md5(plaintext) lowercased as the URL `?pw=`
    /// parameter. The wizard computes this client-side; users never see
    /// the plaintext after first entry.
    pub password_md5: String,
    /// Status poll interval (seconds). Default matches v0.1.
    #[serde(default = "default_controller_poll")]
    pub poll_interval_s: u32,
}

fn default_os_port() -> u16 {
    80
}
fn default_controller_poll() -> u32 {
    10
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HaServiceCallConfig {
    pub base_url: String,
    pub bearer_token: String,
    /// Service to call to start a zone. Defaults to
    /// `script.os_zone_toggle`; users with stock Irrigation Unlimited
    /// setup may use `irrigation_unlimited.run_now`.
    #[serde(default = "default_ha_start_service")]
    pub start_service: String,
    /// Service to call to stop a zone. Default `opensprinkler.stop`.
    #[serde(default = "default_ha_stop_service")]
    pub stop_service: String,
    /// Map LocalSky zone slug -> HA station/entity. Slug = key.
    #[serde(default)]
    pub zone_entity_map: BTreeMap<String, String>,
}

fn default_ha_start_service() -> String {
    "script.os_zone_toggle".to_string()
}
fn default_ha_stop_service() -> String {
    "opensprinkler.stop".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EsphomeNativeConfig {
    pub host: String,
    #[serde(default = "default_esphome_port")]
    pub port: u16,
    pub password: Option<String>,
    /// Map LocalSky zone slug -> ESPHome switch entity_id.
    #[serde(default)]
    pub zone_entity_map: BTreeMap<String, String>,
}

fn default_esphome_port() -> u16 {
    6053
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RachioConfig {
    pub api_token: String,
    pub device_id: String,
    /// Map LocalSky zone slug -> Rachio zone UUID.
    #[serde(default)]
    pub zone_uuid_map: BTreeMap<String, String>,
}

/// Hunter Hydrawise cloud controller. The HC v3 / HPC v6 / Pro-C
/// upgrade module all report through app.hydrawise.com. Auth is a
/// per-account API key (Account > Settings > API in the customer
/// portal). The "Restful API" docs at app.hydrawise.com/config/api
/// are the source of truth.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct HydrawiseConfig {
    pub api_key: String,
    /// Controller serial / id. Used to scope status + commands when an
    /// account has multiple controllers.
    pub controller_id: i64,
    /// Map LocalSky zone slug -> Hydrawise relay_id.
    #[serde(default)]
    pub zone_relay_map: BTreeMap<String, i64>,
}

/// Orbit B-hyve cloud controller. WiFi Timer / Smart Indoor Timer /
/// XR + XD models. Uses the orbit API at api.orbitbhyve.com with a
/// token-based auth (email + password -> session token). The session
/// token is rotated by the adapter; users provide email + password.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BhyveConfig {
    pub email: String,
    pub password: String,
    /// Device id from /v1/devices response. Required to scope commands.
    pub device_id: String,
    /// Map LocalSky zone slug -> B-hyve station number (1-based).
    #[serde(default)]
    pub zone_station_map: BTreeMap<String, u32>,
}

/// Rain Bird LNK2 controller via cloud REST. The LNK2 is the WiFi
/// module bolted onto ESP-Me / ARC8 / ESP-RZXe controllers; this
/// adapter targets the rdz-rest.rainbird.com cloud (the same endpoint
/// the official Rain Bird mobile app uses).
///
/// The LAN-direct path (AES-encrypted JSON-RPC on port 80, documented
/// by the pyrainbird community) is a future wave, requires bringing in
/// aes + cbc + pbkdf2 deps. Until then, HA users can wire HA's existing
/// RainBird LAN integration through `ha_service_call` instead.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RainbirdConfig {
    /// Account email registered with the Rain Bird app.
    pub email: String,
    pub password: String,
    /// Controller serial / id. Pulled from /v1/userControllers.
    pub controller_id: String,
    /// Map LocalSky zone slug -> Rain Bird station number (1-based).
    #[serde(default)]
    pub zone_station_map: BTreeMap<String, u32>,
    /// Optional base URL override. Defaults to the production endpoint;
    /// useful if Rain Bird ever rotates hosts again.
    #[serde(default = "default_rainbird_base_url")]
    pub base_url: String,
}

fn default_rainbird_base_url() -> String {
    "https://rdz-rest.rainbird.com".to_string()
}

/// Generic MQTT command-sink controller. Publishes start/stop messages
/// to per-zone topics. Compatible with ESPHome `mqtt:` switches, Tasmota
/// relays, Sonoff/Shelly devices in MQTT mode, OpenSprinkler MQTT plug-in,
/// and any DIY relay board that subscribes to MQTT. No state subscription;
/// commands are fire-and-forget. Use ESPHome native or HaServiceCall when
/// you need confirmed state.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MqttCommandConfig {
    pub broker_host: String,
    #[serde(default = "default_mqtt_broker_port")]
    pub broker_port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    /// MQTT client id. Defaults to "localsky-controller-<id>".
    #[serde(default)]
    pub client_id: Option<String>,
    /// Map LocalSky zone slug -> command spec. Each entry specifies the
    /// command topic and the on/off payloads to publish.
    #[serde(default)]
    pub zone_command_map: BTreeMap<String, MqttZoneCommand>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MqttZoneCommand {
    /// Topic to publish the on/off command to.
    pub topic: String,
    /// Payload to publish to start the zone. Defaults to "ON".
    #[serde(default = "default_mqtt_on_payload")]
    pub on_payload: String,
    /// Payload to publish to stop the zone. Defaults to "OFF".
    #[serde(default = "default_mqtt_off_payload")]
    pub off_payload: String,
    /// Optional retain flag on published messages.
    #[serde(default)]
    pub retain: bool,
}

fn default_mqtt_on_payload() -> String {
    "ON".to_string()
}
fn default_mqtt_off_payload() -> String {
    "OFF".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct DryRunConfig {
    /// When true, every `run_zone` call ALSO writes a synthetic ran-X-min
    /// row into the runs table so the dashboard shows activity. Used by
    /// demo mode.
    #[serde(default)]
    pub simulate_runs: bool,
}

// ----- Zones -----

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ZoneConfig {
    pub display_name: String,
    pub area_sqft: f64,
    pub species: GrassSpecies,
    pub soil_texture: SoilTexture,
    #[serde(default)]
    pub slope_pct: f64,
    #[serde(default)]
    pub sun_exposure: SunExposure,
    pub sprinkler_type: SprinklerType,
    /// Measured precipitation rate (mm/hr) via catch-cup, or catalog
    /// default for `sprinkler_type`. The engine prefers measured.
    #[serde(default)]
    pub precip_rate_mm_hr: Option<f64>,
    #[serde(default)]
    pub precip_rate_source: PrecipRateSource,
    /// Root zone depth override (mm). None = use species default.
    #[serde(default)]
    pub root_depth_mm: Option<f64>,
    /// MAD percent override. None = use species default.
    #[serde(default)]
    pub mad_pct_override: Option<f64>,
    /// Which controller fires this zone.
    pub controller_id: String,
    /// Controller-specific station/zone identifier. OpenSprinkler = 1-based
    /// station number; Rachio = zone UUID; ESPHome = switch entity_id.
    pub controller_station: String,
    /// Soil moisture sensor entity (any source). Optional; engine uses
    /// modeled bucket when absent.
    #[serde(default)]
    pub soil_sensor_id: Option<String>,
    /// Phase E soil-status band low edge (%).
    #[serde(default = "default_target_min_pct")]
    pub target_min_pct_soil: f64,
    /// Saturation threshold (%). At-or-above means skip irrigation.
    #[serde(default = "default_saturation_pct")]
    pub saturation_pct_soil: f64,
    /// Optional photo URL for the zone card (Phase 10 wizard upload).
    #[serde(default)]
    pub photo_url: Option<String>,
    /// Weekly water target (inches) for the standalone budget allocator.
    /// `None` = use the agronomic default inferred from the zone slug
    /// (turf 1.0", shrub/garden/bed 0.5"). On the HA path the live
    /// `input_number.irrigation_<slug>_weekly_budget_in` helper still wins
    /// when present; this is the native (no-HA) source + the HA fallback.
    #[serde(default)]
    pub weekly_budget_in: Option<f64>,
    /// Irrigation sessions per week for the budget allocator. `None` = use
    /// the agronomic default (turf 2, shrub/garden/bed 1). Same HA-helper
    /// precedence as `weekly_budget_in`.
    #[serde(default)]
    pub sessions_per_week: Option<u32>,
}

fn default_target_min_pct() -> f64 {
    30.0
}
fn default_saturation_pct() -> f64 {
    70.0
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum PrecipRateSource {
    Measured,
    #[default]
    Catalog,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SunExposure {
    #[default]
    Full,
    Partial,
    Shade,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SprinklerType {
    Rotor,
    Spray,
    MpRotator,
    Drip,
    Bubbler,
    Other,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum GrassSpecies {
    StAugustine,
    Bermuda,
    Zoysia,
    Bahia,
    Centipede,
    KentuckyBluegrass,
    TallFescue,
    PerennialRyegrass,
    Kikuyu,
    OrnamentalShrubs,
    VegetableGarden,
    DripXeriscape,
    Other,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum SoilTexture {
    Sand,
    LoamySand,
    SandyLoam,
    Loam,
    SiltLoam,
    ClayLoam,
    Clay,
}

// ----- LLM advisor -----

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LlmConfig {
    #[serde(flatten)]
    pub provider: LlmProviderKind,
    #[serde(default = "default_llm_timeout")]
    pub timeout_s: u32,
    /// TTL for the explanation cache (seconds).
    #[serde(default = "default_explanation_ttl")]
    pub explanation_ttl_s: i64,
    /// TTL for the anomaly cache (seconds).
    #[serde(default = "default_anomaly_ttl")]
    pub anomaly_ttl_s: i64,
}

fn default_llm_timeout() -> u32 {
    20
}
fn default_explanation_ttl() -> i64 {
    300
}
fn default_anomaly_ttl() -> i64 {
    3600
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "provider", content = "config", rename_all = "snake_case")]
pub enum LlmProviderKind {
    /// Probe localhost:11434 / :8080 / :1234 at boot; first reachable wins.
    Auto(AutoProviderConfig),
    Ollama(OllamaProviderConfig),
    Llamacpp(LlamacppProviderConfig),
    /// Generic OpenAI-compatible endpoint. Covers OpenAI, Anthropic-compat
    /// shims, vLLM, LM Studio, llama.cpp's /v1 endpoint, and any internal
    /// gateway that speaks /v1/chat/completions.
    OpenaiCompat(OpenaiCompatConfig),
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct AutoProviderConfig {
    /// Override the default probe order. Each entry is a base URL.
    #[serde(default)]
    pub probe_order: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct OllamaProviderConfig {
    pub base_url: String,
    pub model: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct LlamacppProviderConfig {
    pub base_url: String,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct OpenaiCompatConfig {
    pub base_url: String,
    pub model: String,
    pub api_key: Option<String>,
}

// ----- Notifications -----

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, Default)]
pub struct Notifications {
    #[serde(default)]
    pub web_push: Option<WebPushConfig>,
    #[serde(default)]
    pub mqtt: Option<MqttConfig>,
    #[serde(default)]
    pub ntfy: Option<NtfyConfig>,
    #[serde(default)]
    pub slack: Option<SlackConfig>,
    #[serde(default)]
    pub email: Option<EmailConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WebPushConfig {
    pub vapid_public: String,
    pub vapid_private_path: String,
    pub vapid_subject: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MqttConfig {
    pub host: String,
    #[serde(default = "default_mqtt_port")]
    pub port: u16,
    pub username: Option<String>,
    pub password: Option<String>,
    #[serde(default = "default_mqtt_discovery_prefix")]
    pub discovery_prefix: String,
    #[serde(default = "default_true")]
    pub publish_enabled: bool,
    #[serde(default)]
    pub subscribe_enabled: bool,
}

fn default_mqtt_port() -> u16 {
    1883
}
fn default_mqtt_discovery_prefix() -> String {
    "homeassistant".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct NtfyConfig {
    pub base_url: String,
    pub topic: String,
    pub auth_token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SlackConfig {
    pub webhook_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EmailConfig {
    pub smtp_host: String,
    #[serde(default = "default_smtp_port")]
    pub smtp_port: u16,
    pub username: String,
    pub password: String,
    pub from_address: String,
    pub to_address: String,
    #[serde(default = "default_true")]
    pub starttls: bool,
}

fn default_smtp_port() -> u16 {
    587
}

// ----- Engine parameters -----
//
// Every constant that used to live inline in src/ha/skip_logic.rs and
// src/ha/refresher.rs becomes a typed config field with documented
// default. Operators rarely tune these, but the option is there.

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EngineParams {
    #[serde(default)]
    pub skip_rules: SkipRuleParams,
    /// Effective rain capture (gross_rain * capture_eff = soil intake).
    #[serde(default = "default_capture_eff")]
    pub capture_efficiency: f64,
    /// Rain defer threshold per session (in). Used by water-budget mode.
    #[serde(default = "default_session_rain_defer_in")]
    pub session_rain_defer_in: f64,
    /// Default soak duration between cycles (min). Per-zone override
    /// available in ZoneConfig (Phase 3+).
    #[serde(default = "default_soak_minutes")]
    pub soak_minutes: u32,
    /// ET0 method. Auto = prefer Penman-Monteith when sources provide
    /// the inputs; fall back to ASCE simplified, then Hargreaves-Samani.
    #[serde(default)]
    pub et0_method: Et0Method,
    /// Jurisdictional watering restrictions (e.g. St. Johns River WMD).
    /// Empty = no restrictions enforced. Evaluated in engine::restrictions
    /// and gated inside skip_rules::decide() before weather checks.
    #[serde(default)]
    pub watering_restrictions: Vec<WateringRestriction>,
}

impl Default for EngineParams {
    fn default() -> Self {
        Self {
            skip_rules: SkipRuleParams::default(),
            capture_efficiency: default_capture_eff(),
            session_rain_defer_in: default_session_rain_defer_in(),
            soak_minutes: default_soak_minutes(),
            et0_method: Et0Method::default(),
            watering_restrictions: Vec::new(),
        }
    }
}

/// A single regulatory or HOA watering restriction. Multiple may stack;
/// the engine ANDs every enabled, in-effective-window restriction.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WateringRestriction {
    /// Stable id (`sjrwmd_dst`, `sjrwmd_est`, `hoa`, ...). Used for
    /// dedupe and as a primary key in the settings UI.
    pub id: String,
    /// Human label shown in skip reasons and the settings list.
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Which dates this restriction applies on. The engine evaluates the
    /// window against `now.date_naive()`.
    pub effective: EffectiveWindow,
    /// Weekdays the operator is allowed to water when their address is
    /// odd-numbered. `0 = Sun .. 6 = Sat` (chrono's `num_days_from_sunday`).
    /// Empty = no parity restriction.
    #[serde(default)]
    pub allowed_weekdays_odd: Vec<u8>,
    /// Same for even-numbered addresses.
    #[serde(default)]
    pub allowed_weekdays_even: Vec<u8>,
    /// Inclusive start hour (0..23) of the forbidden window. `None` =
    /// no hour gate. SJRWMD uses 10 (10am).
    #[serde(default)]
    pub forbidden_hour_start: Option<u8>,
    /// Exclusive end hour. SJRWMD uses 16 (4pm).
    #[serde(default)]
    pub forbidden_hour_end: Option<u8>,
    /// Hard cap per zone per session (minutes). Min-of-active wins when
    /// multiple restrictions specify a cap.
    #[serde(default)]
    pub max_minutes_per_zone: Option<u32>,
}

/// When a `WateringRestriction` applies. DST/Standard windows handle
/// seasonal summer/winter restriction splits (e.g. Florida districts
/// switch rules with daylight saving). `DateRange` lets local authority,
/// council, or HOA rules name an arbitrary MM-DD..MM-DD range, including
/// wraparound (e.g. Nov-15..Feb-28).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EffectiveWindow {
    AllYear,
    /// 2nd Sunday of March → 1st Sunday of November (US DST rules).
    DstOnly,
    /// Complement of `DstOnly`.
    StandardOnly,
    DateRange {
        start_month: u8,
        start_day: u8,
        end_month: u8,
        end_day: u8,
    },
}

fn default_capture_eff() -> f64 {
    0.70
}
fn default_session_rain_defer_in() -> f64 {
    0.10
}
fn default_soak_minutes() -> u32 {
    30
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, JsonSchema, Default, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Et0Method {
    #[default]
    Auto,
    PenmanMonteith,
    AsceSimplified,
    HargreavesSamani,
    /// Defer to the configured source's native ET0 field.
    SourceNative,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SkipRuleParams {
    /// Considered "already wet" if rain_today >= this (in).
    #[serde(default = "default_already_wet_in")]
    pub already_wet_in: f64,
    /// "Raining now" if intensity > this (in/hr).
    #[serde(default = "default_rain_now_in_hr")]
    pub rain_now_in_hr: f64,
    /// Skip if forecast rain in next 4 hours >= this (in).
    #[serde(default = "default_rain_next_4h_skip_in")]
    pub rain_next_4h_skip_in: f64,
    /// 3-day rain rollup multiplier applied to user rain_skip_in.
    #[serde(default = "default_rain_3day_factor")]
    pub rain_3day_factor: f64,
    /// Heat advisory gate (temp F).
    #[serde(default = "default_heat_advisory_temp_f")]
    pub heat_advisory_temp_f: f64,
    /// Heat advisory gate (RH %).
    #[serde(default = "default_heat_advisory_humidity_pct")]
    pub heat_advisory_humidity_pct: f64,
    /// Heat advisory gate (consecutive dry days).
    #[serde(default = "default_heat_advisory_dry_days")]
    pub heat_advisory_dry_days: u32,
    /// Wind slack (mph) added to user max_wind_mph for forecast checks.
    #[serde(default = "default_wind_forecast_slack")]
    pub wind_forecast_slack_mph: f64,
    /// User-tunable defaults (also editable in HA helpers for legacy mode).
    #[serde(default = "default_max_wind_mph")]
    pub max_wind_mph: f64,
    #[serde(default = "default_min_temp_f")]
    pub min_temp_f: f64,
    #[serde(default = "default_rain_skip_in")]
    pub rain_skip_in: f64,
    #[serde(default = "default_frost_skip_soil_f")]
    pub frost_skip_soil_f: f64,
    /// Built-in engine rules the operator has switched off, by rule id
    /// (see `engine::skip_rules::builtin_rule_catalog()` for the full
    /// catalog: "rain_now", "freeze_now", "already_wet", ...). Disabled
    /// rules still appear in the decision trace for transparency but
    /// never decide. Operator-control and compliance gates ("override",
    /// "pause_until", "paused", "restrictions", "dry_run") are protected:
    /// the engine hard-enforces them and ignores them if listed here.
    #[serde(default)]
    pub disabled_rules: Vec<String>,
}

impl Default for SkipRuleParams {
    fn default() -> Self {
        Self {
            already_wet_in: default_already_wet_in(),
            rain_now_in_hr: default_rain_now_in_hr(),
            rain_next_4h_skip_in: default_rain_next_4h_skip_in(),
            rain_3day_factor: default_rain_3day_factor(),
            heat_advisory_temp_f: default_heat_advisory_temp_f(),
            heat_advisory_humidity_pct: default_heat_advisory_humidity_pct(),
            heat_advisory_dry_days: default_heat_advisory_dry_days(),
            wind_forecast_slack_mph: default_wind_forecast_slack(),
            max_wind_mph: default_max_wind_mph(),
            min_temp_f: default_min_temp_f(),
            rain_skip_in: default_rain_skip_in(),
            frost_skip_soil_f: default_frost_skip_soil_f(),
            disabled_rules: Vec::new(),
        }
    }
}

fn default_already_wet_in() -> f64 {
    0.05
}
fn default_rain_now_in_hr() -> f64 {
    0.01
}
fn default_rain_next_4h_skip_in() -> f64 {
    0.10
}
fn default_rain_3day_factor() -> f64 {
    1.5
}
fn default_heat_advisory_temp_f() -> f64 {
    95.0
}
fn default_heat_advisory_humidity_pct() -> f64 {
    60.0
}
fn default_heat_advisory_dry_days() -> u32 {
    2
}
fn default_wind_forecast_slack() -> f64 {
    5.0
}
fn default_max_wind_mph() -> f64 {
    10.0
}
fn default_min_temp_f() -> f64 {
    38.0
}
fn default_rain_skip_in() -> f64 {
    0.25
}
fn default_frost_skip_soil_f() -> f64 {
    35.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn radar_ui_default_layers_lead_with_librewxr() {
        // LibreWXR leads (the region-aware default radar), then the
        // catalog successors of the old precip + NEXRAD + strikes trio.
        assert_eq!(
            RadarUiConfig::default().default_layers,
            ["librewxr", "rainviewer", "nexrad_iem", "lightning_tempest"]
        );
    }

    #[test]
    fn ui_block_deserializes_from_empty_toml() {
        // Everything under [ui] is serde-defaulted, so a config written
        // before the block existed must keep parsing unchanged.
        let ui: UiConfig = toml::from_str("").unwrap();
        assert_eq!(
            ui.radar.default_layers,
            ["librewxr", "rainviewer", "nexrad_iem", "lightning_tempest"]
        );
        // Absent providers list means Auto (region-recommended set).
        assert!(ui.radar.providers.is_empty());
    }

    #[test]
    fn config_without_ui_block_gets_radar_defaults() {
        let cfg: Config = toml::from_str(
            r#"
            schema_version = 1
            [deployment.location]
            lat = 28.5
            lon = -81.4
            "#,
        )
        .unwrap();
        assert_eq!(
            cfg.ui.radar.default_layers,
            ["librewxr", "rainviewer", "nexrad_iem", "lightning_tempest"]
        );
        assert!(cfg.ui.radar.providers.is_empty());
    }

    #[test]
    fn open_meteo_model_defaults_to_best_match() {
        // Every OpenMeteoConfig field is serde-defaulted, so an empty
        // table yields the catalog default model.
        let om: OpenMeteoConfig = toml::from_str("").unwrap();
        assert_eq!(om.model, "best_match");
    }

    #[test]
    fn pre_model_config_loads_with_best_match() {
        // A config written before the `model` field existed must keep
        // parsing and land on best_match (identical fetch behavior).
        let cfg: Config = toml::from_str(
            r#"
            schema_version = 1
            [deployment.location]
            lat = 28.5
            lon = -81.4
            [[sources]]
            id = "open_meteo"
            kind = "open_meteo"
            [sources.config]
            forecast_days = 7
            "#,
        )
        .unwrap();
        let SourceKind::OpenMeteo(om) = &cfg.sources[0].source else {
            panic!("expected an open_meteo source");
        };
        assert_eq!(om.model, "best_match");
    }

    #[test]
    fn explicit_open_meteo_model_round_trips() {
        let cfg: Config = toml::from_str(
            r#"
            schema_version = 1
            [deployment.location]
            lat = 48.14
            lon = 11.58
            [[sources]]
            id = "open_meteo"
            kind = "open_meteo"
            [sources.config]
            model = "icon_seamless"
            "#,
        )
        .unwrap();
        let SourceKind::OpenMeteo(om) = &cfg.sources[0].source else {
            panic!("expected an open_meteo source");
        };
        assert_eq!(om.model, "icon_seamless");
    }

    #[test]
    fn radar_ui_explicit_providers_parse_in_order() {
        let ui: UiConfig = toml::from_str(
            r#"
            [radar]
            providers = ["geomet_ca", "rainviewer"]
            default_layers = ["rainviewer", "warnings_us"]
            "#,
        )
        .unwrap();
        assert_eq!(ui.radar.providers, ["geomet_ca", "rainviewer"]);
        assert_eq!(ui.radar.default_layers, ["rainviewer", "warnings_us"]);
    }
}
