// Declarative base-config field schema for every weather/sensor SourceKind.
//
// The goal: adding or editing a source's CONNECTION details (host, port,
// base_url, tokens, api keys, poll cadence, model, ...) is point-and-click,
// never a raw-JSON edit. One metadata table (`source_fields`) drives one
// generic renderer (`SourceConfigForm`), so we get 20+ labeled forms without
// 20 bespoke components.
//
// Design (mirrors soil_forms.rs):
//   * `FieldSpec` describes ONE scalar config key: its JSON key (byte-identical
//     to what the engine's serde struct deserializes), label, widget type,
//     helptext, required flag, default, placeholder, and a secret flag (render
//     as a masked password input, but persist plainly: config is tracked).
//   * `source_fields(kind)` returns the &[FieldSpec] for a kind's BASE scalars.
//     Complex nested keys (MQTT `subscriptions`, Ecowitt `soil_calibration`,
//     HA `field_map`, the per-device maps on YoLink/Tuya) are intentionally
//     omitted here: they already have bespoke forms (soil_forms.rs) or stay in
//     the JSON-advanced escape hatch. The generic form only owns scalars.
//   * `SourceConfigForm` seeds each input from the shared `config_text` signal
//     (parsed as JSON), and on every change writes the typed value back into
//     `config_text` using the SAME self_edit guard-flag two-way sync as
//     soil_forms.rs, so the JSON-advanced textarea stays authoritative for
//     anything not in the schema and external edits re-seed without clobber.
//
// EVERY SourceKind variant must have a `source_fields` entry (asserted by a
// coverage test against the wizard/settings kind list); kinds whose entire
// config is one nested structure (none today, but the contract is enforced)
// would return an empty slice deliberately.

use leptos::prelude::*;

use crate::components::ui::{FormField, SecretInput};
use crate::components::units_fmt::{
    distance_unit, distance_value_mi, mi_to_km, use_unit_prefs, UnitPrefs,
};

/// A convertible physical-unit dimension a numeric field is expressed in. The
/// VALUE persisted to JSON always stays in the engine's imperial-as-stored unit
/// (the serde field name encodes it, e.g. `radius_mi`); this marker only drives
/// the DISPLAY boundary -- the label's unit suffix and the value shown/typed in
/// the input -- through the shared units_fmt helpers, so a metric preference
/// shows km without ever changing the stored miles. Extend as more physical
/// numeric config fields appear (rain depth, temperature, etc.).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DisplayUnit {
    /// Source value is MILES; display via distance_value_mi / distance_unit.
    DistanceMi,
}

impl DisplayUnit {
    /// The display-unit suffix (e.g. "mi" or "km") for the active prefs.
    fn unit_label(self, p: UnitPrefs) -> &'static str {
        match self {
            DisplayUnit::DistanceMi => distance_unit(p),
        }
    }

    /// Convert a stored (imperial) source value into the displayed value string.
    fn value_to_display(self, source: f64, p: UnitPrefs) -> String {
        match self {
            DisplayUnit::DistanceMi => distance_value_mi(source, p),
        }
    }

    /// Convert a displayed value (in the active unit) back to the stored
    /// imperial source value, so the wire format stays imperial-as-stored.
    fn display_to_source(self, displayed: f64, p: UnitPrefs) -> f64 {
        match self {
            // Inverse of mi_to_km when metric; identity when imperial.
            DisplayUnit::DistanceMi => {
                if p.distance_metric {
                    displayed / (mi_to_km(1.0))
                } else {
                    displayed
                }
            }
        }
    }
}

/// Widget kind for one config field.
#[derive(Clone, Debug, PartialEq)]
pub enum FieldType {
    /// Free text -> JSON string.
    Text,
    /// Numeric -> JSON number. `integer` true emits an integer (u/i), false a
    /// float; both are parsed leniently and only written when they parse.
    Number { integer: bool },
    /// Masked input (type=password) -> JSON string. Persisted plainly; config
    /// is tracked, so this is presentation-only obfuscation.
    Password,
    /// Checkbox -> JSON bool.
    Bool,
    /// <select> over (value, label) options -> JSON string.
    Select(&'static [(&'static str, &'static str)]),
}

/// A default value for a field, typed so the renderer can both seed the input
/// and decide what to write when the key is absent.
#[derive(Clone, Debug, PartialEq)]
pub enum FieldDefault {
    /// No default; absent key renders blank.
    None,
    Str(&'static str),
    /// Numeric default; rendered via the input as a string.
    Num(f64),
    Bool(bool),
}

/// Metadata for ONE scalar config key.
#[derive(Clone, Debug, PartialEq)]
pub struct FieldSpec {
    /// JSON key, byte-identical to the serde field the engine deserializes.
    pub key: &'static str,
    /// Human label shown above the input.
    pub label: &'static str,
    pub field_type: FieldType,
    /// One-line guidance under the label.
    pub helptext: &'static str,
    /// When true, an empty value surfaces an inline "required" error.
    pub required: bool,
    pub default: FieldDefault,
    /// Placeholder shown in empty text/number/password inputs.
    pub placeholder: &'static str,
    /// Render as a masked password input (still persisted plainly).
    pub secret: bool,
    /// When Some, this numeric field carries a convertible physical unit. The
    /// VALUE on the wire stays imperial-as-stored; the renderer converts the
    /// displayed/typed value and the label's unit suffix through units_fmt at
    /// the display boundary only. None for non-physical numbers (counts, ports,
    /// seconds, ids) and all non-numeric fields.
    pub display_unit: Option<DisplayUnit>,
    /// When Some, an absolute "Where to get this ->" link rendered under the
    /// field, pointing the user at the provider page where they create/find the
    /// credential (e.g. openweathermap.org/api). Only set on the secret/account
    /// fields that strand a newcomer otherwise; None for self-explanatory fields.
    pub doc_url: Option<&'static str>,
}

impl FieldSpec {
    pub(crate) const fn text(
        key: &'static str,
        label: &'static str,
        helptext: &'static str,
        required: bool,
        placeholder: &'static str,
    ) -> Self {
        Self {
            key,
            label,
            field_type: FieldType::Text,
            helptext,
            required,
            default: FieldDefault::None,
            placeholder,
            secret: false,
            display_unit: None,
            doc_url: None,
        }
    }

    pub(crate) const fn secret(
        key: &'static str,
        label: &'static str,
        helptext: &'static str,
        required: bool,
        placeholder: &'static str,
    ) -> Self {
        Self {
            key,
            label,
            field_type: FieldType::Password,
            helptext,
            required,
            default: FieldDefault::None,
            placeholder,
            secret: true,
            display_unit: None,
            doc_url: None,
        }
    }

    /// A masked secret field that ALSO carries a "Where to get this ->" link to
    /// the provider's credential page. Same persistence as `secret`; the only
    /// addition is `doc_url`, rendered under the field so a newcomer is never
    /// stranded wondering where the key comes from.
    pub(crate) const fn secret_doc(
        key: &'static str,
        label: &'static str,
        helptext: &'static str,
        required: bool,
        placeholder: &'static str,
        doc_url: &'static str,
    ) -> Self {
        Self {
            key,
            label,
            field_type: FieldType::Password,
            helptext,
            required,
            default: FieldDefault::None,
            placeholder,
            secret: true,
            display_unit: None,
            doc_url: Some(doc_url),
        }
    }

    /// A free-text field that carries a "Where to get this ->" link. For the
    /// public-identifier credentials (OAuth client id, account email) that pair
    /// with a secret but aren't themselves masked, yet still send the user to a
    /// provider page to obtain them.
    pub(crate) const fn text_doc(
        key: &'static str,
        label: &'static str,
        helptext: &'static str,
        required: bool,
        placeholder: &'static str,
        doc_url: &'static str,
    ) -> Self {
        Self {
            key,
            label,
            field_type: FieldType::Text,
            helptext,
            required,
            default: FieldDefault::None,
            placeholder,
            secret: false,
            display_unit: None,
            doc_url: Some(doc_url),
        }
    }

