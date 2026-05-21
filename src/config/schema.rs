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
        }
    }
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
    Nws(NwsConfig),
    OpenWeather(OpenWeatherConfig),
    PirateWeather(PirateWeatherConfig),
    MetNorway(MetNorwayConfig),
    AmbientWeather(AmbientWeatherConfig),
    HaPassthrough(HaPassthroughConfig),
    /// Subscribe to MQTT topics from any publisher (Tasmota, ESPHome,
    /// Zigbee2MQTT, raw MQTT publishers). Works standalone (no HA
    /// required) when paired with a broker like Mosquitto.
    Mqtt(MqttSourceConfig),
    /// Generic HTTP webhook receiver. Any device that can POST JSON
    /// (Arduino, Pi script, custom commercial gateway) feeds LocalSky
    /// without needing MQTT or HA.
    HttpWebhook(HttpWebhookConfig),
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

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AmbientWeatherConfig {
    pub app_key: String,
    pub api_key: String,
    pub mac_address: String,
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
    /// Optional zone slug for per-zone fields (soil moisture, soil temp).
    /// When set, the field is stored as "<field>_<zone_slug>" so the
    /// engine can disambiguate.
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
}

impl Default for EngineParams {
    fn default() -> Self {
        Self {
            skip_rules: SkipRuleParams::default(),
            capture_efficiency: default_capture_eff(),
            session_rain_defer_in: default_session_rain_defer_in(),
            soak_minutes: default_soak_minutes(),
            et0_method: Et0Method::default(),
        }
    }
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