    pub(crate) const fn int(
        key: &'static str,
        label: &'static str,
        helptext: &'static str,
        default: f64,
    ) -> Self {
        Self {
            key,
            label,
            field_type: FieldType::Number { integer: true },
            helptext,
            required: false,
            default: FieldDefault::Num(default),
            placeholder: "",
            secret: false,
            display_unit: None,
            doc_url: None,
        }
    }

    /// A REQUIRED integer. No sentinel default (FieldDefault::None), so the
    /// input starts empty with a guiding placeholder rather than seeding a bogus
    /// 0; an empty value surfaces the inline "<label> is required" error and is
    /// OMITTED from the config (never written as 0) so a half-filled add can't
    /// silently persist a broken source.
    pub(crate) const fn int_required(
        key: &'static str,
        label: &'static str,
        helptext: &'static str,
        placeholder: &'static str,
    ) -> Self {
        Self {
            key,
            label,
            field_type: FieldType::Number { integer: true },
            helptext,
            required: true,
            default: FieldDefault::None,
            placeholder,
            secret: false,
            display_unit: None,
            doc_url: None,
        }
    }

    pub(crate) const fn float(
        key: &'static str,
        label: &'static str,
        helptext: &'static str,
        default: f64,
    ) -> Self {
        Self {
            key,
            label,
            field_type: FieldType::Number { integer: false },
            helptext,
            required: false,
            default: FieldDefault::Num(default),
            placeholder: "",
            secret: false,
            display_unit: None,
            doc_url: None,
        }
    }

    /// An optional float carrying a convertible physical unit. The `default` and
    /// the stored value stay in the source (imperial) unit; the renderer shows /
    /// accepts the active display unit and adjusts the label suffix via
    /// units_fmt. Use for physical numeric config (e.g. Blitzortung radius_mi).
    pub(crate) const fn float_unit(
        key: &'static str,
        label: &'static str,
        helptext: &'static str,
        default: f64,
        display_unit: DisplayUnit,
    ) -> Self {
        Self {
            key,
            label,
            field_type: FieldType::Number { integer: false },
            helptext,
            required: false,
            default: FieldDefault::Num(default),
            placeholder: "",
            secret: false,
            display_unit: Some(display_unit),
            doc_url: None,
        }
    }

    /// A REQUIRED float. Mirror of `int_required` for non-integer numbers: no
    /// sentinel default, inline required error when empty, key omitted on empty.
    #[allow(dead_code)]
    pub(crate) const fn float_required(
        key: &'static str,
        label: &'static str,
        helptext: &'static str,
        placeholder: &'static str,
    ) -> Self {
        Self {
            key,
            label,
            field_type: FieldType::Number { integer: false },
            helptext,
            required: true,
            default: FieldDefault::None,
            placeholder,
            secret: false,
            display_unit: None,
            doc_url: None,
        }
    }

    pub(crate) const fn boolean(
        key: &'static str,
        label: &'static str,
        helptext: &'static str,
        default: bool,
    ) -> Self {
        Self {
            key,
            label,
            field_type: FieldType::Bool,
            helptext,
            required: false,
            default: FieldDefault::Bool(default),
            placeholder: "",
            secret: false,
            display_unit: None,
            doc_url: None,
        }
    }

    pub(crate) const fn text_default(
        key: &'static str,
        label: &'static str,
        helptext: &'static str,
        default: &'static str,
        placeholder: &'static str,
    ) -> Self {
        Self {
            key,
            label,
            field_type: FieldType::Text,
            helptext,
            required: false,
            default: FieldDefault::Str(default),
            placeholder,
            secret: false,
            display_unit: None,
            doc_url: None,
        }
    }

    pub(crate) const fn select(
        key: &'static str,
        label: &'static str,
        helptext: &'static str,
        options: &'static [(&'static str, &'static str)],
        default: &'static str,
    ) -> Self {
        Self {
            key,
            label,
            field_type: FieldType::Select(options),
            helptext,
            required: false,
            default: FieldDefault::Str(default),
            placeholder: "",
            secret: false,
            display_unit: None,
            doc_url: None,
        }
    }
}

// Open-Meteo model menu mirrors forecast::model_catalog ids; the default
// "best_match" keeps the fetch URL identical to pre-model-selection configs.
const OPEN_METEO_MODELS: &[(&str, &str)] = &[
    ("best_match", "Best match (auto)"),
    ("gfs_seamless", "NOAA GFS"),
    ("ecmwf_ifs025", "ECMWF IFS"),
    ("icon_seamless", "DWD ICON"),
    ("gem_seamless", "Canadian GEM"),
    ("meteofrance_seamless", "Météo-France"),
];

// Tuya regional endpoints (the four documented openapi.tuya{us|eu|cn|in}.com).
const TUYA_REGIONS: &[(&str, &str)] = &[
    ("https://openapi.tuyaus.com", "Americas (US)"),
    ("https://openapi.tuyaeu.com", "Europe (EU)"),
    ("https://openapi.tuyacn.com", "China (CN)"),
    ("https://openapi.tuyain.com", "India (IN)"),
];

// Per-kind field tables. Each is a module-level `static` (the constructors are
// `const fn`, so these are baked at compile time) so `source_fields` can return
// a `&'static [FieldSpec]` instead of a borrow of a temporary array.

static F_TEMPEST_UDP: &[FieldSpec] = &[
    FieldSpec::text_default(
        "bind_addr",
        "Listen address",
        "Where LocalSky LISTENS for the Tempest -- not the device's IP. The hub \
         broadcasts its readings over your LAN on its own; you don't enter an IP. \
         Leave the default (all interfaces, the Tempest port 50222).",
        "0.0.0.0:50222",
        "0.0.0.0:50222",
    ),
    FieldSpec::text(
        "hub_serial",
        "Hub serial (optional)",
        "Filter to one Tempest hub (e.g. HB-00012345). Leave blank to accept any hub on the LAN.",
        false,
        "e.g. HB-00012345",
    ),
];

static F_TEMPEST_WS: &[FieldSpec] = &[
    FieldSpec::secret_doc(
        "access_token",
        "Access token",
        "Tempest personal-use access token from tempestwx.com (Settings, Data Authorizations).",
        true,
        "your Tempest token",
        "https://tempestwx.com/settings/tokens",
    ),
    FieldSpec::int_required(
        "station_id",
        "Station ID",
        "Numeric station id from your Tempest account.",
        "e.g. 12345",
    ),
];

static F_DAVIS_WLL: &[FieldSpec] = &[
    FieldSpec::text(
        "host",
        "Device IP / host",
        "The WeatherLink Live device's own IP (or hostname) on your LAN -- find it in your router or the WeatherLink app.",
        true,
        "weatherlinklive.local",
    ),
    FieldSpec::int(
        "txid",
        "Transmitter ID",
        "ISS transmitter id (txid) to read. Most single-station installs use 1.",
        1.0,
    ),
];

static F_ECOWITT_LOCAL: &[FieldSpec] = &[
    FieldSpec::text_default(
        "path",
        "Ingest path",
        "HTTP path LocalSky listens on for the gateway's Customized (Ecowitt protocol) POST.",
        "/ingest/ecowitt",
        "/ingest/ecowitt",
    ),
    FieldSpec::secret(
        "shared_secret",
        "Shared secret (optional)",
        "Optional ?key=... value the gateway appends to the POST URL. Leave blank for none.",
        false,
        "none",
    ),
];

static F_ECOWITT_GW_POLL: &[FieldSpec] = &[
    FieldSpec::text(
        "host",
        "Gateway IP / host",
        "The Ecowitt gateway's own IP (or hostname) on your LAN -- find it in the WS View app or your router. LocalSky polls its local API (no cloud).",
        true,
        "192.0.2.50",
    ),
    FieldSpec::int(
        "poll_interval_s",
        "Poll interval (seconds)",
        "How often to poll the gateway. 30s is plenty for irrigation and easy on the device.",
        30.0,
    ),
];

static F_AMBIENT_WEATHER: &[FieldSpec] = &[
    FieldSpec::secret_doc(
        "app_key",
        "Application key",
        "Ambient Weather application key (ambientweather.net, Account, API Keys).",
        true,
        "your application key",
        "https://ambientweather.net/account",
    ),
    FieldSpec::secret(
        "api_key",
        "API key",
        "Ambient Weather API key paired with the application key.",
        true,
        "your API key",
    ),
    FieldSpec::text(
        "mac_address",
        "Station MAC",
        "MAC address of your station as shown in the Ambient Weather dashboard.",
        true,
        "AA:BB:CC:DD:EE:FF",
    ),
];

static F_NETATMO: &[FieldSpec] = &[
    // OAuth client id is a public identifier, not a secret: visible + not redacted.
    FieldSpec::text_doc(
        "client_id",
        "Client ID",
        "Netatmo app client id from dev.netatmo.com.",
        true,
        "your client id",
        "https://dev.netatmo.com/apps",
    ),
    FieldSpec::secret(
        "client_secret",
        "Client secret",
        "Netatmo app client secret. Treat like a password.",
        true,
        "your client secret",
    ),
    FieldSpec::secret(
        "refresh_token",
        "Refresh token",
        "OAuth2 refresh token from the one-time browser auth. The adapter rotates access tokens itself.",
        true,
        "your refresh token",
    ),
    FieldSpec::text(
        "device_id",
        "Station MAC",
        "MAC of the main station (e.g. 70:ee:50:00:11:22). The response is filtered to this MAC.",
        true,
        "70:ee:50:00:11:22",
    ),
];

static F_YOLINK: &[FieldSpec] = &[
    // UAID (the YoLink client id) is a public identifier, not a secret.
    FieldSpec::text_doc(
        "client_id",
        "UAID",
        "YoLink UAID + Secret Key come from the YoLink phone app: Account, Advanced \
         Settings, User Access Credentials, then create a pair.",
        true,
        "your UAID",
        "https://www.yosmart.com",
    ),
    FieldSpec::secret(
        "client_secret",
        "Secret key",
        "YoLink secret key paired with the UAID. Treat like a password.",
        true,
        "your secret key",
    ),
    FieldSpec::text_default(
        "base_url",
        "API base URL",
        "YoLink API endpoint. The default is correct for nearly everyone.",
        "https://api.yosmart.com",
        "https://api.yosmart.com",
    ),
];

static F_LACROSSE: &[FieldSpec] = &[
    FieldSpec::text(
        "email",
        "Account email",
        "Email for your La Crosse View account.",
        true,
        "you@example.com",
    ),
    FieldSpec::secret(
        "password",
        "Password",
        "Password for your La Crosse View account.",
        true,
        "your password",
    ),
    FieldSpec::text(
        "device_id",
        "Device ID (optional)",
        "Specific device (e.g. LTV-WSDTH04). Leave blank to use the first active device on the account.",
        false,
        "e.g. LTV-WSDTH04",
    ),
];

static F_TUYA_CLOUD: &[FieldSpec] = &[
    // Tuya Access ID (the client_id) is a public identifier, not a secret.
    FieldSpec::text_doc(
        "client_id",
        "Access ID",
        "Tuya IoT Platform access_id (client_id) from your Cloud project (iot.tuya.com).",
        true,
        "your access id",
        "https://iot.tuya.com",
    ),
    FieldSpec::secret(
        "client_secret",
        "Access secret",
        "Tuya access_secret paired with the access id. Treat like a password.",
        true,
        "your access secret",
    ),
    FieldSpec::select(
        "base_url",
        "Region",
        "Regional Tuya data center. Match the region where your Smart Life / Tuya account lives.",
        TUYA_REGIONS,
        "https://openapi.tuyaus.com",
    ),
];

static F_OPEN_METEO: &[FieldSpec] = &[
    FieldSpec::int(
        "forecast_days",
        "Forecast days",
        "How many days of daily forecast to fetch.",
        7.0,
    ),
    FieldSpec::int(
        "forecast_hours",
        "Forecast hours",
        "How many hours of hourly forecast to fetch.",
        48.0,
    ),
    FieldSpec::int(
        "past_days",
        "Past days",
        "How many days of recent history to backfill (for rain accounting).",
        1.0,
    ),
    FieldSpec::boolean(
        "include_radar",
        "Provide radar",
        "On by default. Lets this Open-Meteo source power the Live Radar map's precipitation overlay. Turn off only if you do not want a radar map from Open-Meteo.",
        true,
    ),
    FieldSpec::select(
        "model",
        "Weather model",
        "Which Open-Meteo model to query. Best match auto-blends the regionally strongest model.",
        OPEN_METEO_MODELS,
        "best_match",
    ),
];

static F_NWS: &[FieldSpec] = &[FieldSpec::text(
    "user_agent",
    "User-Agent",
    "Required by api.weather.gov to identify you (plain ASCII), e.g. LocalSky (you@example.com).",
    true,
    "LocalSky (you@example.com)",
)];

static F_MET_NORWAY: &[FieldSpec] = &[FieldSpec::text(
    "user_agent",
    "User-Agent",
    "Required by api.met.no terms of service to identify you, e.g. LocalSky (you@example.com).",
    true,
    "LocalSky (you@example.com)",
)];

static F_OPENWEATHER: &[FieldSpec] = &[FieldSpec::secret_doc(
    "api_key",
    "API key",
    "OpenWeather API key from openweathermap.org (free tier; API keys tab after sign-up).",
    true,
    "your OpenWeather key",
    "https://openweathermap.org/api",
)];

static F_PIRATE_WEATHER: &[FieldSpec] = &[FieldSpec::secret_doc(
    "api_key",
    "API key",
    "Pirate Weather API key from pirateweather.net (free tier; register for a key).",
    true,
    "your Pirate Weather key",
    "https://pirateweather.net",
)];

static F_SYNOPTIC: &[FieldSpec] = &[
    FieldSpec::secret_doc(
        "token",
        "API token",
        "Synoptic public API token from synopticdata.com (free tier; create a token in your account).",
        true,
        "your Synoptic token",
        "https://synopticdata.com/",
    ),
    FieldSpec::text(
        "station_id",
        "Station ID (optional)",
        "Pin a specific station by its STID (e.g. KSLC). Leave blank to use the nearest reporting station to your location.",
        false,
        "e.g. KSLC",
    ),
    FieldSpec::float(
        "radius_mi",
        "Search radius (miles)",
        "How far to search for the nearest station when no station id is pinned. 25 miles is a good default.",
        25.0,
    ),
];

static F_MQTT: &[FieldSpec] = &[
    FieldSpec::text(
        "broker_host",
        "Broker host",
        "MQTT broker hostname or IP (Mosquitto, EMQX, the HA broker, anything MQTT 3.1.1/5.0).",
        true,
        "broker.local",
    ),
    FieldSpec::int(
        "broker_port",
        "Broker port",
        "MQTT broker port. 1883 is the unencrypted default.",
        1883.0,
    ),
    FieldSpec::text(
        "username",
        "Username (optional)",
        "Broker username, if your broker requires auth.",
        false,
        "none",
    ),
    FieldSpec::secret(
        "password",
        "Password (optional)",
        "Broker password, if your broker requires auth.",
        false,
        "none",
    ),
    FieldSpec::text(
        "client_id",
        "Client ID (optional)",
        "MQTT client id. Leave blank for the auto default (localsky-source-<id>).",
        false,
        "localsky-source-<id>",
    ),
];

static F_HTTP_WEBHOOK: &[FieldSpec] = &[
    FieldSpec::text(
        "path",
        "Ingest path",
        "Path under /ingest where the device POSTs (must start with /), e.g. /ingest/lawn.",
        true,
        "/ingest/webhook/myhook",
    ),
    FieldSpec::secret(
        "token",
        "Shared token (optional)",
        "Optional secret devices send via X-LocalSky-Token header or ?token=. Leave blank for open.",
        false,
        "changeme",
    ),
];

static F_REST_METHODS: &[(&str, &str)] = &[("GET", "GET"), ("POST", "POST")];

static F_REST_POLL: &[FieldSpec] = &[
    FieldSpec::text(
        "url",
        "API URL",
        "Full URL to poll (HTTPS recommended). Put query-param API keys here. The field mappings (JSON paths) are edited in Advanced.",
        true,
        "https://api.example.com/current?apiKey=...",
    ),
    FieldSpec::select(
        "method",
        "HTTP method",
        "GET for most weather APIs; POST when the API needs a request body.",
        F_REST_METHODS,
        "GET",
    ),
    FieldSpec::int(
        "poll_interval_s",
        "Poll interval (seconds)",
        "How often to poll. 300s (5 min) is friendly to rate-limited APIs.",
        300.0,
    ),
];

static F_PROMETHEUS: &[FieldSpec] = &[
    FieldSpec::text(
        "url",
        "Prometheus URL",
        "Base URL of your Prometheus server. The PromQL queries (one per reading) are edited in Advanced.",
        true,
        "http://prometheus.local:9090",
    ),
    FieldSpec::int(
        "poll_interval_s",
        "Poll interval (seconds)",
        "How often to evaluate the queries. 60s is typical.",
        60.0,
    ),
    FieldSpec::text(
        "username",
        "Basic-auth user (optional)",
        "Only if your Prometheus is behind HTTP basic-auth (e.g. a reverse proxy). Leave blank otherwise.",
        false,
        "",
    ),
    FieldSpec::secret(
        "password",
        "Basic-auth password (optional)",
        "Paired with the basic-auth user. Leave blank if Prometheus is unauthenticated.",
        false,
        "",
    ),
];

static F_WEATHERKIT: &[FieldSpec] = &[
    FieldSpec::text(
        "key_id",
        "Key ID",
        "The WeatherKit key's Key ID from the Apple Developer portal.",
        true,
        "ABC123KEYID",
    ),
    FieldSpec::text(
        "team_id",
        "Team ID",
        "Your Apple Developer Team ID.",
        true,
        "DEF456TEAM",
    ),
    FieldSpec::text(
        "service_id",
        "Service ID",
        "The WeatherKit service identifier you registered (e.g. com.example.localsky).",
        true,
        "com.example.localsky",
    ),
    FieldSpec::secret_doc(
        "private_key_pem",
        "Private key (.p8)",
        "Contents of the downloaded .p8 key (PKCS#8 PEM). Treat like a password; LocalSky signs JWTs with it locally. Needs a paid Apple Developer account; expect ~5-10 min to register a WeatherKit key + Service ID.",
        true,
        "-----BEGIN PRIVATE KEY-----",
        "https://developer.apple.com/weatherkit/",
    ),
];

static F_INFLUXDB: &[FieldSpec] = &[
    FieldSpec::text(
        "url",
        "InfluxDB URL",
        "Base URL of your InfluxDB server. The InfluxQL queries (one per reading) are edited in Advanced.",
        true,
        "http://influxdb.local:8086",
    ),
    FieldSpec::text(
        "database",
        "Database / bucket",
        "v1 database name, or the v1-compat bucket mapping for v2 (the `db` query param).",
        true,
        "weather",
    ),
    FieldSpec::secret(
        "token",
        "API token (v2, optional)",
        "InfluxDB 2.x API token (sent as Authorization: Token). Leave blank for v1 with basic-auth or an open instance.",
        false,
        "",
    ),
    FieldSpec::int(
        "poll_interval_s",
        "Poll interval (seconds)",
        "How often to run the queries. 60s is typical.",
        60.0,
    ),
];

static F_HA_PASSTHROUGH: &[FieldSpec] = &[
    FieldSpec::text(
        "base_url",
        "Home Assistant URL",
        "Base URL of your Home Assistant instance.",
        true,
        "http://homeassistant.local:8123",
    ),
    FieldSpec::secret(
        "bearer_token",
        "Long-lived token",
        "HA long-lived access token (Profile, Security, Long-Lived Access Tokens). Treat like a password.",
        true,
        "your HA token",
    ),
];

static F_BLITZORTUNG: &[FieldSpec] = &[
    FieldSpec::boolean(
        "enabled",
        "Connect to Blitzortung",
        "Explicit opt-in (default off). Community CC BY-SA data, display-only; never a safety feature.",
        false,
    ),
    FieldSpec::float_unit(
        // Stored key + value stay miles (radius_mi); the label suffix and the
        // shown/typed value convert at the display boundary via units_fmt, so a
        // km preference shows km without changing the wire format.
        "radius_mi",
        "Radius",
        "Keep only strikes within this distance of the station; the global firehose is filtered locally.",
        100.0,
        DisplayUnit::DistanceMi,
    ),
];

static F_DEMO_REPLAY: &[FieldSpec] = &[
    FieldSpec::float(
        "rate",
        "Replay rate",
        "Playback speed. 1 = real-time, 10 = 10x, 60 = 1 hour per minute.",
        10.0,
    ),
    FieldSpec::text(
        "replay_path",
        "Replay file (optional)",
        "Path to a recorded JSONL packet stream. Leave blank to use the bundled demo replay.",
        false,
        "bundled demo",
    ),
];

/// The scalar base-config fields for a source kind, as serde keys + UI
/// metadata. Returns an empty slice for kinds with no scalar base config (none
/// today; the contract is that EVERY kind still has an entry here, even if
/// empty, so none is silently form-less). Nested keys (subscriptions,
/// soil_calibration, field_map, device_field_map) are handled by bespoke forms
/// or the JSON-advanced escape hatch and are deliberately NOT listed.
pub fn source_fields(kind: &str) -> &'static [FieldSpec] {
    match kind {
        // ---- LAN stations ----
        "tempest_udp" => F_TEMPEST_UDP,
        "tempest_ws" => F_TEMPEST_WS,
        "davis_wll" => F_DAVIS_WLL,
        // ---- Ecowitt ----
        "ecowitt_local" => F_ECOWITT_LOCAL,
        "ecowitt_gw_poll" => F_ECOWITT_GW_POLL,
        // ---- Cloud personal weather stations ----
        "ambient_weather" => F_AMBIENT_WEATHER,
        "netatmo" => F_NETATMO,
        "yolink" => F_YOLINK,
        "lacrosse" => F_LACROSSE,
        "tuya_cloud" => F_TUYA_CLOUD,
        // ---- Forecast models ----
        "open_meteo" => F_OPEN_METEO,
        "nws" => F_NWS,
        // NOAA MRMS is a fully keyless region-auto-seeded source (US-only radar
        // QPE, no account, no scalar config), seeded like NWS rather than added
        // by hand, so it is NOT in kind_options() and carries no base-config
        // fields. The JSON-advanced editor covers the lone optional product knob.
        "noaa_mrms" => &[],
        "met_norway" => F_MET_NORWAY,
        "synoptic" => F_SYNOPTIC,
        "openweather" => F_OPENWEATHER,
        "pirate_weather" => F_PIRATE_WEATHER,
        // ---- Generic ingest ----
        "mqtt" => F_MQTT,
        "http_webhook" => F_HTTP_WEBHOOK,
        "rest_poll" => F_REST_POLL,
        "prometheus" => F_PROMETHEUS,
        "influxdb" => F_INFLUXDB,
        "weatherkit" => F_WEATHERKIT,
        "ha_passthrough" => F_HA_PASSTHROUGH,
        // ---- Display-only / demo ----
        "blitzortung" => F_BLITZORTUNG,
        "demo_replay" => F_DEMO_REPLAY,
        // Unknown kind: no schema. The generic form renders nothing and the
        // JSON-advanced textarea is the sole editor. The coverage test ensures
        // no SHIPPED kind falls into this arm.
        _ => &[],
    }
}

// ===================================================================
// Pure logic: seed one field value out of the config JSON
// ===================================================================

/// Read a field's current string value out of the parsed config object. Numbers
/// stringify to their JSON form; bools to "true"/"false"; missing keys fall back
/// to the spec default (so a fresh add shows the right defaults). Returns "" for
/// no value + no default. Used to seed text/number/password inputs.
pub fn field_string_value(config: &serde_json::Value, spec: &FieldSpec) -> String {
    match config.get(spec.key) {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Number(n)) => n.to_string(),
        Some(serde_json::Value::Bool(b)) => b.to_string(),
        // Explicit null (the common Option<String> = None shape) -> fall through
        // to the default so the placeholder/default still seeds.
        Some(serde_json::Value::Null) | None => match &spec.default {
            FieldDefault::Str(s) => s.to_string(),
            FieldDefault::Num(n) => num_to_string(*n, spec),
            FieldDefault::Bool(b) => b.to_string(),
            FieldDefault::None => String::new(),
        },
        // Arrays/objects are nested keys this form does not own; never happens
        // for scalar specs, but render blank rather than dumping JSON.
        Some(_) => String::new(),
    }
}

/// Read a field's current bool value (for checkboxes). Missing -> spec default.
pub fn field_bool_value(config: &serde_json::Value, spec: &FieldSpec) -> bool {
    match config.get(spec.key) {
        Some(serde_json::Value::Bool(b)) => *b,
        _ => matches!(spec.default, FieldDefault::Bool(true)),
    }
}

/// Render an integer-vs-float default cleanly (e.g. 1883 not "1883.0").
fn num_to_string(n: f64, spec: &FieldSpec) -> String {
    if matches!(spec.field_type, FieldType::Number { integer: true }) || n.fract() == 0.0 {
        format!("{}", n as i64)
    } else {
        n.to_string()
    }
}

/// Write a typed value for `spec` into the config object at `spec.key`,
/// preserving every other key. Empty strings on an OPTIONAL text/password field
/// become JSON null (the Option<String> = None shape the engine expects);
/// empty strings on a REQUIRED field also write null so a half-filled add never
/// persists a bogus "" (validation surfaces the requirement inline). Numbers
/// that fail to parse are left at whatever the field currently is (we simply
/// don't write), so a transient "1." mid-typing never corrupts the JSON.
pub fn apply_string_value(config: &mut serde_json::Value, spec: &FieldSpec, raw: &str) {
    if !config.is_object() {
        *config = serde_json::json!({});
    }
    let Some(obj) = config.as_object_mut() else {
        return;
    };
    let trimmed = raw.trim();
    match &spec.field_type {
        FieldType::Number { integer } => {
            if trimmed.is_empty() {
                // An OPTIONAL number (FieldDefault::Num) re-applies its default on
                // clear so the engine never sees a missing value. A REQUIRED number
                // has FieldDefault::None: clearing it must REMOVE the key (never
                // write a sentinel 0), so the field starts/stays empty, the inline
                // "<label> is required" surfaces, and a half-filled add can't
                // silently persist a broken source (e.g. tempest_ws station_id 0).
                match spec.default {
                    FieldDefault::Num(n) => {
                        obj.insert(spec.key.to_string(), num_to_json(n, *integer));
                    }
                    _ => {
                        obj.remove(spec.key);
                    }
                }
                return;
            }
            if *integer {
                if let Ok(v) = trimmed.parse::<i64>() {
                    obj.insert(spec.key.to_string(), serde_json::json!(v));
                }
            } else if let Ok(v) = trimmed.parse::<f64>() {
                if let Some(num) = serde_json::Number::from_f64(v) {
                    obj.insert(spec.key.to_string(), serde_json::Value::Number(num));
                }
            }
        }
        // Text / password / select all persist as strings. Empty -> null for
        // optional Option<String> fields; for fields with a non-empty string
        // default we still write null on clear (the engine applies the serde
        // default), which keeps the JSON minimal and re-seeds to the default.
        _ => {
            if trimmed.is_empty() {
                obj.insert(spec.key.to_string(), serde_json::Value::Null);
            } else {
                obj.insert(
                    spec.key.to_string(),
                    serde_json::Value::String(trimmed.to_string()),
                );
            }
        }
    }
}

/// Write a bool value into the config object at `spec.key`.
pub fn apply_bool_value(config: &mut serde_json::Value, spec: &FieldSpec, val: bool) {
    if !config.is_object() {
        *config = serde_json::json!({});
    }
    if let Some(obj) = config.as_object_mut() {
        obj.insert(spec.key.to_string(), serde_json::Value::Bool(val));
    }
}

fn num_to_json(n: f64, integer: bool) -> serde_json::Value {
    if integer {
        serde_json::json!(n as i64)
    } else {
        serde_json::Number::from_f64(n)
            .map(serde_json::Value::Number)
            .unwrap_or(serde_json::Value::Null)
    }
}

// ===================================================================
// Component: generic per-kind labeled config form
// ===================================================================

/// LABELED BASE-CONFIG FORM. For the selected `kind`, renders one labeled input
/// per `source_fields(kind)` entry (text / number / masked password / checkbox /
/// select), seeded from the shared `config_text` JSON and flushing each change
/// back into `config_text` with the SAME self_edit guard-flag two-way sync as
/// soil_forms.rs. The JSON-advanced textarea stays authoritative for nested keys
/// (subscriptions, soil_calibration, field_map) and anything not in the schema;
/// editing it re-seeds these inputs. Required fields show an inline error when
/// empty. Renders nothing for a kind with no scalar schema.
#[component]
pub fn SourceConfigForm(
    /// The source's raw config JSON text, shared with the raw editor + soil forms.
    config_text: RwSignal<String>,
    /// The selected source kind (reactive); the field set swaps when it changes.
    #[prop(into)]
    kind: Signal<String>,
) -> impl IntoView {
    // Local parsed-config mirror, seeded from config_text and flushed back on
    // every edit. Kept as a serde_json::Value so we preserve nested keys
    // (subscriptions etc.) the bespoke forms own.
    let cfg = RwSignal::new(
        serde_json::from_str::<serde_json::Value>(&config_text.get_untracked())
            .unwrap_or(serde_json::Value::Null),
    );

    // Guard flag (see soil_forms.rs for the full rationale). flush() can write a
    // config_text that differs from a literal re-parse (it normalizes null/empty
    // and re-applies number defaults), so a value-based guard would loop. This
    // flag lets the re-seed Effect skip config_text changes WE caused while
    // still re-seeding on EXTERNAL ones (the advanced textarea, kind swap).
    let self_edit = RwSignal::new(false);

    // Flush this form's scalar keys -> config_text. Mirrors the proven
    // soil_forms.rs flush() pattern: RE-READ config_text fresh (so we apply onto
    // the live JSON, never a stale-able local mirror that can lag Leptos effect
    // timing), then write back ONLY the keys this kind's schema owns
    // (source_fields(kind)). Every other key (the nested subscriptions /
    // soil_calibration / field_map / per-device maps the bespoke forms and the
    // advanced textarea own) is structurally preserved because we only touch our
    // own keys on the freshly-parsed value.
    let flush = move || {
        let mut fresh: serde_json::Value =
            serde_json::from_str(&config_text.get_untracked()).unwrap_or(serde_json::json!({}));
        let mine = cfg.get_untracked();
        for spec in source_fields(&kind.get_untracked()) {
            match mine.get(spec.key) {
                Some(v) => {
                    if let Some(obj) = fresh.as_object_mut() {
                        obj.insert(spec.key.to_string(), v.clone());
                    } else {
                        fresh = serde_json::json!({ spec.key: v.clone() });
                    }
                }
                // Absent in our mirror (a required number cleared to empty was
                // REMOVED by apply_string_value): mirror that removal on the
                // fresh value so the key truly drops, not just goes stale.
                None => {
                    if let Some(obj) = fresh.as_object_mut() {
                        obj.remove(spec.key);
                    }
                }
            }
        }
        self_edit.set(true);
        config_text.set(serde_json::to_string_pretty(&fresh).unwrap_or_else(|_| "{}".into()));
    };

    // Two-way sync: re-seed `cfg` when config_text changes from OUTSIDE this
    // form (the advanced textarea, or the kind-swap template reset in the
    // parent). Track config_text first to stay subscribed, then bail on our own
    // writes via the guard.
    Effect::new(move |_| {
        let text = config_text.get();
        if self_edit.get_untracked() {
            self_edit.set(false);
            return;
        }
        let parsed =
            serde_json::from_str::<serde_json::Value>(&text).unwrap_or(serde_json::Value::Null);
        if parsed != cfg.get_untracked() {
            cfg.set(parsed);
        }
    });

    view! {
        <div class="source-config-form">
            {move || {
                let specs = source_fields(&kind.get());
                specs
                    .iter()
                    .map(|spec| field_row(spec, cfg, flush))
                    .collect_view()
            }}
        </div>
    }
}

/// Render one FieldSpec as a labeled, two-way-bound input. A free function (not
/// inline view! nesting) so each row monomorphizes in its own boundary, keeping
/// the recursion depth flat per the no-deep-nesting guidance.
pub(crate) fn field_row(
    spec: &FieldSpec,
    cfg: RwSignal<serde_json::Value>,
    // Send + Sync so the Password arm can wrap `flush` in a leptos Callback for
    // SecretInput (Callback::new requires Send + Sync). `flush` only captures
    // RwSignals, which already satisfy both.
    flush: impl Fn() + Copy + 'static + Send + Sync,
) -> impl IntoView {
    // Clone the spec into 'static owned bits the closures + reactive reads need
    // (FieldSpec is all &'static, so this is cheap copies of fat pointers).
    let spec = spec.clone();
    let key = spec.key;
    let label = spec.label;
    let helptext = spec.helptext;
    let placeholder = spec.placeholder;
    let required = spec.required;
    let field_type = spec.field_type.clone();
    let display_unit = spec.display_unit;

    // Per-device unit prefs (reactive; updates after the post-hydration
    // localStorage read). Created ONLY for display_unit fields so plain fields
    // (the vast majority) don't each spin up a redundant hydrate effect; the
    // default imperial prefs are used as a static stand-in otherwise so the
    // label/seed closures stay uniform.
    let prefs: Signal<UnitPrefs> = if display_unit.is_some() {
        use_unit_prefs()
    } else {
        Signal::derive(UnitPrefs::default)
    };

    // Reactive label. Plain fields show the static label; a display_unit field
    // appends the ACTIVE unit suffix ("Radius (mi)" / "Radius (km)") so the
    // label tracks the preference while the stored value stays imperial.
    let label_text = Signal::derive(move || match display_unit {
        Some(du) => format!("{label} ({})", du.unit_label(prefs.get())),
        None => label.to_string(),
    });

    // Inline required-validation: empty value on a required field shows an error.
    let spec_for_err = spec.clone();
    let error = Signal::derive(move || {
        if !required {
            return None;
        }
        let v = field_string_value(&cfg.get(), &spec_for_err);
        v.trim()
            .is_empty()
            .then(|| format!("{} is required", label_text.get()))
    });

    let input: AnyView = match field_type {
        FieldType::Bool => {
            let spec_for_seed = spec.clone();
            let spec_for_set = spec.clone();
            view! {
                <label style="display: flex; gap: 0.5rem; align-items: center; min-height: 44px;">
                    <input
                        type="checkbox"
                        prop:checked=move || field_bool_value(&cfg.get(), &spec_for_seed)
                        on:input=move |ev| {
                            let v = event_target_checked(&ev);
                            cfg.update(|c| apply_bool_value(c, &spec_for_set, v));
                            flush();
                        }
                    />
                    {label}
                </label>
            }
            .into_any()
        }
        FieldType::Select(options) => {
            let spec_for_seed = spec.clone();
            let spec_for_set = spec.clone();
            view! {
                <select
                    class="ui-input"
                    on:change=move |ev| {
                        let v = event_target_value(&ev);
                        cfg.update(|c| apply_string_value(c, &spec_for_set, &v));
                        flush();
                    }
                >
                    {move || {
                        let current = field_string_value(&cfg.get(), &spec_for_seed);
                        let mut opts: Vec<_> = options
                            .iter()
                            .map(|(val, lbl)| {
                                let val = val.to_string();
                                let sel = current == val;
                                view! { <option value=val.clone() selected=sel>{lbl.to_string()}</option> }
                            })
                            .collect();
                        // A stored value set in the JSON-advanced box that is NOT
                        // one of the spec's options would otherwise show option 0
                        // selected, silently misrepresenting the real config (and
                        // clobbering it on the next change). Mirror the
                        // soil-subscription zone select: append a synthetic
                        // selected option that reflects the actual stored value.
                        if !current.is_empty()
                            && !options.iter().any(|(v, _)| *v == current)
                        {
                            let label = format!("Custom (set in JSON): {current}");
                            opts.push(
                                view! { <option value=current.clone() selected=true>{label}</option> },
                            );
                        }
                        opts.into_iter().collect_view()
                    }}
                </select>
            }
            .into_any()
        }
        FieldType::Password => {
            let spec_for_seed = spec.clone();
            let spec_for_set = spec.clone();
            let value = Signal::derive(move || field_string_value(&cfg.get(), &spec_for_seed));
            let on_input = Callback::new(move |v: String| {
                cfg.update(|c| apply_string_value(c, &spec_for_set, &v));
                flush();
            });
            view! {
                <SecretInput value=value on_input=on_input placeholder=placeholder/>
            }
            .into_any()
        }
        FieldType::Number { .. } => {
            let spec_for_seed = spec.clone();
            let spec_for_set = spec.clone();
            // Seed: read the stored (imperial) source value, then for a
            // display_unit field show it in the active unit; plain numbers seed
            // the raw stored string unchanged.
            let seed = move || {
                let raw = field_string_value(&cfg.get(), &spec_for_seed);
                match display_unit {
                    Some(du) => match raw.trim().parse::<f64>() {
                        Ok(src) => du.value_to_display(src, prefs.get()),
                        // Empty / unparsable (e.g. mid-typing in JSON-advanced):
                        // pass through so the input isn't blanked.
                        Err(_) => raw,
                    },
                    None => raw,
                }
            };
            view! {
                <input
                    type="number"
                    class="ui-input"
                    step="any"
                    placeholder=placeholder
                    prop:value=seed
                    on:input=move |ev| {
                        let v = event_target_value(&ev);
                        // For a display_unit field the user types the DISPLAY
                        // unit; convert back to the stored imperial source value
                        // before persisting so the wire format stays imperial.
                        // Empty/unparsable strings pass through to apply_string_value
                        // unchanged (it handles empty -> default/remove).
                        match display_unit {
                            Some(du) => match v.trim().parse::<f64>() {
                                Ok(disp) => {
                                    let src = du.display_to_source(disp, prefs.get());
                                    cfg.update(|c| apply_string_value(c, &spec_for_set, &src.to_string()));
                                }
                                Err(_) => {
                                    cfg.update(|c| apply_string_value(c, &spec_for_set, &v));
                                }
                            },
                            None => {
                                cfg.update(|c| apply_string_value(c, &spec_for_set, &v));
                            }
                        }
                        flush();
                    }
                />
            }
            .into_any()
        }
        FieldType::Text => {
            let spec_for_seed = spec.clone();
            let spec_for_set = spec.clone();
            view! {
                <input
                    type="text"
                    class="ui-input"
                    placeholder=placeholder
                    prop:value=move || field_string_value(&cfg.get(), &spec_for_seed)
                    on:input=move |ev| {
                        let v = event_target_value(&ev);
                        cfg.update(|c| apply_string_value(c, &spec_for_set, &v));
                        flush();
                    }
                />
            }
            .into_any()
        }
    };

    // Bool renders its own inline label inside the checkbox <label>; a
    // display_unit field needs a REACTIVE label (the unit suffix tracks the
    // pref signal) which FormField's plain-String label can't do. Both pass an
    // empty FormField label and render their own; everything else uses the
    // static label.
    let renders_own_label = matches!(spec.field_type, FieldType::Bool) || display_unit.is_some();
    let ff_label = if renders_own_label {
        String::new()
    } else {
        label.to_string()
    };
    let _ = key;

    // Reactive label rendered inside the slot ONLY for display_unit fields (Bool
    // already has its inline label in the checkbox). Mirrors the
    // ui-form-field__label class so it matches the static-label fields visually.
    let own_label = (display_unit.is_some()).then(move || {
        view! {
            <label class="ui-form-field__label">{move || label_text.get()}</label>
        }
    });

    // "Where to get this ->" link for credential/account fields, rendered under
    // the input so a newcomer is never stranded wondering where the key/token
    // comes from. The href is an external provider URL (full https), so it opens
    // in a new tab with the usual noopener hardening. Absent for self-explanatory
    // fields (host/port/path/...).
    let doc_link = spec.doc_url.map(|href| {
        view! {
            <a
                class="source-field-doc"
                href=href
                target="_blank"
                rel="noopener noreferrer"
            >
                "Where to get this \u{2192}"
            </a>
        }
    });

    view! {
        <FormField label=ff_label helptext=helptext.to_string() error=error>
            {own_label}
            {input}
            {doc_link}
        </FormField>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::components::sources_form::kind_options;

    const IMPERIAL: UnitPrefs = UnitPrefs {
        temp_c: false,
        rain_mm: false,
        wind_metric: false,
        pressure_metric: false,
        distance_metric: false,
        area_metric: false,
    };
    const METRIC: UnitPrefs = UnitPrefs {
        distance_metric: true,
        ..IMPERIAL
    };

    #[test]
    fn blitzortung_radius_carries_a_distance_display_unit() {
        // The radius field's stored key/value stay imperial (radius_mi); only
        // the DISPLAY is unit-aware. The marker is what drives that.
        let spec = source_fields("blitzortung")
            .iter()
            .find(|s| s.key == "radius_mi")
            .unwrap();
        assert_eq!(spec.display_unit, Some(DisplayUnit::DistanceMi));
        // Default stays in the stored (miles) unit, untouched by display.
        assert_eq!(spec.default, FieldDefault::Num(100.0));
    }

    #[test]
    fn display_unit_label_and_value_track_prefs() {
        let du = DisplayUnit::DistanceMi;
        // Label suffix flips with the pref.
        assert_eq!(du.unit_label(IMPERIAL), "mi");
        assert_eq!(du.unit_label(METRIC), "km");
        // Stored miles render in the active display unit.
        assert_eq!(du.value_to_display(100.0, IMPERIAL), "100.0");
        assert_eq!(du.value_to_display(100.0, METRIC), "160.9");
    }

    #[test]
    fn display_to_source_keeps_the_wire_imperial() {
        let du = DisplayUnit::DistanceMi;
        // Imperial: identity (typed miles persist as miles).
        assert!((du.display_to_source(100.0, IMPERIAL) - 100.0).abs() < 1e-9);
        // Metric: typed km convert back to the stored miles wire value.
        assert!((du.display_to_source(160.9344, METRIC) - 100.0).abs() < 1e-6);
    }

    #[test]
    fn no_field_marks_a_non_numeric_kind_with_a_display_unit() {
        // A display_unit only makes sense on a numeric field; assert the marker
        // never lands on a text/select/bool/password spec.
        for (kind, _label) in kind_options() {
            for spec in source_fields(&kind) {
                if spec.display_unit.is_some() {
                    assert!(
                        matches!(spec.field_type, FieldType::Number { .. }),
                        "kind `{kind}` field `{}` has a display_unit but is not numeric",
                        spec.key
                    );
                }
            }
        }
    }

    #[test]
    fn every_source_kind_has_a_field_schema_entry() {
        // The wizard + settings offer exactly these kinds. Each MUST have a
        // source_fields entry (possibly empty by deliberate design) so no kind
        // is silently form-less. A non-empty schema is required for every kind
        // that has scalar base config; today that is every shipped kind.
        for (kind, label) in kind_options() {
            let specs = source_fields(&kind);
            assert!(
                !specs.is_empty(),
                "source kind `{kind}` ({label}) has no labeled base-config fields; \
                 add a source_fields arm or document why it is JSON-only"
            );
        }
    }

    #[test]
    fn coverage_kind_list_matches_default_config_text() {
        // Sanity: every kind the form offers also has a default_config_text
        // template (so the kind-swap seed has something to show), and vice
        // versa nothing in kind_options is a typo'd kind missing a template.
        use crate::components::sources_form::default_config_text;
        for (kind, _label) in kind_options() {
            let t = default_config_text(&kind);
            assert!(
                t.starts_with('{'),
                "default_config_text(`{kind}`) is not a JSON object template"
            );
        }
    }

    // Engine-struct round-trip: references crate::config::schema, which is
    // cfg(feature = "ssr"). Gated so bare `cargo test` (no features) still
    // compiles; the pure coverage/string tests above stay ungated.
    #[cfg(feature = "ssr")]
    #[test]
    fn every_kind_string_deserializes_into_source_kind() {
        // The kind string the UI saves MUST be a tag the engine's SourceKind
        // accepts on deserialize, or /api/config PUT 422s on the new source.
        // (This caught OpenWeather: serde snake_case is `open_weather` but the
        // UI emits `openweather`; the variant now carries a serde alias.) We
        // build a minimal config per kind from default_config_text, then fill
        // any REQUIRED-but-template-absent scalar (a required field intentionally
        // seeded blank so the form flags it, e.g. tempest_ws station_id), then
        // assert the {kind, config} pair parses. This validates the kind->tag
        // mapping, not the template's completeness.
        use crate::components::sources_form::default_config_text;
        use crate::config::schema::SourceKind;
        for (kind, label) in kind_options() {
            let mut config: serde_json::Value =
                serde_json::from_str(&default_config_text(&kind)).unwrap_or(serde_json::json!({}));
            // tempest_ws.station_id is a required u32 with no serde default,
            // deliberately omitted from the template so the form starts empty +
            // flags it. Supply a value so this tag-mapping check still passes.
            if kind == "tempest_ws" {
                if let Some(obj) = config.as_object_mut() {
                    obj.insert("station_id".into(), serde_json::json!(12345));
                }
            }
            let tagged = serde_json::json!({ "kind": kind, "config": config });
            let parsed: Result<SourceKind, _> = serde_json::from_value(tagged);
            assert!(
                parsed.is_ok(),
                "UI kind string `{kind}` ({label}) does not deserialize into SourceKind: {:?}",
                parsed.err()
            );
        }
    }

    #[test]
    fn keyed_sources_carry_a_setup_doc_link() {
        // The credential/account sources that strand a newcomer must each expose
        // a "Where to get this ->" link on one of their fields, pointing at the
        // provider page. Guards against a future field rename dropping the link.
        let want_doc = [
            "openweather",
            "pirate_weather",
            "weatherkit",
            "tempest_ws",
            "netatmo",
            "ambient_weather",
            "tuya_cloud",
            "yolink",
        ];
        for kind in want_doc {
            let has_link = source_fields(kind).iter().any(|s| s.doc_url.is_some());
            assert!(
                has_link,
                "kind `{kind}` should carry a setup doc link on a credential field"
            );
        }
    }

    #[test]
    fn doc_links_are_absolute_provider_urls() {
        // doc_url is an EXTERNAL provider page (full https), unlike the in-app
        // doc_url() slug links: it must be absolute so target=_blank resolves.
        for (kind, _label) in kind_options() {
            for spec in source_fields(&kind) {
                if let Some(href) = spec.doc_url {
                    assert!(
                        href.starts_with("https://"),
                        "kind `{kind}` field `{}` doc_url `{href}` must be an absolute https URL",
                        spec.key
                    );
                }
            }
        }
    }

    #[test]
    fn field_keys_are_unique_per_kind() {
        for (kind, _label) in kind_options() {
            let specs = source_fields(&kind);
            let mut keys: Vec<&str> = specs.iter().map(|s| s.key).collect();
            keys.sort_unstable();
            let before = keys.len();
            keys.dedup();
            assert_eq!(before, keys.len(), "duplicate field key in kind `{kind}`");
        }
    }

    // ---- Round-trip: form value lands at the exact engine JSON key ----

    fn set(config: &mut serde_json::Value, kind: &str, key: &str, raw: &str) {
        let spec = source_fields(kind)
            .iter()
            .find(|s| s.key == key)
            .unwrap_or_else(|| panic!("no field `{key}` for kind `{kind}`"));
        apply_string_value(config, spec, raw);
    }

    #[cfg(feature = "ssr")]
    #[test]
    fn ha_passthrough_round_trip_base_url_and_token() {
        let mut cfg = serde_json::json!({ "field_map": { "air_temp_f": "sensor.outdoor" } });
        set(
            &mut cfg,
            "ha_passthrough",
            "base_url",
            "http://ha.local:8123",
        );
        set(&mut cfg, "ha_passthrough", "bearer_token", "tok123");
        // Keys are byte-identical to HaPassthroughConfig serde fields.
        assert_eq!(cfg["base_url"], "http://ha.local:8123");
        assert_eq!(cfg["bearer_token"], "tok123");
        // The bespoke-form key (field_map) is untouched.
        assert_eq!(cfg["field_map"]["air_temp_f"], "sensor.outdoor");
        // It deserializes into the real engine struct.
        let parsed: crate::config::schema::HaPassthroughConfig =
            serde_json::from_value(cfg).unwrap();
        assert_eq!(parsed.base_url, "http://ha.local:8123");
        assert_eq!(parsed.bearer_token, "tok123");
    }

    #[cfg(feature = "ssr")]
    #[test]
    fn mqtt_round_trip_broker_host_and_port() {
        let mut cfg = serde_json::json!({ "subscriptions": [] });
        set(&mut cfg, "mqtt", "broker_host", "broker.local");
        set(&mut cfg, "mqtt", "broker_port", "8883");
        assert_eq!(cfg["broker_host"], "broker.local");
        assert_eq!(cfg["broker_port"], 8883);
        // broker_port must be a JSON integer (u16), not a float/string.
        assert!(cfg["broker_port"].is_i64() || cfg["broker_port"].is_u64());
        let parsed: crate::config::schema::MqttSourceConfig = serde_json::from_value(cfg).unwrap();
        assert_eq!(parsed.broker_host, "broker.local");
        assert_eq!(parsed.broker_port, 8883);
    }

    #[cfg(feature = "ssr")]
    #[test]
    fn openweather_round_trip_api_key() {
        let mut cfg = serde_json::json!({});
        set(&mut cfg, "openweather", "api_key", "OWMKEY");
        assert_eq!(cfg["api_key"], "OWMKEY");
        let parsed: crate::config::schema::OpenWeatherConfig = serde_json::from_value(cfg).unwrap();
        assert_eq!(parsed.api_key, "OWMKEY");
    }

    #[cfg(feature = "ssr")]
    #[test]
    fn openweather_serializes_to_the_ui_kind_string() {
        // A persisted OpenWeather source must serialize back to `openweather`
        // (not the auto snake_case `open_weather`), or the labeled Connection
        // form, kind icon, and segmented picker all miss it on reload.
        let k: crate::config::schema::SourceKind = serde_json::from_value(
            serde_json::json!({ "kind": "openweather", "config": { "api_key": "x" } }),
        )
        .unwrap();
        let v = serde_json::to_value(&k).unwrap();
        assert_eq!(v["kind"], "openweather");
        // And the old snake_case tag still deserializes (back-compat alias).
        let back: crate::config::schema::SourceKind = serde_json::from_value(
            serde_json::json!({ "kind": "open_weather", "config": { "api_key": "x" } }),
        )
        .unwrap();
        assert!(matches!(
            back,
            crate::config::schema::SourceKind::OpenWeather(_)
        ));
    }

    #[cfg(feature = "ssr")]
    #[test]
    fn ecowitt_gw_poll_round_trip_host_and_interval() {
        let mut cfg = serde_json::json!({ "soil_calibration": {} });
        set(&mut cfg, "ecowitt_gw_poll", "host", "192.0.2.50");
        set(&mut cfg, "ecowitt_gw_poll", "poll_interval_s", "45");
        assert_eq!(cfg["host"], "192.0.2.50");
        assert_eq!(cfg["poll_interval_s"], 45);
        let parsed: crate::config::schema::EcowittGwPollConfig =
            serde_json::from_value(cfg).unwrap();
        assert_eq!(parsed.host, "192.0.2.50");
        assert_eq!(parsed.poll_interval_s, 45);
    }

    #[cfg(feature = "ssr")]
    #[test]
    fn empty_optional_text_serializes_to_null() {
        // Clearing an optional text/password field writes JSON null (the
        // Option<String> = None shape), not an empty string.
        let mut cfg = serde_json::json!({});
        set(&mut cfg, "tempest_udp", "hub_serial", "");
        assert!(cfg["hub_serial"].is_null());
        // And it deserializes to None.
        let parsed: crate::config::schema::TempestUdpConfig = serde_json::from_value(cfg).unwrap();
        assert_eq!(parsed.hub_serial, None);
    }

    #[test]
    fn cleared_number_reapplies_default() {
        // Clearing an OPTIONAL number (one with a spec default) re-applies the
        // default so the engine never sees a missing value.
        let mut cfg = serde_json::json!({ "broker_host": "b" });
        set(&mut cfg, "mqtt", "broker_port", "");
        assert_eq!(cfg["broker_port"], 1883);
    }

    #[test]
    fn cleared_required_number_omits_key() {
        // A REQUIRED number (FieldDefault::None, e.g. tempest_ws station_id) must
        // be OMITTED when empty, never written as a sentinel 0. So an empty
        // station_id can't silently persist a broken Tempest cloud source; the
        // inline "Station ID is required" surfaces and /api/config PUT 422s a
        // truly missing required field rather than accepting station 0.
        let mut cfg = serde_json::json!({ "station_id": 7, "access_token": "t" });
        set(&mut cfg, "tempest_ws", "station_id", "");
        assert!(
            cfg.get("station_id").is_none(),
            "cleared required number must remove the key, got {:?}",
            cfg.get("station_id")
        );
        // access_token (the other key) is untouched.
        assert_eq!(cfg["access_token"], "t");
        // And the spec carries no sentinel default.
        let spec = source_fields("tempest_ws")
            .iter()
            .find(|s| s.key == "station_id")
            .unwrap();
        assert!(spec.required);
        assert_eq!(spec.default, FieldDefault::None);
    }

    #[test]
    fn required_number_seeds_empty_not_zero() {
        // A fresh config seeds the required station_id input as "" (so the
        // placeholder shows + the required error flags), NOT "0".
        let cfg = serde_json::json!({});
        let spec = source_fields("tempest_ws")
            .iter()
            .find(|s| s.key == "station_id")
            .unwrap();
        assert_eq!(field_string_value(&cfg, spec), "");
    }

    #[cfg(feature = "ssr")]
    #[test]
    fn bool_round_trip_blitzortung_enabled() {
        let spec = source_fields("blitzortung")
            .iter()
            .find(|s| s.key == "enabled")
            .unwrap();
        let mut cfg = serde_json::json!({ "radius_mi": 100.0 });
        apply_bool_value(&mut cfg, spec, true);
        assert_eq!(cfg["enabled"], true);
        let parsed: crate::config::schema::BlitzortungConfig = serde_json::from_value(cfg).unwrap();
        assert!(parsed.enabled);
    }

    #[test]
    fn seed_value_falls_back_to_default_when_absent() {
        // A fresh (empty) config seeds inputs from the spec defaults so the form
        // shows the right starting values before the user types.
        let cfg = serde_json::json!({});
        let host_spec = source_fields("ecowitt_gw_poll")
            .iter()
            .find(|s| s.key == "poll_interval_s")
            .unwrap();
        assert_eq!(field_string_value(&cfg, host_spec), "30");
        let tempest = source_fields("tempest_udp")
            .iter()
            .find(|s| s.key == "bind_addr")
            .unwrap();
        assert_eq!(field_string_value(&cfg, tempest), "0.0.0.0:50222");
    }

    #[test]
    fn integer_number_renders_without_trailing_decimal() {
        // Defaults like 1883 must seed as "1883", never "1883.0".
        let cfg = serde_json::json!({});
        let port = source_fields("mqtt")
            .iter()
            .find(|s| s.key == "broker_port")
            .unwrap();
        assert_eq!(field_string_value(&cfg, port), "1883");
    }

    #[test]
    fn select_default_is_a_real_option() {
        // Every Select field's default must be one of its options, or the
        // dropdown shows a phantom blank on first render.
        for (kind, _label) in kind_options() {
            for spec in source_fields(&kind) {
                if let FieldType::Select(opts) = &spec.field_type {
                    if let FieldDefault::Str(d) = spec.default {
                        assert!(
                            opts.iter().any(|(v, _)| *v == d),
                            "kind `{kind}` field `{}` default `{d}` is not an option",
                            spec.key
                        );
                    }
                }
            }
        }
    }
}
