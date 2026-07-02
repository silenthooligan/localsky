// CloudWeatherServices, the cloud-weather experience for EVERY customer (with
// hardware or without).
//
// GUIDING PRINCIPLE: the customer turns ON weather feeds for their address;
// they never configure a merge engine. The whole point is HONESTY so they can
// judge the watering risk. Every fact on a row is read VERBATIM from the
// server's GET /api/config/source_catalog (handler in src/api/config.rs, backed
// by src/sources/cloud_catalog.rs); this client never duplicates the honesty
// copy. It only renders the catalog + wires the enable PUTs.
//
// THE ONE HONEST RAIN SIGNAL. The single loudest element on a row is ONE rain
// badge derived from `rain_nature` (NOT the overall `data_nature`): green
// "Measures rain" for an Observation, green/teal "Radar-measured rain" for a
// RadarQpe, blue "Nowcast" for a Nowcast, amber "Forecast only" for a Forecast.
// Always glyph + word + color (the a11y rule: color is never the only signal).
// This is the live-rain honesty fix at the UI: Pirate and every model source
// read amber "Forecast only" automatically (their rain is a model output), and
// only NWS + NOAA MRMS read green. We NEVER print the word "live" on a Forecast.
//
// LAYOUT: a SCANNABLE LIST, not a wall of dense cards. Each provider is ONE
// compact ROW that, by DEFAULT, is collapsed and carries exactly: a left
// entity-stripe, a chevron, the friendly name, the ONE rain badge, the key
// chip, an optional "On by default here" marker, and the enable control.
// Everything else (the capability summary, the real-time + localization +
// watering-risk lines, the key caution, the Met.no synthetic-POP note, and the
// keyed key form) lives in the expand, revealed only on a chevron / row /
// "Add key" click. Progressive disclosure: calm by default, the full honest
// picture one click away.
//
// LEAD WITH THE ANSWER. Above the list a single live line names the source that
// owns the rain reading right now + its honest nature: "Rain on your yard right
// now: <owner> (<Measured|Radar|Forecast>) - updated <Nm> ago". The section
// summary counts by rain_nature: "N measure rain - M forecast it".
//
// REGION CLUTTER. The list splits into RECOMMENDED (region-recommended keyless
// authorities, plus Pirate when it carries an upgrade) and a collapsed
// <details> "More providers (N)" holding the paid + region-inappropriate
// options. A US user sees Open-Meteo, NWS, NOAA MRMS, and Pirate without opening
// the fold; Met.no stays tucked away until they ask for it.
//
// ENABLE has two shapes:
//   * Keyless (open_meteo, nws, met_norway, noaa_mrms): a single Toggle on the
//     row. ON splices {id:kind, kind, enabled:true} into config.sources and PUTs
//     /api/config; the server's normalize_new_cloud_sources stamps region
//     priority + max_age (we NEVER compute priority client-side). OFF flips
//     enabled:false but keeps the entry.
//   * Keyed (pirate_weather, openweather, weatherkit): an "Add key" affordance
//     on the row that expands the details to a SecretInput + the provider
//     get-a-key link and one primary that writes key + enabled:true in a single
//     PUT. WeatherKit (four pieces) routes to the full SourceEditorPanel
//     instead. We NEVER synthesize an enabled keyed entry with a placeholder key
//     (it would 401 and trip the degraded banner).

use leptos::prelude::*;
use leptos::tachys::view::any_view::IntoAny;
use serde::Deserialize;

use crate::components::settings::RestartBanner;
use crate::components::ui::{Button, Icon, SecretInput, Toggle};

/// One cloud weather kind as GET /api/config/source_catalog returns it: the
/// flattened honesty facts (CloudSourceMeta) plus the live per-deployment
/// wiring. Field names + the snake_case enum strings match the server's
/// `CloudCatalogEntry` (api/config.rs) + `CloudSourceMeta` (cloud_catalog.rs).
#[derive(Clone, Debug, Deserialize)]
pub struct CloudEntry {
    /// Stable kind tag (open_meteo / nws / noaa_mrms / openweather / ...). The
    /// key the enable PUT uses for {id:kind, kind} and the catalog joins on.
    pub kind: String,
    /// "observation" | "radar_qpe" | "nowcast" | "forecast", the OVERALL headline
    /// honesty axis for the whole feed (temp/wind/humidity, not just rain).
    pub data_nature: String,
    /// The HONEST nature of the CURRENT-RAIN signal specifically, which can
    /// differ from `data_nature`. THE ONE RAIN BADGE keys on this, never on
    /// `data_nature`: it is the mislabel fix for Pirate (whose rain is a model
    /// forecast even though its overall headline is a nowcast). Values:
    /// "observation" | "radar_qpe" | "nowcast" | "forecast".
    #[serde(default = "default_forecast_nature")]
    pub rain_nature: String,
    /// How live + how laggy "current" really is (verbatim catalog copy).
    pub real_time: String,
    /// How close to the user's yard the value resolves (verbatim).
    pub localization: String,
    /// The one honest watering-decision risk line (verbatim, rendered italic).
    pub watering_risk: String,
    /// "no_key" | "free_key" | "paid", the cost/friction axis.
    pub key_tier: String,
    /// True when the adapter emits a CURRENT rain scalar (every kind but Met.no).
    pub emits_current_rain: bool,
    /// True ONLY for Met.no: its rain probability is synthesized, not measured.
    pub pop_is_synthetic: bool,
    /// Honesty ranking, highest first; the catalog is already sorted by it.
    pub honesty_rank: i32,
    /// Best-rain-DECISION ranking for irrigation, highest first (a gauge on the
    /// yard ranks highest elsewhere ~100, then NOAA MRMS, NWS, Pirate, and so
    /// on). Drives the backup-chain ordering for the rain reading specifically.
    #[serde(default)]
    pub irrigation_rank: i32,
    /// The honest "why you might still want this" upgrade note, `Some` only for
    /// Pirate in CONUS (its free key sharpens temp/wind even though its rain is a
    /// forecast). Shown verbatim in RECOMMENDED, never auto-enabling the source.
    #[serde(default)]
    pub upgrade_reason: Option<String>,
    /// True when a one-click upgrade is offered (mirrors `upgrade_reason` being
    /// Some). The UI promotes the option without flipping it on.
    #[serde(default)]
    pub upgrade_available: bool,
    /// Canonical WeatherField names (snake_case) this kind emits as live current
    /// scalars (the capability matrix lights a column from these).
    #[serde(default)]
    pub live_current_fields: Vec<String>,
    /// The HONEST per-field data nature for EACH field this kind emits, as
    /// `(canonical_field_key, nature_string)` pairs over EXACTLY
    /// `live_current_fields` (same keys, same order). The CAPABILITY MATRIX reads
    /// this to TINT each lit cell by its own truth, the per-cell refinement the
    /// single overall `data_nature` cannot express: Pirate's `wind_mph` carries
    /// "nowcast" while its rain keys carry "forecast" in the SAME row. The nature
    /// strings are the SAME snake_case `CloudDataNature` wire values
    /// ("observation" / "radar_qpe" / "nowcast" / "forecast") the row already
    /// matches `data_nature` / `rain_nature` on. Serializes from the server's
    /// `field_natures` array of `[key, nature]` 2-tuples. Absent (older payload) =
    /// empty, so the matrix simply leaves cells untinted-but-lit, never panics.
    #[serde(default)]
    pub field_natures: Vec<(String, String)>,
    /// True when this kind is the region default here (the server derives this
    /// from `region::is_region_keyless_authority`: Open-Meteo always, NWS +
    /// NOAA MRMS in the US, Met.no in Europe/the Nordics). The "On by default
    /// here" marker AND the RECOMMENDED-section membership key on this.
    #[serde(default)]
    pub recommended_here: bool,
    /// True when this kind is region-APPROPRIATE at the deployment location (the
    /// softer collapse signal). False today only for Met.no outside Europe/the
    /// Nordics; true for every other kind everywhere. A region-INappropriate kind
    /// is tucked into the "More providers" fold. Defaults true so an older
    /// payload (or any kind the server omits the flag for) never gets hidden.
    #[serde(default = "default_true")]
    pub region_appropriate: bool,
    /// The region merge priority the server would seed; informational only (the
    /// UI never sets priority client-side).
    #[serde(default)]
    pub region_priority: i32,
    /// True when a source of this kind is already present AND enabled in config.
    #[serde(default)]
    pub already_configured: bool,
    /// True when a source of this kind exists in config REGARDLESS of enabled.
    /// The device-card list owns every configured source (on or off), so the
    /// "add coverage" discovery list filters on this to never double-show a
    /// disabled cloud source in both the device list and discovery.
    #[serde(default)]
    pub configured_present: bool,
    /// The honest 5-state source-status taxonomy for this kind right now, computed
    /// SERVER-SIDE by the shared `api::health::compute_source_status` fn that also
    /// drives /api/health, so the row word and the health rollup read ONE source of
    /// truth. One of "active" | "watching" | "standby" | "falling_through" |
    /// "offline" (the `SourceStatus::as_str` wire strings). The row maps this enum
    /// (plus which field this kind owns in `field_sources`, plus the enabled/key/
    /// region flags) to the single homeowner status WORD. Absent (older payload)
    /// reads the calm "watching" default, never a false fault.
    #[serde(default = "default_watching_status")]
    pub status: String,
}

/// serde default for `status`: an absent value reads as the calm "watching" state
/// (reachable, nothing to report), never a fault, so an older catalog payload can
/// never red a row.
fn default_watching_status() -> String {
    "watching".to_string()
}

/// serde default for `rain_nature`: an absent value reads as the honest
/// fallback, a forecast (never green, never "live").
fn default_forecast_nature() -> String {
    "forecast".to_string()
}

/// serde default for the `region_appropriate` flag: absent reads as appropriate,
/// so a kind the server does not annotate is never wrongly hidden.
fn default_true() -> bool {
    true
}

/// The GET /api/config/source_catalog payload.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct CloudCatalog {
    #[serde(default)]
    pub lat: f64,
    #[serde(default)]
    pub lon: f64,
    #[serde(default)]
    pub cloud_sources: Vec<CloudEntry>,
}

/// The canonical "current" weather fields a cloud row can advertise, in a
/// stable display order, as (snake_case field name, short human label). The
/// compact capability SUMMARY ("Covers: temp, wind, humidity, rain") reads the
/// ones in `live_current_fields`; the per-field backup chain uses the same keys.
///
/// HONESTY INVARIANT: the field KEY is NEVER a hand-typed literal local to this
/// file. Each key is resolved from `sources_form::WEATHER_FIELD_OPTIONS`, the
/// canonical client-side `(WeatherField key, label)` list every source form
/// already shares. Those keys are pinned to the real `WeatherField` by an ssr
/// unit test (`every_field_option_is_a_real_weather_field` parses each via
/// `mqtt_subscribe::parse_weather_field`), and they ARE the snapshot's
/// `field_sources` / merge keys, so a listed field always maps to a real
/// reading. A UI label once drifted from the merge key (the Solar chip shipped
/// "solar_wm2" while the merge key is "solar_w_m2", so it read as permanently
/// dark even when Open-Meteo emitted solar); sourcing the key from the shared
/// canonical const makes that class of drift impossible. Only the short
/// capability LABEL and the display ORDER live here, addressed by canonical key.
fn capability_fields() -> Vec<(&'static str, &'static str)> {
    use crate::components::sources_form::WEATHER_FIELD_OPTIONS;
    // (canonical key, short label) in display order. The key is looked up in the
    // shared canonical list, so a typo here resolves to no key and is a
    // compile-visible empty set in tests, never a silently mislabeled one.
    const CHIPS: &[(&str, &str)] = &[
        ("air_temp_f", "Temp"),
        ("rh_pct", "Humidity"),
        ("wind_mph", "Wind"),
        ("rain_today_in", "Rain"),
        ("pressure_in_hg", "Pressure"),
        ("solar_w_m2", "Solar"),
        ("uv_index", "UV"),
    ];
    CHIPS
        .iter()
        .filter_map(|(key, label)| {
            // Resolve the key to the canonical entry so a field name cannot drift
            // from the merge layer (every WEATHER_FIELD_OPTIONS key is a real
            // WeatherField, enforced by the ssr unit test next to it).
            WEATHER_FIELD_OPTIONS
                .iter()
                .find(|(k, _)| k == key)
                .map(|(k, _)| (*k, *label))
        })
        .collect()
}

/// The canonical merge key the live RAIN reading is owned under in the
/// snapshot's `field_sources` map. Resolved from the shared capability list (the
/// "Rain" label), never a local literal, so it tracks the merge key the engine
/// actually writes. Falls back to the known key if the label is ever renamed.
fn rain_field_key() -> &'static str {
    capability_fields()
        .into_iter()
        .find(|(_, label)| *label == "Rain")
        .map(|(k, _)| k)
        .unwrap_or("rain_today_in")
}

/// The lowercase friendly word for a canonical capability field, for the
/// homeowner status line ("Feeding wind now"). Resolved off the same shared
/// capability label list the row + chain use, so it tracks the merge key and can
/// never drift. Falls back to the raw key only if a label is renamed away.
fn friendly_field_word(field_key: &str) -> String {
    capability_fields()
        .into_iter()
        .find(|(k, _)| *k == field_key)
        .map(|(_, label)| label.to_lowercase())
        .unwrap_or_else(|| field_key.to_string())
}

/// The single CALM homeowner status WORD for a row, mapped from the catalog
/// `status` enum PLUS which field this kind owns right now PLUS the enable / key /
/// region flags. Returns (semantic-color slug, the one word the homeowner reads).
/// This is the ONLY status text on the collapsed row: no pill, no sentence, one
/// weighted word in one semantic color (slug -> `cloud-word--<slug>` in main.scss).
///
/// Decision order (honest, never "stale"/"offline"/"error" for a calm state):
///   region-gated                         -> dim     "Not in your area"
///   not enabled, keyed                   -> warn    "Add key to turn on"
///   not enabled, keyless                 -> dim     "Off"
///   active, owns the rain field          -> owner   "Feeding rain now"
///   active, owns another field           -> owner   "Feeding <field> now"
///   standby                              -> neutral "On, standby"
///   watching                             -> neutral "Watching, no rain"
///   falling_through                      -> neutral "Quiet here right now"
///   offline (a true fault, enabled)      -> fault   "Not reachable right now"
///
/// `owned_rain` / `owned_other` are this kind's live field ownership read off the
/// snapshot `field_sources` (matched by friendly label), so the `active` word can
/// name the exact reading it is feeding. `status` is the verbatim catalog string.
///
/// Made `pub` so the Devices hub's weather-source cards reuse the EXACT same calm
/// word for a cloud source that is already configured (looked up in the catalog by
/// its `source_kind`), instead of inventing a second status vocabulary.
pub fn homeowner_status_word(
    status: &str,
    enabled: bool,
    keyless: bool,
    region_gated: bool,
    owned_rain: bool,
    owned_other_field: Option<String>,
) -> (&'static str, String) {
    // The off / gated cases are flag-driven, congruent with the contract: the
    // catalog reads `offline` for a not-enabled kind, but the homeowner word comes
    // off the enable + key + region flags, never the raw `offline` enum.
    if region_gated {
        return ("dim", "Not in your area".to_string());
    }
    if !enabled {
        return if keyless {
            ("dim", "Off".to_string())
        } else {
            ("warn", "Add key to turn on".to_string())
        };
    }
    match status {
        "active" => {
            if owned_rain {
                ("owner", "Feeding rain now".to_string())
            } else if let Some(field) = owned_other_field {
                ("owner", format!("Feeding {field} now"))
            } else {
                // Active per the catalog but the snapshot has not surfaced which
                // field yet (a refresh-timing gap); read the calm generic "now".
                ("owner", "Feeding readings now".to_string())
            }
        }
        "standby" => ("neutral", "On, standby".to_string()),
        "falling_through" => ("neutral", "Quiet here right now".to_string()),
        // A genuinely-unreachable enabled source is the ONLY fault word. Plain,
        // never the raw source id, never "stale"/"error".
        "offline" => ("fault", "Not reachable right now".to_string()),
        // watching (and any unknown value): the calm reachable-but-quiet default.
        _ => ("neutral", "Watching, no rain".to_string()),
    }
}

/// The calm status word for a catalog cloud entry, for the Devices hub's
/// weather-source card. It resolves the same enable / keyless / region flags the
/// row does off the entry itself, then defers to `homeowner_status_word`. The card
/// has no live per-field ownership plumbed (that lives in the panel's snapshot
/// fetch), so ownership reads `false` / `None`: the word reflects the catalog
/// status ("On, standby", "Watching, no rain", "Not reachable right now") rather
/// than naming the exact field it feeds. Two same-kind sources therefore share a
/// word, which is acceptable (a READ-ONLY status cue, not an owner claim). Returns
/// (semantic slug, word) so the card reuses the same `cloud-word--<slug>` classes.
pub fn cloud_status_word_for_entry(entry: &CloudEntry) -> (&'static str, String) {
    let keyless = is_keyless(&entry.kind);
    let region_gated =
        matches!(entry.kind.as_str(), "nws" | "noaa_mrms") && !entry.recommended_here;
    homeowner_status_word(
        &entry.status,
        entry.already_configured,
        keyless,
        region_gated,
        false,
        None,
    )
}

/// THE one honest rain badge, derived from `rain_nature` (NOT `data_nature`).
/// Returns (css-modifier slug, glyph name, word). Color + glyph + word together
/// (the a11y rule). The word "live" never appears on a Forecast.
///
///   * observation -> green "Measures rain"        (a real station gauge)
///   * radar_qpe   -> green/teal "Radar-measured rain" (gauge-corrected radar)
///   * nowcast     -> blue "Nowcast"               (live radar + station blend)
///   * forecast    -> amber "Forecast only"        (a model / ML estimate)
///
/// The eyebrow rain badge it built was retired from the row (the rain nature now
/// reads as plain prose in the expand via `rain_badge_meaning`), so this mapping
/// is kept only as the honesty CONTRACT GUARD its unit test exercises.
#[cfg(test)]
fn rain_badge(rain_nature: &str) -> (&'static str, &'static str, &'static str) {
    match rain_nature {
        "observation" => ("measures", "droplet", "Measures rain"),
        "radar_qpe" => ("radar", "activity", "Radar-measured rain"),
        "nowcast" => ("nowcast", "cloud-drizzle", "Nowcast"),
        // forecast (and any unknown value falls back to the honest default).
        _ => ("forecast", "cloud-sun", "Forecast only"),
    }
}

/// The plain-English hover meaning behind the rain badge, keyed off the same
/// `rain_nature`. No jargon, no em dashes.
fn rain_badge_meaning(rain_nature: &str) -> &'static str {
    match rain_nature {
        "observation" => {
            "Its rain reading is a real station gauge measurement. It can lag and \
             it is not your exact yard, but rain that registered actually fell."
        }
        "radar_qpe" => {
            "Its rain reading is gauge-corrected radar, observation grade. It \
             measures the rain that fell on a 1 km cell over your block, the best \
             off-yard read short of your own gauge."
        }
        "nowcast" => {
            "Its rain reading is a very-short-range analysis blending live radar \
             and station reports. Seconds of lag, a grid estimate."
        }
        // forecast (and any unknown value).
        _ => {
            "Its rain reading is a model or ML estimate for the current interval, \
             never a direct measurement. Treat it as a prediction, not proof."
        }
    }
}

/// True when the rain badge is a green (measured) nature, so a caller can pick
/// the "measure rain" vs "forecast it" bucket for the section summary.
fn rain_is_measured_nature(rain_nature: &str) -> bool {
    matches!(rain_nature, "observation" | "radar_qpe")
}

/// The short honest WORD for a snapshot `rain_nature` value (the live rain owner
/// nature off the snapshot, serialized "measured" | "radar_qpe" | "model"), for
/// the lead line. Distinct enum from the catalog's per-kind `rain_nature`.
fn snapshot_rain_word(nature: &str) -> &'static str {
    match nature {
        "measured" => "Measured",
        "radar_qpe" => "Radar",
        // model (and any unknown value): an honest forecast, never "live".
        _ => "Forecast",
    }
}

/// Friendly title for a cloud kind (reuses the shared resolver so a kind reads
/// as a brand). NOAA MRMS is named locally so the row reads as a brand even if
/// the shared resolver has not yet learned the kind.
fn cloud_title(kind: &str) -> String {
    if kind == "noaa_mrms" {
        return "NOAA MRMS (US radar rain)".to_string();
    }
    crate::components::sources_form::cloud_service_name(kind).to_string()
}

/// The provider get-a-key URL for a keyed kind. Shown next to the SecretInput so
/// the customer can fetch a key without leaving the page. None for keyless kinds.
fn get_a_key_url(kind: &str) -> Option<&'static str> {
    match kind {
        "pirate_weather" => Some("https://pirate-weather.apiable.io/"),
        "openweather" => Some("https://openweathermap.org/api"),
        "weatherkit" => Some("https://developer.apple.com/weatherkit/"),
        _ => None,
    }
}

/// True when a kind is keyless (a single Toggle enables it). The keyless kinds
/// need no account: Open-Meteo, NWS, NOAA MRMS, Met.no. Everything else is keyed.
fn is_keyless(kind: &str) -> bool {
    matches!(kind, "open_meteo" | "nws" | "met_norway" | "noaa_mrms")
}

/// The shared tooltip behind the "On by default here" marker. Spelled out once
/// so the row marker, the wizard marker, and the section explainer all use the
/// same words: a US user understands WHY NWS + NOAA MRMS + Open-Meteo are their
/// defaults and Met.no is merely optional.
const REGION_DEFAULT_TOOLTIP: &str = "We turn this on automatically because it is \
    a best free source for your location, no key needed. (NWS + NOAA MRMS in the \
    US, Met.no in Europe and the Nordics, Open-Meteo everywhere.)";

/// True when a keyed kind needs the FULL source editor (more than one secret),
/// so the single-SecretInput one-click flow would synthesize a dead entry.
///
/// WeatherKit is the only such kind: its JWT is signed from FOUR pieces (key_id,
/// team_id, service_id, and the .p8 private key). The one-click flow captures
/// only the .p8, leaving the three ids empty, which makes Apple return 401 on
/// the first poll AND the server validator (`validate::weatherkit_ids_incomplete`)
/// reject the save. So WeatherKit's "Add key to enable" routes to the full
/// `SourceEditorPanel` (which captures all four fields) instead of one-clicking
/// an enabled-but-dead entry. Pirate Weather + OpenWeather take a single
/// `api_key`, so they keep the simple one-click.
fn needs_full_editor(kind: &str) -> bool {
    kind == "weatherkit"
}

#[cfg(feature = "hydrate")]
async fn fetch_catalog() -> Result<CloudCatalog, String> {
    use gloo_net::http::Request;
    let resp = Request::get("/api/config/source_catalog")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        return Err(format!("HTTP {}", resp.status()));
    }
    resp.json::<CloudCatalog>().await.map_err(|e| e.to_string())
}

/// The live per-field owner map off the irrigation snapshot's `field_sources`
/// (field_name -> the source id currently driving it), plus the live rain
/// reading's honest nature + the snapshot freshness, so the lead line + the row
/// states can tell "warming up" from "active owner" and name the rain owner with
/// its honest nature. Best-effort: defaults on any failure read as "warming up".
#[cfg(feature = "hydrate")]
#[derive(Clone, Debug, Default)]
struct LiveChain {
    /// field_name -> owning source id (the live winner right now).
    owners: std::collections::BTreeMap<String, String>,
    /// The honest nature of the live rain reading: "measured" | "radar_qpe" |
    /// "model" (off the snapshot's forecast.rain_nature). Default "model".
    rain_nature: String,
    /// UTC epoch of the most recent successful refresh; 0 = cold start.
    refresh_epoch: i64,
}

#[cfg(feature = "hydrate")]
async fn fetch_live_chain() -> LiveChain {
    use gloo_net::http::Request;
    let Ok(resp) = Request::get("/api/v1/irrigation/snapshot").send().await else {
        return Default::default();
    };
    let Ok(v) = resp.json::<serde_json::Value>().await else {
        return Default::default();
    };
    let owners = v
        .get("field_sources")
        .and_then(|o| o.as_object())
        .map(|m| {
            m.iter()
                .filter_map(|(k, val)| Some((k.clone(), val.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default();
    let rain_nature = v
        .get("forecast")
        .and_then(|f| f.get("rain_nature"))
        .and_then(|n| n.as_str())
        .unwrap_or("model")
        .to_string();
    let refresh_epoch = v
        .get("last_refresh_epoch")
        .and_then(|x| x.as_i64())
        .unwrap_or(0);
    LiveChain {
        owners,
        rain_nature,
        refresh_epoch,
    }
}

/// One LOCAL (non-cloud) weather STATION entry as the chain reads it: the saved
/// config id, the friendly display name, the configured merge priority (a LAN
/// station seeds ~100, well above any cloud region priority), and the canonical
/// field keys it emits. The position-aware backup chain merges these into the
/// per-field candidate list so a PRIMARY station lands at its TRUE rank (the head
/// of every field it carries), never appended as a terminal "(last resort)".
#[derive(Clone, Debug, Default)]
pub struct StationEntry {
    /// The saved config source id (the merge key); also matched against the live
    /// `field_sources` owner so a station that is live-owning a field is bolded.
    pub id: String,
    /// The friendly display name (kind-pretty) shown in the chain.
    pub friendly_name: String,
    /// The configured merge priority (LAN station ~100). Higher wins.
    pub priority: i32,
    /// The canonical `field_overrides::field_name` keys this station emits, the
    /// SAME keys the cloud entries' `live_current_fields` use, so a station lands
    /// in the per-field candidate list for exactly the fields it covers.
    pub fields: Vec<String>,
}

/// The canonical LAN-station source kinds: a real station on the user's network
/// (or its hub cloud) that the merge ranks at ~100, ABOVE every cloud region
/// priority. These are the only `/api/config` source kinds the chain treats as a
/// LOCAL station candidate; every other kind is either a cloud catalog kind or a
/// generic mapping source that is not a station. Kept as one list so the chain
/// and any future caller agree on what "a station" is. Used by the hydrate-only
/// config fetch (and the unit tests), so it is gated to those builds.
#[cfg(any(feature = "hydrate", test))]
const LAN_STATION_KINDS: &[&str] = &[
    "tempest_udp",
    "tempest_ws",
    "ecowitt_local",
    "ecowitt_gw_poll",
    "davis_wll",
];

/// True when a `/api/config` source `kind` string is a LAN weather station (a
/// real station the merge ranks at ~100), so the chain reads it as the LOCAL
/// owner candidate rather than a cloud or generic-mapping source.
#[cfg(any(feature = "hydrate", test))]
fn is_lan_station_kind(kind: &str) -> bool {
    LAN_STATION_KINDS.contains(&kind)
}

/// The canonical field keys a LAN station of `kind` emits as live current
/// scalars, mirroring `runtime::source_field_names`' declared sets for the
/// stations so the client chain covers exactly the fields the station can own.
/// A full station carries the whole capability set (incl. Lightning for the
/// Tempest/Ecowitt families); Davis omits Lightning. Unknown kinds (never
/// reached, guarded by `is_lan_station_kind`) read empty.
#[cfg(any(feature = "hydrate", test))]
fn station_field_keys(kind: &str) -> Vec<&'static str> {
    match kind {
        "tempest_udp" | "tempest_ws" | "ecowitt_local" => vec![
            "air_temp_f",
            "rh_pct",
            "dew_point_f",
            "wind_mph",
            "wind_gust_mph",
            "wind_bearing_deg",
            "pressure_in_hg",
            "solar_w_m2",
            "uv_index",
            "rain_today_in",
            "rain_intensity_in_hr",
            "lightning_count",
        ],
        "ecowitt_gw_poll" => vec![
            "air_temp_f",
            "rh_pct",
            "dew_point_f",
            "wind_mph",
            "wind_gust_mph",
            "pressure_in_hg",
            "solar_w_m2",
            "uv_index",
            "rain_today_in",
            "rain_intensity_in_hr",
        ],
        // Davis WeatherLink Live: a full station minus the lightning channel.
        "davis_wll" => vec![
            "air_temp_f",
            "rh_pct",
            "dew_point_f",
            "wind_mph",
            "wind_gust_mph",
            "wind_bearing_deg",
            "pressure_in_hg",
            "solar_w_m2",
            "uv_index",
            "rain_today_in",
            "rain_intensity_in_hr",
        ],
        _ => Vec::new(),
    }
}

/// Fetch GET /api/config and extract the enabled non-cloud LAN STATION entries
/// for the position-aware backup chain, as `StationEntry { id, friendly_name,
/// priority, fields }`. Mirrors the same round-trip `devices.rs::fetch_config`
/// uses; the priority round-trips on each source entry, and the kind+config sit
/// flattened (the `SourceKind` serde tag) so we read `kind` directly. Only
/// `is_lan_station_kind` entries that are enabled are returned; a disabled or
/// cloud/generic source is skipped. Best-effort: any failure reads as no station.
#[cfg(feature = "hydrate")]
async fn fetch_station_entries() -> Vec<StationEntry> {
    use gloo_net::http::Request;
    let Ok(resp) = Request::get("/api/config").send().await else {
        return Vec::new();
    };
    let Ok(cfg) = resp.json::<serde_json::Value>().await else {
        return Vec::new();
    };
    let Some(sources) = cfg.get("sources").and_then(|s| s.as_array()) else {
        return Vec::new();
    };
    sources
        .iter()
        .filter_map(|s| {
            let kind = s.get("kind").and_then(|k| k.as_str())?;
            if !is_lan_station_kind(kind) {
                return None;
            }
            // Default enabled=true (matches the config schema default) when the
            // flag is absent, so a hand-trimmed entry is not silently dropped.
            let enabled = s.get("enabled").and_then(|e| e.as_bool()).unwrap_or(true);
            if !enabled {
                return None;
            }
            let id = s.get("id").and_then(|i| i.as_str())?.to_string();
            // The LAN-station default priority is 100 (config schema convention),
            // used when the entry omits it.
            let priority = s
                .get("priority")
                .and_then(|p| p.as_i64())
                .map(|p| p as i32)
                .unwrap_or(100);
            let friendly_name = crate::components::sources_form::friendly_source_name(kind);
            let fields = station_field_keys(kind)
                .into_iter()
                .map(|f| f.to_string())
                .collect();
            Some(StationEntry {
                id,
                friendly_name,
                priority,
                fields,
            })
        })
        .collect()
}

/// GET the live config, run a mutation on its `sources` array, PUT it back.
/// Same round-trip every settings page uses, so untouched config + secrets
/// survive (the server unredacts the sentinel). Returns the restart_reasons the
/// PUT carried (empty when the change hot-reloaded). NEVER computes priority:
/// the server's normalize_new_cloud_sources stamps region rank + max_age for any
/// source added on this write.
#[cfg(feature = "hydrate")]
async fn patch_sources<F>(mutate: F) -> Result<Vec<String>, String>
where
    F: FnOnce(&mut Vec<serde_json::Value>),
{
    use gloo_net::http::Request;
    let cur = Request::get("/api/config")
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !cur.ok() {
        return Err(format!("HTTP {}", cur.status()));
    }
    let mut cfg: serde_json::Value = cur.json().await.map_err(|e| e.to_string())?;
    {
        let obj = cfg
            .as_object_mut()
            .ok_or_else(|| "config is not an object".to_string())?;
        let arr = obj
            .entry("sources")
            .or_insert_with(|| serde_json::json!([]))
            .as_array_mut()
            .ok_or_else(|| "sources is not an array".to_string())?;
        mutate(arr);
    }
    let resp = Request::put("/api/config")
        .json(&cfg)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.ok() {
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("HTTP {}: {body}", resp.status()));
    }
    let reasons = resp
        .json::<serde_json::Value>()
        .await
        .ok()
        .filter(|v| {
            v.get("restart_required")
                .and_then(|r| r.as_bool())
                .unwrap_or(false)
        })
        .and_then(|v| {
            v.get("restart_reasons")
                .and_then(|r| r.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(str::to_string))
                        .collect()
                })
        })
        .unwrap_or_default();
    Ok(reasons)
}

/// Build the `config` block for a SINGLE-SECRET keyed kind from the secret the
/// customer typed. Only pirate_weather + openweather qualify: each takes one
/// `api_key`. WeatherKit is deliberately NOT here: it needs four pieces
/// (key_id, team_id, service_id, .p8), so a one-click that wrote only the .p8
/// with empty ids would 401 at Apple and be rejected by the server validator;
/// WeatherKit routes to the full source editor instead (see `needs_full_editor`
/// / `KeyedEnable`). A keyless kind, or any kind that needs the full editor,
/// needs no one-click config block (returns null).
#[cfg(feature = "hydrate")]
fn keyed_config(kind: &str, secret: &str) -> serde_json::Value {
    match kind {
        "pirate_weather" | "openweather" => serde_json::json!({ "api_key": secret }),
        _ => serde_json::Value::Null,
    }
}

/// The default `config` block for a keyless kind when first splicing it in. NWS +
/// Met.no want a User-Agent; Open-Meteo wants its forecast knobs; NOAA MRMS is
/// keyless with no scalar config (an empty object). The server validates +
/// normalizes, so these match the form's `default_config_text`.
#[cfg(feature = "hydrate")]
fn keyless_config(kind: &str) -> serde_json::Value {
    match kind {
        "nws" | "met_norway" => {
            serde_json::json!({ "user_agent": "localsky/0.2 (you@example.com)" })
        }
        "open_meteo" => serde_json::json!({
            "forecast_days": 7,
            "forecast_hours": 48,
            "past_days": 1,
            "include_radar": true,
        }),
        // NOAA MRMS is keyless with no required scalar config; the server stamps
        // its product default. An empty object lets normalize fill the rest.
        "noaa_mrms" => serde_json::json!({}),
        _ => serde_json::Value::Null,
    }
}

/// True when this entry belongs in the RECOMMENDED section: a region-recommended
/// keyless authority, OR a kind carrying an honest upgrade (Pirate in CONUS).
/// Everything else (paid, region-inappropriate, or a non-recommended keyless
/// kind) goes into the collapsed "More providers" fold.
fn is_recommended_section(e: &CloudEntry) -> bool {
    e.recommended_here || e.upgrade_available || e.upgrade_reason.is_some()
}

/// The catalog panel. Owns the catalog fetch + the live chain, renders the
/// section header + the lead "rain right now" line + the scannable provider LIST
/// (split RECOMMENDED / More), then the read-only backup chain beneath it.
/// First-class for every customer: a `for_hardware` host (a local
/// weather_gateway exists) sees the same list framed as a complement / backup, a
/// no-hardware host sees it as the primary path.
///
/// `on_changed` lets the host (Devices hub) refresh its device list after an
/// enable/disable PUT lands.
#[component]
pub fn CloudWeatherServices(
    /// True when the customer already has a local weather station. Only changes
    /// the framing copy (local sensors outrank cloud, cloud complements/backs
    /// up); the list + every honesty element is identical either way.
    #[prop(into, default = Signal::derive(|| false))]
    for_hardware: Signal<bool>,
    /// Whether to render the read-only backup-chain visualization beneath the
    /// provider list. Default `true` (the Settings/Devices posture, where a
    /// running engine has live owners to show). The WIZARD passes `false`: at
    /// setup there is no running engine to derive live owners from, and the
    /// hero + matrix + toggles already tell the capability truth, so the
    /// operational chain would only add noise. Strictly additive: the panel API
    /// is not forked, the wizard just hides one section.
    #[prop(default = true)]
    show_chain: bool,
    /// Fired after a successful enable/disable PUT so the host can refresh.
    #[prop(into, optional)]
    on_changed: Option<Callback<()>>,
    /// Asked to open the FULL source editor for a kind (the kind string).
    /// Multi-field keyed sources (WeatherKit) cannot use the single-SecretInput
    /// one-click flow without writing a dead, 401-ing entry, so they route here
    /// instead; the host (Devices hub) opens its `SourceEditorPanel` prefilled
    /// with that kind. None when no host wired it (the simple one-click stays
    /// the only path; a multi-field row then shows a disabled control + a note,
    /// never a one-click that would save a dead source).
    #[prop(into, optional)]
    on_edit_full: Option<Callback<String>>,
    /// A monotonic bump the HOST raises after it toggles/removes a source from the
    /// device-card list, so this panel refetches its catalog + chain. Without it,
    /// removing a source from the list would leave it stale in this panel's matrix
    /// + discovery (the two own separate catalog signals).
    #[prop(into, default = Signal::derive(|| 0u32))]
    reload_trigger: Signal<u32>,
) -> impl IntoView {
    let catalog: RwSignal<CloudCatalog> = RwSignal::new(CloudCatalog::default());
    let loaded = RwSignal::new(false);
    let error = RwSignal::new(String::new());
    // Live per-field ownership off the snapshot, for the lead line + the "active
    // owner" / "backup" row states + the backup chain ownership strip.
    let live_owners: RwSignal<std::collections::BTreeMap<String, String>> =
        RwSignal::new(std::collections::BTreeMap::new());
    // The honest nature of the live rain reading right now ("measured" |
    // "radar_qpe" | "model"), off the snapshot's forecast.rain_nature.
    let live_rain_nature: RwSignal<String> = RwSignal::new("model".to_string());
    let live_refresh: RwSignal<i64> = RwSignal::new(0);
    // The enabled LOCAL (non-cloud) weather STATION entries off /api/config, for
    // the position-aware backup chain: a PRIMARY station merges into the per-field
    // candidate list at its TRUE priority (~100) so it reads as the HEAD of every
    // field it carries, never appended as a terminal "(last resort)".
    let stations: RwSignal<Vec<StationEntry>> = RwSignal::new(Vec::new());
    // A monotonic bump to re-fetch the catalog + live chain after a PUT.
    let reload = RwSignal::new(0u32);
    let saving = RwSignal::new(false);
    let result_msg = RwSignal::new(String::new());
    let result_ok = RwSignal::new(false);
    // Persistent, dismissible restart-required banner (a newly enabled source
    // needs a boot-wired refresher); routine toggles hot-reload and leave it
    // empty. Reset on each restart-required save.
    let restart_reasons: RwSignal<Vec<String>> = RwSignal::new(Vec::new());
    let restart_dismissed = RwSignal::new(false);

    // reload_trigger is consumed only by the hydrate-gated fetch effect below.
    #[cfg(not(feature = "hydrate"))]
    let _ = reload_trigger;

    #[cfg(feature = "hydrate")]
    {
        Effect::new(move |_| {
            let _ = reload.get();
            // Also refetch when the HOST bumps its trigger (a device-card
            // toggle/remove), so this panel's matrix + discovery never go stale.
            let _ = reload_trigger.get();
            wasm_bindgen_futures::spawn_local(async move {
                match fetch_catalog().await {
                    Ok(c) => {
                        catalog.set(c);
                        error.set(String::new());
                    }
                    Err(e) => error.set(e),
                }
                let live = fetch_live_chain().await;
                live_owners.set(live.owners);
                live_rain_nature.set(live.rain_nature);
                live_refresh.set(live.refresh_epoch);
                // The LOCAL station entries for the position-aware chain. Read off
                // the same /api/config the PUTs round-trip, so the station's TRUE
                // priority drives its chain rank.
                stations.set(fetch_station_entries().await);
                loaded.set(true);
            });
        });
    }
    #[cfg(not(feature = "hydrate"))]
    {
        let _ = (
            reload,
            error,
            live_owners,
            live_rain_nature,
            live_refresh,
            stations,
            on_changed,
        );
    }

    // After any PUT: refresh result, raise/clear the restart banner, bump reload,
    // and notify the host. Shared by every row's enable/disable path. Only the
    // hydrate build drives a PUT, so the ssr build never calls it.
    #[cfg_attr(not(feature = "hydrate"), allow(unused_variables))]
    let apply_result = move |outcome: Result<Vec<String>, String>| {
        saving.set(false);
        match outcome {
            Ok(reasons) => {
                result_ok.set(true);
                result_msg.set("Saved. Applied to the live engine.".to_string());
                restart_dismissed.set(false);
                restart_reasons.set(reasons);
                reload.update(|n| *n += 1);
                if let Some(cb) = on_changed {
                    cb.run(());
                }
            }
            Err(e) => {
                result_ok.set(false);
                result_msg.set(e);
            }
        }
    };

    // Enable/disable a KEYLESS kind: splice {id:kind, kind, enabled:flag, config}
    // (config only on the first add) and PUT. The server stamps region priority.
    #[allow(unused_variables)]
    let set_keyless = Callback::new(move |(kind, on): (String, bool)| {
        #[cfg(feature = "hydrate")]
        {
            saving.set(true);
            result_msg.set(String::new());
            wasm_bindgen_futures::spawn_local(async move {
                let kind_for_mut = kind.clone();
                let outcome = patch_sources(move |arr| {
                    if let Some(slot) = arr
                        .iter_mut()
                        .find(|s| s.get("kind").and_then(|v| v.as_str()) == Some(&kind_for_mut))
                    {
                        // Flip the existing entry; keep its config + any
                        // server-stamped priority untouched.
                        if let Some(obj) = slot.as_object_mut() {
                            obj.insert("enabled".into(), serde_json::Value::Bool(on));
                        }
                    } else if on {
                        // First add. NEVER set priority; normalize stamps it.
                        arr.push(serde_json::json!({
                            "id": kind_for_mut,
                            "kind": kind_for_mut,
                            "enabled": true,
                            "config": keyless_config(&kind_for_mut),
                        }));
                    }
                })
                .await;
                apply_result(outcome);
            });
        }
    });

    // Enable a KEYED kind with the secret the customer typed: write key +
    // enabled:true in a SINGLE PUT (never a placeholder-key entry that 401s).
    #[allow(unused_variables)]
    let enable_keyed = Callback::new(move |(kind, secret): (String, String)| {
        #[cfg(feature = "hydrate")]
        {
            saving.set(true);
            result_msg.set(String::new());
            wasm_bindgen_futures::spawn_local(async move {
                let kind_for_mut = kind.clone();
                let secret_for_mut = secret.clone();
                let outcome = patch_sources(move |arr| {
                    let cfg = keyed_config(&kind_for_mut, &secret_for_mut);
                    if let Some(slot) = arr
                        .iter_mut()
                        .find(|s| s.get("kind").and_then(|v| v.as_str()) == Some(&kind_for_mut))
                    {
                        if let Some(obj) = slot.as_object_mut() {
                            obj.insert("enabled".into(), serde_json::Value::Bool(true));
                            obj.insert("config".into(), cfg);
                        }
                    } else {
                        arr.push(serde_json::json!({
                            "id": kind_for_mut,
                            "kind": kind_for_mut,
                            "enabled": true,
                            "config": cfg,
                        }));
                    }
                })
                .await;
                apply_result(outcome);
            });
        }
    });

    // Disable a KEYED kind: flip enabled:false but KEEP the entry (and its key)
    // so re-enabling needs no retype.
    #[allow(unused_variables)]
    let disable_keyed = Callback::new(move |kind: String| {
        #[cfg(feature = "hydrate")]
        {
            saving.set(true);
            result_msg.set(String::new());
            wasm_bindgen_futures::spawn_local(async move {
                let kind_for_mut = kind.clone();
                let outcome = patch_sources(move |arr| {
                    if let Some(slot) = arr
                        .iter_mut()
                        .find(|s| s.get("kind").and_then(|v| v.as_str()) == Some(&kind_for_mut))
                    {
                        if let Some(obj) = slot.as_object_mut() {
                            obj.insert("enabled".into(), serde_json::Value::Bool(false));
                        }
                    }
                })
                .await;
                apply_result(outcome);
            });
        }
    });

    // Render one row from an entry, with all the shared wiring.
    let render_row = move |entry: CloudEntry| {
        view! {
            <CloudRow
                entry=entry
                live_owners=live_owners
                saving=saving
                set_keyless=set_keyless
                enable_keyed=enable_keyed
                disable_keyed=disable_keyed
                on_edit_full=on_edit_full
            />
        }
    };

    // The DISCOVERY list: the cloud providers that are AVAILABLE but not yet
    // configured, so a user turns on more coverage from here. An already-configured
    // cloud source is DELIBERATELY excluded: it lives in the "Weather sources"
    // device-card list now (with its own status word + on/off toggle + remove), so
    // showing it here too would duplicate the source. Split into RECOMMENDED (region
    // authorities + Pirate's upgrade) and a collapsed "More providers (N)" fold
    // (paid + region inappropriate). The fold keeps the default view calm: a US user
    // sees Open-Meteo, NWS, NOAA MRMS, and Pirate without opening it.
    let cloud_rows = move || {
        let entries: Vec<CloudEntry> = catalog.with(|c| {
            c.cloud_sources
                .iter()
                // Presence, not enabled: a configured-but-DISABLED cloud source
                // lives in the device-card list (with its toggle), so it must not
                // also show here as "available" (the exactly-once rule).
                .filter(|e| !e.configured_present)
                .cloned()
                .collect()
        });
        if entries.is_empty() {
            // Every available cloud source is already configured (they render in
            // the device-card list), or none loaded yet.
            let msg = if loaded.get() {
                "Every available cloud service is already on. It appears in your weather sources below."
            } else {
                "Loading cloud weather services\u{2026}"
            };
            return view! { <p class="settings-empty">{msg}</p> }.into_any();
        }
        let (recommended, more): (Vec<_>, Vec<_>) =
            entries.into_iter().partition(is_recommended_section);
        let recommended_items: Vec<_> = recommended.into_iter().map(render_row).collect();
        let more_count = more.len();
        let more_items: Vec<_> = more.into_iter().map(render_row).collect();

        let more_block = (!more_items.is_empty()).then(|| {
            view! {
                <details class="cloud-weather__more">
                    <summary class="cloud-weather__more-summary">
                        <Icon name="chevron-right" size=18/>
                        {format!("Show {more_count} more providers")}
                        <span class="cloud-weather__more-hint">
                            "paid or built for another region"
                        </span>
                    </summary>
                    <ul class="cloud-weather-list">{more_items}</ul>
                </details>
            }
        });

        view! {
            <ul class="cloud-weather-list">{recommended_items}</ul>
            {more_block}
        }
        .into_any()
    };

    // DISCOVERY, "Add weather coverage": the panel is now a SUMMARY (hero +
    // capability matrix) plus this ADD list of the AVAILABLE-but-unconfigured cloud
    // providers. The configured sources (local stations AND already-on cloud) live
    // in the host Devices hub's "Weather sources" device-card list (each with its
    // own status word + on/off toggle + remove), so the old configured-source bands
    // are gone: showing a configured source here too would duplicate it. This block
    // renders ONLY the unconfigured rows (`cloud_rows` filters `already_configured`
    // out), so a US user sees the free authorities they have not yet turned on. The
    // heading is persona-aware only in framing, never in content.
    let discovery_list = move || {
        let framing = if for_hardware.get() {
            "Turn on a cloud service to fill readings your station does not cover, \
             or to back it up if it goes quiet."
        } else {
            "Turn on a free service to cover your yard now. No station to buy, and \
             the default for your region is already on."
        };
        view! {
            <div class="cloud-band cloud-band--cloud entity-stripe entity-stripe--source">
                <h3 class="cloud-band__title">"Add weather coverage"</h3>
                <p class="cloud-band__sub">{framing}</p>
                {cloud_rows()}
            </div>
        }
    };

    // FIRST-RUN GATE (persona A): true once ANY source is on, a local station OR
    // an enabled cloud entry. Until then the hero and the backup chain are
    // suppressed (they would each render their own "nothing yet" empty state, so
    // a brand-new user saw the same "nothing is set up" fact three times). The
    // provider list carries the single first-run invite instead.
    let any_source_on = move || {
        !stations.get().is_empty()
            || catalog.with(|c| c.cloud_sources.iter().any(|e| e.already_configured))
    };

    // LEAD WITH THE DATA: the HERO CARD, the loudest element on the section. It
    // answers "is it raining on my yard, and how do I know" before any provider
    // scroll, off the snapshot's live rain owner + honest nature + age:
    //   line 1  label
    //   line 2  the active reading source + its ONE trust word + age
    //   line 3  a single calm rollup chip (covered / on forecast backup), never
    //           red unless a field has zero possible owner.
    // The old `summary` count + `explainer` framing demote into the card subtext.
    let hero = move || {
        // Framing subtext: same words as the old explainer, now the card's quiet
        // second voice rather than a competing paragraph.
        let subtext = if for_hardware.get() {
            "Your weather station always wins for the readings it covers. Turn on a \
             cloud service to fill the readings it does not, or to back it up if it \
             goes quiet. Free options need no account."
        } else {
            "Live conditions for your exact spot, pulled from weather services. No \
             station to buy. Free options need no account, and the default for your \
             region is already on."
        };

        if !loaded.get() {
            return view! {
                <div class="cloud-hero cloud-hero--warming">
                    <span class="cloud-hero__label">"Rain on your yard right now"</span>
                    <span class="cloud-hero__reading">"Checking your weather feeds\u{2026}"</span>
                    <p class="cloud-hero__subtext">{subtext}</p>
                </div>
            }
            .into_any();
        }

        let owners = live_owners.get();
        let nature = live_rain_nature.get();
        let owner_label = owners.get(rain_field_key()).cloned();
        let trust = snapshot_rain_word(&nature);
        let trust_slug = match nature.as_str() {
            "measured" => "measures",
            "radar_qpe" => "radar",
            _ => "forecast",
        };
        let trust_cls = format!("cloud-hero__trust cloud-hero__trust--{trust_slug}");

        match owner_label {
            Some(label) => {
                // The owner value in `field_sources` is already the friendly brand
                // label (the merge writes the friendly name), so show it verbatim.
                let owner = crate::components::sources_form::friendly_source_name(&label);
                let ago = relative_ago(live_refresh.get());
                // Rollup chip: green "All readings covered" when a measured/radar
                // source owns the rain reading; calm "Rain on forecast backup" when
                // a model is covering rain because the live observation/radar source
                // is quiet. Never red here: a model floor always covers rain.
                let measured = rain_is_measured_nature(match nature.as_str() {
                    // Map the snapshot rain-nature enum onto the catalog one so the
                    // shared "is this measured" test applies to both.
                    "measured" => "observation",
                    "radar_qpe" => "radar_qpe",
                    _ => "forecast",
                });
                let (rollup_slug, rollup_word) = if measured {
                    ("covered", "All readings covered")
                } else {
                    ("backup", "Rain on forecast backup")
                };
                let rollup_cls = format!("cloud-hero__rollup cloud-hero__rollup--{rollup_slug}");
                view! {
                    <div class="cloud-hero">
                        <span class="cloud-hero__label">"Rain on your yard right now"</span>
                        <span class="cloud-hero__reading">
                            <strong class="cloud-hero__owner">{owner}</strong>
                            <span class=trust_cls>{trust}</span>
                            <span class="cloud-hero__ago">{format!("updated {ago}")}</span>
                        </span>
                        <span class=rollup_cls>{rollup_word}</span>
                        <p class="cloud-hero__subtext">{subtext}</p>
                    </div>
                }
                .into_any()
            }
            // Nothing owns rain yet: warming up, never a false "live" claim and
            // never red (turning on a service below covers it).
            None => view! {
                <div class="cloud-hero cloud-hero--warming">
                    <span class="cloud-hero__label">"Rain on your yard right now"</span>
                    <span class="cloud-hero__reading">
                        "Warming up. Turn on a service below to cover your rain reading."
                    </span>
                    <p class="cloud-hero__subtext">{subtext}</p>
                </div>
            }
            .into_any(),
        }
    };

    view! {
        <section class="cloud-weather settings-prominent-section">
            <header class="cloud-weather__header">
                <div class="cloud-weather__heading">
                    <Icon name="sources" size=20/>
                    <h2 class="cloud-weather__title">"Weather sources"</h2>
                </div>
            </header>

            // The hero is SUPPRESSED at first run (persona A): with no source on
            // it would only echo the provider list's own "turn one on" invite.
            {move || any_source_on().then(hero)}

            // The compact at-a-glance capability matrix, rendered IN-APP now that
            // the pane is wide enough (the SCSS phase widened the settings shell).
            // The old doc-exile note is gone; the full detail guide link stays for
            // the per-provider deep dive. Only shown once a source is on (it reads
            // the enabled providers).
            {move || any_source_on().then(|| view! {
                <CapabilityMatrix catalog=catalog/>
            })}

            <RestartBanner reasons=restart_reasons dismissed=restart_dismissed/>

            {move || {
                let m = result_msg.get();
                (!m.is_empty()).then(|| {
                    let cls = if result_ok.get() {
                        "setup-result setup-result--ok"
                    } else {
                        "setup-result setup-result--err"
                    };
                    view! { <p class=cls role="status">{m}</p> }
                })
            }}
            {move || {
                let e = error.get();
                (!e.is_empty()).then(|| view! {
                    <p class="setup-result setup-result--err" role="alert">
                        {format!("Couldn't load the cloud catalog: {e}")}
                    </p>
                })
            }}

            // DISCOVERY, "Add weather coverage": the AVAILABLE-but-unconfigured
            // cloud providers a user can turn on, each with its existing enable
            // toggle / add-key control. The CONFIGURED sources (local stations AND
            // already-on cloud) are NOT here: they live in the host Devices hub's
            // "Weather sources" device-card list, so this panel is a SUMMARY (hero +
            // matrix) plus this ADD list. The capability matrix above carries
            // provider x reading; the full per-provider detail lives in the guide.
            {discovery_list}

            // The read-only backup chain is now OFF in the Devices hub
            // (`show_chain=false`): the Advanced per-field picker is the ONE home
            // for source-to-reading ownership, so the chain would be a second
            // representation of the same fact (persona E). It is kept behind the
            // opt-in prop (and additionally gated on `any_source_on` so it never
            // renders its own "no source is on yet" empty state at first run) for
            // any future host that wants the visualization.
            {move || (show_chain && any_source_on()).then(|| view! {
                <ActiveChain
                    catalog=catalog
                    stations=stations
                    live_owners=live_owners
                />
            })}
        </section>
    }
}

/// The theme token a per-field nature tints its lit matrix cell with, so the
/// cell truth (measured / radar / nowcast / forecast) reads at a glance in the
/// SAME palette the rain badge + status words use. An unlit cell is dim. The
/// nature strings are the shared snake_case CloudDataNature wire values.
fn nature_color_token(nature: &str) -> &'static str {
    match nature {
        // Measured (a real gauge) + radar QPE both read healthy green.
        "observation" | "radar_qpe" => "var(--accent-good)",
        // A live nowcast blend reads brand blue.
        "nowcast" => "var(--accent)",
        // Forecast (and any unknown) reads the amber "needs judgement" tone.
        _ => "var(--accent-warn)",
    }
}

/// The COMPACT, at-a-glance capability matrix, rendered IN-APP now that the
/// settings pane is wide enough (the SCSS phase widened the shell). One row per
/// ENABLED provider, one column per canonical current-weather field; a lit cell
/// is a dot tinted by that field's own honest nature (measured / radar / nowcast
/// / forecast), an unlit cell is a dim dash. This is the calm summary; the full
/// per-provider detail lives in the guide, linked below. It reads the same
/// catalog fields the rows do (`live_current_fields` + `field_natures`), so it
/// can never drift from the running merge. Rendered as a semantic table with
/// token-driven inline styles (no bespoke stylesheet class), so it stays on
/// theme without adding SCSS.
#[component]
fn CapabilityMatrix(catalog: RwSignal<CloudCatalog>) -> impl IntoView {
    let table = move || {
        let fields = capability_fields();
        let enabled: Vec<CloudEntry> = catalog.with(|c| {
            c.cloud_sources
                .iter()
                .filter(|e| e.already_configured)
                .cloned()
                .collect()
        });
        if enabled.is_empty() {
            return view! {
                <p class="settings-empty">"Turn on a cloud source to compare what each reading covers."</p>
            }
            .into_any();
        }

        let head_cells: Vec<_> = fields
            .iter()
            .map(|(_, label)| {
                view! {
                    <th scope="col" style="padding:0.3rem 0.5rem;font-weight:600;color:var(--text-dim);font-size:var(--text-caption);text-align:center">
                        {*label}
                    </th>
                }
            })
            .collect();

        let body_rows: Vec<_> = enabled
            .iter()
            .map(|e| {
                let name = cloud_title(&e.kind);
                // Per-field nature lookup for THIS provider (same keys/order as
                // live_current_fields), so a lit cell tints by its own truth.
                let natures: std::collections::BTreeMap<&str, &str> = e
                    .field_natures
                    .iter()
                    .map(|(k, n)| (k.as_str(), n.as_str()))
                    .collect();
                let cells: Vec<_> = fields
                    .iter()
                    .map(|(key, label)| {
                        let lit = e.live_current_fields.iter().any(|f| f == key);
                        let cell = if lit {
                            // A lit cell reads as a dot tinted by the field's own
                            // nature (falls back to the row headline data_nature).
                            let nature = natures
                                .get(key)
                                .copied()
                                .unwrap_or(e.data_nature.as_str());
                            let color = nature_color_token(nature);
                            view! {
                                <span
                                    aria-label=format!("{label}: covered")
                                    style=format!("display:inline-block;width:0.5rem;height:0.5rem;border-radius:50%;background:{color}")
                                ></span>
                            }
                            .into_any()
                        } else {
                            view! {
                                <span aria-label=format!("{label}: not covered") style="color:var(--text-faint)">"\u{2013}"</span>
                            }
                            .into_any()
                        };
                        view! {
                            <td style="padding:0.3rem 0.5rem;text-align:center">{cell}</td>
                        }
                    })
                    .collect();
                view! {
                    <tr>
                        <th scope="row" style="padding:0.3rem 0.5rem;font-weight:600;color:var(--text);text-align:left;white-space:nowrap">
                            {name}
                        </th>
                        {cells}
                    </tr>
                }
            })
            .collect();

        view! {
            <table style="width:100%;border-collapse:collapse;font-size:var(--text-caption)">
                <thead>
                    <tr>
                        <th scope="col" style="padding:0.3rem 0.5rem;text-align:left;font-weight:600;color:var(--text-dim);font-size:var(--text-caption)">
                            "Provider"
                        </th>
                        {head_cells}
                    </tr>
                </thead>
                <tbody>{body_rows}</tbody>
            </table>
        }
        .into_any()
    };

    view! {
        <div class="settings-shell__pane-body" style="margin:0 0 var(--space-4);overflow-x:auto">
            <div class="settings-section-head" style="margin-bottom:var(--space-2)">
                <h3 class="settings-section__title" style="font-size:var(--text-h3)">"What each reading covers"</h3>
                <p class="settings-section-head__sub">
                    "A dot marks a covered reading, tinted by how honest that reading is: "
                    <span style="color:var(--accent-good)">"green measured"</span>", "
                    <span style="color:var(--accent)">"blue nowcast"</span>", "
                    <span style="color:var(--accent-warn)">"amber forecast"</span>"."
                </p>
            </div>
            {table}
            <a
                class="cloud-weather__matrix-link"
                href=crate::docs::doc_url("provider-matrix")
                target="_blank"
                rel="noopener noreferrer"
            >
                "See the full per-provider comparison \u{2192}"
            </a>
        </div>
    }
}

/// Relative "Nm ago" for the lead line, from a UTC epoch and the current time.
/// Plain and short (seconds / minutes / hours). 0 epoch (cold start) reads "just
/// now" so the lead never shows a nonsense age before the first refresh. Uses
/// the wall clock only under hydrate (the ssr build never renders the live
/// lead), so the ssr path returns a stable placeholder.
#[cfg(feature = "hydrate")]
fn relative_ago(epoch: i64) -> String {
    if epoch <= 0 {
        return "just now".to_string();
    }
    let now = (js_sys::Date::now() / 1000.0) as i64;
    let secs = (now - epoch).max(0);
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else {
        format!("{}h ago", secs / 3600)
    }
}

#[cfg(not(feature = "hydrate"))]
fn relative_ago(_epoch: i64) -> String {
    "just now".to_string()
}

/// One cloud service ROW in the scannable list. By DEFAULT the row is COLLAPSED
/// and carries exactly the compact quick hits: a left entity-stripe, the
/// chevron, the friendly name, THE ONE rain badge (derived from rain_nature),
/// the key chip, an optional "On by default here" marker, and the enable
/// control. Clicking the chevron / the identity (or the "Add key" button) flips
/// `expanded`, revealing the DETAILS: the compact capability summary, the
/// real-time + localization lines, the italic watering-risk line, the key
/// caution, the Met.no synthetic-POP note, the Pirate upgrade line, and (for a
/// keyed kind) the key-entry form. Progressive disclosure, every honesty fact
/// read VERBATIM from the catalog.
#[component]
fn CloudRow(
    entry: CloudEntry,
    live_owners: RwSignal<std::collections::BTreeMap<String, String>>,
    saving: RwSignal<bool>,
    set_keyless: Callback<(String, bool)>,
    enable_keyed: Callback<(String, String)>,
    disable_keyed: Callback<String>,
    /// Open the full source editor for a kind (multi-field keyed sources like
    /// WeatherKit route here instead of the one-click flow). None = unwired.
    on_edit_full: Option<Callback<String>>,
) -> impl IntoView {
    let kind = entry.kind.clone();
    let title = cloud_title(&kind);
    let enabled = entry.already_configured;
    let keyless = is_keyless(&kind);
    // NWS + NOAA MRMS are the region-gated kinds: both are US-only, so outside
    // the US the catalog reports recommended_here=false AND the customer cannot
    // use them. We gate exactly these two; a non-US row disables its control and
    // explains, rather than reading as broken.
    let region_gated = matches!(kind.as_str(), "nws" | "noaa_mrms") && !entry.recommended_here;
    let recommended = entry.recommended_here && !region_gated;

    // Per-row disclosure state: COLLAPSED BY DEFAULT (the honesty fix for the
    // old auto-expand). The chevron / identity / "Add key" button flip it open;
    // nothing forces it open at init, so the list reads calm at a glance.
    let expanded = RwSignal::new(false);

    // The set of canonical field names this row emits live, for the compact
    // capability summary line.
    // The CAPABILITY MATRIX at the top of the section now carries the per-field
    // capability + rain truth (Measured / Nowcast / Model / Forecast per cell), so
    // the eyebrow rain badge and the redundant "Covers: ..." summary line are
    // RETIRED from the row. The row keeps only the default-for-you dot + the one
    // status word; the rain nature still reads as PLAIN PROSE inside the expand.
    let rain_nature_prose = rain_badge_meaning(&entry.rain_nature).to_string();

    // Met.no is forecast-precip-ONLY (no current rain scalar); its expand carries
    // the synthetic-POP note. The badge already reads amber "Forecast only", so
    // the row needs no extra rain dot.
    let pop_synthetic = entry.pop_is_synthetic;

    // KEY TIER, now shown only in the EXPAND next to the actionable control (the
    // eyebrow key chip is retired so color stops being overloaded). One plain line
    // (the cost word + a short caution) plus a semantic slug for its tint.
    let (key_tier_slug, key_tier_label, key_caution) = match entry.key_tier.as_str() {
        "no_key" => ("free", "Free, no key needed", None),
        "free_key" => (
            "freekey",
            "Free key",
            Some("Heads up: the free key rides in the request URL."),
        ),
        // paid (and any unknown): treat as paid, the conservative honest default.
        _ => {
            let note = if kind == "weatherkit" {
                "Paid: an Apple Developer account, 99 dollars a year."
            } else {
                "Paid: a card on file with the provider."
            };
            ("paid", "Paid", Some(note))
        }
    };
    let key_tier_cls = format!("cloud-row__keytier cloud-row__keytier--{key_tier_slug}");

    // The Pirate upgrade line (verbatim from the catalog), shown in the expand so
    // a CONUS user understands its honest value without being misled.
    let upgrade_reason = entry.upgrade_reason.clone();

    // The catalog status enum for THIS kind, computed server-side from the shared
    // taxonomy fn (so the row word + /api/health agree). The homeowner word is
    // derived from it plus live ownership below.
    let status = entry.status.clone();
    // This kind's FRIENDLY label, the value the merge writes into field_sources.
    // Ownership is matched against THIS, never the raw kind id (a latent mismatch
    // the old state_view had), so "Feeding rain now" lights on the real owner.
    let friendly = crate::components::sources_form::friendly_source_name(&kind);

    // THE ONE STATUS WORD: the single calm homeowner word on the eyebrow, mapped
    // from the catalog status + which field this kind owns right now + the enable /
    // key / region flags. Reactive on live_owners so it tracks the chain. Returns
    // (semantic slug, word); the slug drives the single-color `cloud-word--<slug>`.
    let status_friendly = friendly.clone();
    let status_status = status.clone();
    let status_word_view = move || {
        let owners = live_owners.get();
        // Which fields does this source own right now (matched by friendly label)?
        let owns_rain = owners
            .get(rain_field_key())
            .map(|o| o == &status_friendly)
            .unwrap_or(false);
        let owned_other = capability_fields()
            .into_iter()
            .find(|(field, _)| {
                *field != rain_field_key()
                    && owners
                        .get(*field)
                        .map(|o| o == &status_friendly)
                        .unwrap_or(false)
            })
            .map(|(field, _)| friendly_field_word(field));
        let (slug, word) = homeowner_status_word(
            &status_status,
            enabled,
            keyless,
            region_gated,
            owns_rain,
            owned_other,
        );
        view! { <span class=format!("cloud-word cloud-word--{slug}")>{word}</span> }
    };

    // The CALM "who is covering" line for a standby / falling_through / watching
    // source: name the source that owns the rain reading right now so a quiet
    // backup reads as handled, never as the user's problem (the Rachio pattern).
    // Only meaningful when this source is enabled and NOT itself the rain owner.
    let cover_friendly = friendly.clone();
    let cover_status = status.clone();
    let cover_kind = kind.clone();
    // ENABLE control (lives on the row).
    let control = build_enable_control(
        &kind,
        keyless,
        enabled,
        region_gated,
        expanded,
        saving,
        set_keyless,
        enable_keyed,
        disable_keyed,
        on_edit_full,
    );

    let toggle_open = move |_| expanded.update(|v| *v = !*v);

    view! {
        <li
            class="cloud-row entity-stripe entity-stripe--source"
            class:cloud-row--enabled=enabled
            class:cloud-row--gated=region_gated
            class:cloud-row--open=move || expanded.get()
        >
            // The always-visible COMPACT row: stripe (css), chevron, identity
            // (name + tiny default dot + ONE rain badge + ONE status word),
            // control. Just two markers compete now: the rain badge (the one data
            // fact) and the status word (the one calm state), down from ~5 tinted
            // chips.
            <div class="cloud-row__main">
                <button
                    type="button"
                    class="cloud-row__chevron"
                    aria-expanded=move || if expanded.get() { "true" } else { "false" }
                    aria-label=format!("Show {title} details")
                    on:click=toggle_open
                >
                    <Icon name="chevron-right" size=16/>
                </button>

                <div
                    class="cloud-row__identity is-interactive"
                    role="button"
                    tabindex="0"
                    on:click=toggle_open
                >
                    <span class="cloud-row__title">{title.clone()}</span>
                    // The "Default for you" 6px dot, tooltip-only, replaces the
                    // wordy "On by default here" pill so the eyebrow stays calm.
                    {recommended.then(|| view! {
                        <span
                            class="cloud-row__default-dot"
                            title=REGION_DEFAULT_TOOLTIP
                            aria-label="Default for your region"
                        ></span>
                    })}
                    // The eyebrow rain badge is RETIRED: the capability matrix at
                    // the top of the section now carries the per-cell rain truth.
                    // Only the one status word remains on the row eyebrow.
                    <span class="cloud-row__status">{status_word_view}</span>
                </div>

                <div class="cloud-row__control">{control}</div>
            </div>

            // The expanded DETAILS body: only rendered when expanded. In order:
            // the rain nature as prose, the calm "who is covering" line for a
            // quiet/backup source, the real-time / localization / watering-risk
            // honesty lines, the Met.no caveat, the Pirate upgrade line, the key
            // tier next to the actionable control, and the keyed key-entry form.
            // The "Covers: ..." summary line is RETIRED (the matrix carries it).
            <Show when=move || expanded.get()>
                <div class="cloud-row__details">
                    // The rain nature as PLAIN PROSE (the per-row honest meaning).
                    <p class="cloud-row__nature">{rain_nature_prose.clone()}</p>

                    {
                        // Built fresh each time the expand renders (keeps the Show
                        // children Fn). Names who is covering rain for a calm
                        // standby/watching/falling-through source so the user sees
                        // the fall-through working, not a fault.
                        let owners = live_owners.get();
                        let rain_owner = owners.get(rain_field_key()).cloned();
                        let show = enabled
                            && !region_gated
                            && matches!(
                                cover_status.as_str(),
                                "standby" | "watching" | "falling_through"
                            )
                            && rain_owner.as_deref() != Some(cover_friendly.as_str());
                        show.then(|| {
                            let this_name = cloud_title(&cover_kind);
                            let line = match rain_owner {
                                Some(owner) => format!(
                                    "Right now {owner} owns your rain reading. {this_name} is ready to take over if it drops."
                                ),
                                None => format!(
                                    "{this_name} is on and ready. Turn on a rain source above so it has something to back up."
                                ),
                            };
                            view! { <p class="cloud-row__cover">{line}</p> }
                        })
                    }

                    {upgrade_reason.clone().map(|u| view! {
                        <p class="cloud-row__upgrade">{u}</p>
                    })}

                    <dl class="cloud-row__meta">
                        <div class="cloud-row__meta-row">
                            <dt>"Real-time"</dt>
                            <dd>{entry.real_time.clone()}</dd>
                        </div>
                        <div class="cloud-row__meta-row">
                            <dt>"Localization"</dt>
                            <dd>{entry.localization.clone()}</dd>
                        </div>
                    </dl>

                    <p class="cloud-row__risk"><em>{entry.watering_risk.clone()}</em></p>

                    {pop_synthetic.then(|| view! {
                        <p class="cloud-row__note">
                            "Its rain probability is a heuristic, not a measured forecast probability."
                        </p>
                    })}

                    // The key tier sits next to the actionable control, with its
                    // short caution (the eyebrow key chip is retired).
                    <p class=key_tier_cls.clone()>
                        <span class="cloud-row__keytier-label">"Key: "</span>
                        {key_tier_label}
                        {key_caution.map(|c| view! {
                            <span class="cloud-row__keytier-note">{c}</span>
                        })}
                    </p>

                    // The keyed key-entry form lives in the details (the row's
                    // "Add key" control flips `expanded` to reveal it).
                    <KeyedDetails
                        kind=kind.clone()
                        keyless=keyless
                        enabled=enabled
                        region_gated=region_gated
                        saving=saving
                        enable_keyed=enable_keyed
                        on_edit_full=on_edit_full
                    />
                </div>
            </Show>
        </li>
    }
}

/// Build the per-row enable affordance shown on the always-visible row: a single
/// Toggle for a keyless kind, or an "Add key" button (that flips the row open to
/// the key form) for a keyed kind. Region-gated NWS / NOAA MRMS gets a disabled
/// toggle. Extracted so the row's monomorphized view tree stays flat.
#[allow(clippy::too_many_arguments)]
fn build_enable_control(
    kind: &str,
    keyless: bool,
    enabled: bool,
    region_gated: bool,
    expanded: RwSignal<bool>,
    saving: RwSignal<bool>,
    set_keyless: Callback<(String, bool)>,
    enable_keyed: Callback<(String, String)>,
    disable_keyed: Callback<String>,
    on_edit_full: Option<Callback<String>>,
) -> impl IntoView {
    if keyless {
        // A keyless single toggle. Its checked state seeds from the catalog's
        // already_configured; on flip it splices/flips the source entry. A
        // region-gated kind outside the US is disabled with no checked state.
        let checked = RwSignal::new(enabled && !region_gated);
        let kind_owned = kind.to_string();
        // Mirror an external change (e.g. a reload) back onto the toggle, and
        // drive the PUT when the user flips it. We fire the callback only when
        // the toggle's value diverges from the persisted `enabled`, so a
        // reload-driven sync does not re-PUT.
        Effect::new(move |_| {
            let on = checked.get();
            if region_gated {
                return;
            }
            if on != enabled {
                set_keyless.run((kind_owned.clone(), on));
            }
        });
        let label = if region_gated {
            "US only, not available here".to_string()
        } else if enabled {
            "On".to_string()
        } else {
            "Turn on".to_string()
        };
        view! {
            <Toggle
                checked=checked
                label=label
                disabled=region_gated
            />
        }
        .into_any()
    } else {
        view! {
            <KeyedControl
                kind=kind.to_string()
                enabled=enabled
                expanded=expanded
                saving=saving
                enable_keyed=enable_keyed
                disable_keyed=disable_keyed
                on_edit_full=on_edit_full
            />
        }
        .into_any()
    }
}

/// The on-row control for a keyed kind. When enabled, a live toggle that
/// disables the source (keeping the stored key). When not enabled, an "Add key"
/// button that flips the row open to the key form (the form lives in the details
/// via `KeyedDetails`). A `needs_full_editor` kind (WeatherKit) routes its "Set
/// up" action to the host's full editor. We NEVER synthesize an enabled keyed
/// entry with a placeholder key.
#[component]
fn KeyedControl(
    kind: String,
    enabled: bool,
    expanded: RwSignal<bool>,
    saving: RwSignal<bool>,
    enable_keyed: Callback<(String, String)>,
    disable_keyed: Callback<String>,
    on_edit_full: Option<Callback<String>>,
) -> impl IntoView {
    let _ = (saving, enable_keyed);
    // Enabled path: a live toggle that, when switched off, disables the source
    // (keeps the key). When already on, flipping off calls disable_keyed.
    if enabled {
        let on_for_toggle = RwSignal::new(true);
        let kind_for_off = kind.clone();
        Effect::new(move |_| {
            let on = on_for_toggle.get();
            // Only react to a user turning a CURRENTLY-enabled keyed source OFF;
            // the ON direction needs a key, handled by the details form, so we
            // never let the bare toggle synthesize a keyless-on PUT.
            if !on {
                disable_keyed.run(kind_for_off.clone());
            }
        });
        return view! {
            <Toggle checked=on_for_toggle label="On".to_string()/>
        }
        .into_any();
    }

    // Not enabled. WeatherKit needs the full editor; route there.
    if needs_full_editor(&kind) {
        let kind_for_full = kind.clone();
        return match on_edit_full {
            Some(open_full) => view! {
                <Button
                    variant="primary"
                    on_click=Callback::new(move |_| open_full.run(kind_for_full.clone()))
                >
                    "Set up"
                </Button>
            }
            .into_any(),
            None => view! {
                <span class="cloud-row__control-note">"Set up in Devices"</span>
            }
            .into_any(),
        };
    }

    // Single-secret keyed kind: an "Add key" button that opens the row details
    // (where the SecretInput + Save lives).
    view! {
        <Button
            variant="primary"
            on_click=Callback::new(move |_| expanded.set(true))
        >
            "Add key"
        </Button>
    }
    .into_any()
}

/// The keyed key-entry form, rendered in the row DETAILS (revealed when the row
/// is expanded). For a single-secret kind (Pirate, OpenWeather) it shows the
/// SecretInput + the get-a-key link + one primary that writes key + enabled:true
/// in a single PUT. For a `needs_full_editor` kind (WeatherKit) it explains the
/// four-piece setup and points to the full editor (the on-row "Set up" button
/// opens it). Renders nothing for a keyless kind, an already-enabled keyed kind,
/// or a region-gated kind.
#[component]
fn KeyedDetails(
    kind: String,
    keyless: bool,
    enabled: bool,
    region_gated: bool,
    saving: RwSignal<bool>,
    enable_keyed: Callback<(String, String)>,
    on_edit_full: Option<Callback<String>>,
) -> impl IntoView {
    // Nothing to add for a keyless / already-on / gated row.
    if keyless || enabled || region_gated {
        return ().into_any();
    }

    // WeatherKit: explain the multi-field setup (the on-row "Set up" button is
    // the action). Falls back to a "use the full editor" note when unwired.
    if needs_full_editor(&kind) {
        return match on_edit_full {
            Some(_) => view! {
                <p class="cloud-row__note">
                    "WeatherKit needs a key id, team id, service id, and the .p8 key. "
                    "Set up opens the full editor so all four are entered together."
                </p>
            }
            .into_any(),
            None => view! {
                <p class="cloud-row__note">
                    "WeatherKit needs a key id, team id, service id, and the .p8 key. "
                    "Add it from the full weather-source editor under Devices."
                </p>
            }
            .into_any(),
        };
    }

    let secret = RwSignal::new(String::new());
    let key_url = get_a_key_url(&kind);
    let kind_for_enable = kind.clone();
    let on_enable: Callback<()> = Callback::new(move |()| {
        let s = secret.get_untracked();
        if s.trim().is_empty() {
            return;
        }
        enable_keyed.run((kind_for_enable.clone(), s));
    });
    let secret_label = "API key";

    view! {
        <div class="cloud-keyed__form">
            <label class="cloud-keyed__label">{secret_label}</label>
            <SecretInput
                value=secret
                on_input=Callback::new(move |v: String| secret.set(v))
                placeholder=secret_label
                autocomplete="off"
            />
            {key_url.map(|u| view! {
                <a
                    class="cloud-keyed__getkey"
                    href=u
                    target="_blank"
                    rel="noopener noreferrer external"
                >
                    "Get a key \u{2192}"
                </a>
            })}
            <Button
                variant="primary"
                disabled=Signal::derive(move || {
                    saving.get() || secret.get().trim().is_empty()
                })
                on_click=Callback::new(move |_| on_enable.run(()))
            >
                {move || if saving.get() { "Saving\u{2026}" } else { "Save key and enable" }}
            </Button>
        </div>
    }
    .into_any()
}

/// Read-only per-field BACKUP CHAIN. For each canonical reading it draws a
/// one-line arrow chain of the sources that can emit it, ordered by their TRUE
/// merge priority (a LOCAL station seeds ~100, ABOVE every cloud region
/// priority), with the LIVE owner bolded. The role is derived from POSITION, not
/// from "is it a station": index 0 is the HEAD (no suffix; a station head keeps
/// its teal tint as the strongest signal), and only the genuine terminal lowest
/// link of a multi-link chain reads "(backstop)". So a PRIMARY station reads
/// FIRST on every field it carries, never appended as a terminal "(last resort)".
/// Only when there is genuinely nothing behind the owner does a field read "no
/// backup". No new endpoint: the cloud links come from the catalog's enabled
/// entries by region_priority, the station links from the saved /api/config
/// station entries by their ~100 priority, joined to the snapshot's
/// field_sources for the live owner.
#[component]
fn ActiveChain(
    catalog: RwSignal<CloudCatalog>,
    stations: RwSignal<Vec<StationEntry>>,
    live_owners: RwSignal<std::collections::BTreeMap<String, String>>,
) -> impl IntoView {
    // The per-field arrow chains. Each field: ONE merged, priority-sorted
    // candidate list of the enabled cloud sources (region priority) PLUS any
    // local station that emits the field (its ~100 priority), so the station
    // lands at its TRUE rank, the head of every field it covers.
    let strip = move || {
        let owners = live_owners.get();
        let station_entries = stations.get();
        let enabled: Vec<CloudEntry> = catalog.with(|c| {
            c.cloud_sources
                .iter()
                .filter(|e| e.already_configured)
                .cloned()
                .collect()
        });

        if enabled.is_empty() && station_entries.is_empty() {
            return view! {
                <p class="cloud-chain__empty">
                    "No source is on yet. Turn one on above to build a backup chain."
                </p>
            }
            .into_any();
        }

        let rows: Vec<_> = capability_fields()
            .iter()
            .map(|(field, label)| {
                let owner_id = owners.get(*field).cloned();
                let links =
                    merge_field_chain(field, &enabled, &station_entries, owner_id.as_deref());
                view! {
                    <li class="cloud-chain__field">
                        <span class="cloud-chain__field-name">{*label}</span>
                        {render_chain(&links)}
                    </li>
                }
            })
            .collect();
        view! { <ul class="cloud-chain__strip">{rows}</ul> }.into_any()
    };

    view! {
        <section class="cloud-chain">
            <h3 class="cloud-chain__title">"Your backup chain"</h3>
            <p class="cloud-chain__lede">
                "Your own station leads for the readings it carries; cloud fills the rest and covers it if it goes quiet."
            </p>
            {strip}
        </section>
    }
}

/// One link in a per-field backup chain: a source name, its TRUE merge priority
/// (used to order the merged station+cloud candidate list), the honesty rank
/// tie-break, whether it is the live owner (bolded), and whether it is a local
/// station (so a station HEAD keeps its teal tint). The HEAD vs BACKSTOP role is
/// derived from POSITION at render time, never from `is_station`.
#[derive(Clone)]
struct ChainLink {
    name: String,
    priority: i32,
    honesty_rank: i32,
    is_owner: bool,
    is_station: bool,
}

/// Build the ONE merged, priority-sorted candidate list for a single field: every
/// enabled cloud source that emits it (keyed by `region_priority`) PLUS every
/// local station that carries it (its real ~100 priority). The list is sorted by
/// TRUE priority descending (honesty rank breaks ties), so a PRIMARY station
/// (priority ~100) lands at the HEAD of every field it covers, ahead of
/// MRMS/NWS/Open-Meteo, instead of being appended as a terminal "(last resort)".
/// The live owner (matched by id: a cloud id equals its kind; a station id equals
/// its config id) is flagged so the head bolds when it is live. This is the data
/// core of the position-aware chain fix, pure so it is unit-testable.
fn merge_field_chain(
    field: &str,
    enabled: &[CloudEntry],
    stations: &[StationEntry],
    owner_id: Option<&str>,
) -> Vec<ChainLink> {
    let mut links: Vec<ChainLink> = enabled
        .iter()
        .filter(|e| e.live_current_fields.iter().any(|f| f == field))
        .map(|e| ChainLink {
            name: cloud_title(&e.kind),
            priority: e.region_priority,
            honesty_rank: e.honesty_rank,
            is_owner: owner_id == Some(e.kind.as_str()),
            is_station: false,
        })
        .collect();

    for st in stations {
        if st.fields.iter().any(|f| f == field) {
            links.push(ChainLink {
                name: st.friendly_name.clone(),
                priority: st.priority,
                // A station has no cloud honesty rank; rank it above any cloud at
                // an equal priority (which never happens, a station is ~100) so a
                // tie still keeps it ahead.
                honesty_rank: i32::MAX,
                is_owner: owner_id == Some(st.id.as_str()),
                is_station: true,
            });
        }
    }

    // Higher priority wins, so sort descending; ties keep honesty order.
    links.sort_by(|a, b| {
        b.priority
            .cmp(&a.priority)
            .then(b.honesty_rank.cmp(&a.honesty_rank))
    });
    links
}

/// The POSITION-derived role suffix for a chain link: the "(backstop)" tag
/// attaches ONLY to the genuine terminal lowest-priority link of a MULTI-link
/// chain (`index == len - 1 && len > 1`), never to a link because it is a station
/// (the old `is_station` bug that read a PRIMARY station as "(last resort)" even
/// at the head). The head (index 0) and a single-link chain read with no suffix.
fn chain_link_suffix(index: usize, len: usize) -> &'static str {
    if len > 1 && index == len - 1 {
        " (backstop)"
    } else {
        ""
    }
}

/// Render a per-field chain as "A -> B -> C (backstop)", bolding the live owner
/// and tinting a station link teal. The role is POSITION-derived: index 0 is the
/// HEAD (no suffix, the strongest signal), and only the terminal lowest-priority
/// link of a multi-link chain reads "(backstop)". A single-link chain has no
/// suffix (it is both head and only option). An empty chain reads "no backup"
/// (nothing can emit the field and no station stands behind it).
fn render_chain(links: &[ChainLink]) -> impl IntoView {
    if links.is_empty() {
        return view! {
            <span class="cloud-chain__nobackup">"no backup"</span>
        }
        .into_any();
    }
    let last = links.len() - 1;
    let len = links.len();
    let parts: Vec<_> = links
        .iter()
        .enumerate()
        .map(|(i, link)| {
            let mut cls = String::from("cloud-chain__link");
            if link.is_owner {
                cls.push_str(" cloud-chain__link--owner");
            }
            if link.is_station {
                cls.push_str(" cloud-chain__link--station");
            }
            if i == 0 {
                cls.push_str(" cloud-chain__link--head");
            }
            let suffix = chain_link_suffix(i, len);
            let arrow = (i < last).then(|| {
                view! { <span class="cloud-chain__arrow">{" \u{2192} "}</span> }
            });
            view! {
                <span class=cls>{format!("{}{suffix}", link.name)}</span>
                {arrow}
            }
        })
        .collect();
    view! { <span class="cloud-chain__links">{parts}</span> }.into_any()
}

/// The wizard weather step's cloud section. First-class for EVERY user: it
/// renders the SAME interactive provider LIST as the Devices hub (the
/// `CloudWeatherServices` panel), so a no-hardware user lands with their region
/// default already on and a hardware user can turn a cloud provider on as a
/// complement or backup, both with the full honest data picture. It is NOT a
/// watered-down preview: it drives the same enable PUTs, so what the user sets
/// here is exactly what goes live.
///
/// `for_hardware` only reframes the explainer copy (local sensors outrank cloud,
/// cloud fills/backs up); the list is identical. The wizard's finalize step
/// still seeds the region keyless authority for a user who changes nothing, so
/// the default path stays "your region default is already on, click Next".
#[component]
pub fn CloudWeatherWizardSection(
    /// True when the user has (or is adding) a local weather station in this same
    /// step, so the cloud copy frames cloud as a complement/backup.
    #[prop(into, default = Signal::derive(|| false))]
    for_hardware: Signal<bool>,
) -> impl IntoView {
    view! {
        // The wizard hides the operational backup-chain visualization: at setup
        // there is no running engine with live owners to show, and the hero +
        // matrix + toggles already carry the capability truth. The chain lives
        // only in Settings/Devices (where a live engine has owners to display).
        <crate::components::settings::CloudWeatherServices
            for_hardware=for_hardware
            show_chain=false
        />
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::field_overrides::parse_field_name;

    /// A test CloudEntry builder so the unit tests stay terse.
    fn mk(kind: &str, rain_nature: &str, recommended: bool, configured: bool) -> CloudEntry {
        CloudEntry {
            kind: kind.to_string(),
            data_nature: "forecast".to_string(),
            rain_nature: rain_nature.to_string(),
            real_time: String::new(),
            localization: String::new(),
            watering_risk: String::new(),
            key_tier: "no_key".to_string(),
            emits_current_rain: true,
            pop_is_synthetic: false,
            honesty_rank: 0,
            irrigation_rank: 0,
            upgrade_reason: None,
            upgrade_available: false,
            live_current_fields: Vec::new(),
            field_natures: Vec::new(),
            recommended_here: recommended,
            region_appropriate: true,
            region_priority: 50,
            already_configured: configured,
            configured_present: configured,
            status: "watching".to_string(),
        }
    }

    #[test]
    fn solar_capability_chip_uses_the_canonical_merge_key() {
        // The Solar chip once shipped "solar_wm2" while the canonical merge key
        // (field_overrides::field_name for SolarWm2) is "solar_w_m2", so the chip
        // read as permanently dark even when Open-Meteo emitted solar. Sourcing
        // the key from WeatherField makes that impossible: the Solar chip must
        // resolve the canonical key the snapshot's field_sources uses.
        let fields = capability_fields();
        let solar = fields
            .iter()
            .find(|(_, label)| *label == "Solar")
            .expect("a Solar capability chip exists");
        assert_eq!(
            solar.0, "solar_w_m2",
            "the Solar chip must use the canonical merge key, not a drifted literal"
        );
        // The buggy literal must be gone entirely.
        assert!(
            !fields.iter().any(|(key, _)| *key == "solar_wm2"),
            "the stale 'solar_wm2' key must not appear"
        );
    }

    #[test]
    fn every_capability_key_is_a_canonical_weather_field() {
        // No capability key may be a hand-typed literal that drifts from the
        // merge layer: each must parse back to a real WeatherField (the inverse
        // of the field_name mapping the keys are now sourced from).
        for (key, label) in capability_fields() {
            assert!(
                parse_field_name(key).is_some(),
                "capability key '{key}' (chip '{label}') is not a canonical WeatherField name"
            );
        }
    }

    #[test]
    fn rain_field_key_resolves_the_canonical_rain_key() {
        // The lead line + chain own the rain reading under the canonical merge
        // key, never a local literal; it must parse back to a real WeatherField.
        let key = rain_field_key();
        assert_eq!(key, "rain_today_in", "rain owner key is the canonical one");
        assert!(parse_field_name(key).is_some());
    }

    #[test]
    fn rain_badge_is_honest_per_contract() {
        // THE one rain badge keys off rain_nature, never data_nature. Green for a
        // measured nature, blue for nowcast, amber for forecast, and NEVER the
        // word "live" anywhere.
        let (slug_obs, _, word_obs) = rain_badge("observation");
        assert_eq!(slug_obs, "measures");
        assert_eq!(word_obs, "Measures rain");

        let (slug_radar, _, word_radar) = rain_badge("radar_qpe");
        assert_eq!(slug_radar, "radar");
        assert_eq!(word_radar, "Radar-measured rain");

        let (slug_now, _, word_now) = rain_badge("nowcast");
        assert_eq!(slug_now, "nowcast");
        assert_eq!(word_now, "Nowcast");

        // Forecast (and the unknown fallback) read amber "Forecast only".
        for n in ["forecast", "", "totally-unknown"] {
            let (slug, _, word) = rain_badge(n);
            assert_eq!(slug, "forecast", "{n} is a forecast nature");
            assert_eq!(word, "Forecast only", "{n} reads forecast only");
        }
        // The word "live" never rides the badge for any nature.
        for n in ["observation", "radar_qpe", "nowcast", "forecast"] {
            let (_, _, word) = rain_badge(n);
            assert!(
                !word.to_lowercase().contains("live"),
                "the rain badge never says 'live' ({n})"
            );
        }
    }

    #[test]
    fn measured_natures_are_green() {
        // Only an Observation or a RadarQpe rain reads as measured (green); a
        // nowcast or a forecast does not.
        assert!(rain_is_measured_nature("observation"));
        assert!(rain_is_measured_nature("radar_qpe"));
        assert!(!rain_is_measured_nature("nowcast"));
        assert!(!rain_is_measured_nature("forecast"));
    }

    #[test]
    fn recommended_section_holds_authorities_and_pirate_upgrade() {
        // RECOMMENDED holds the region authorities AND a kind with an upgrade
        // (Pirate). Paid + region-inappropriate fall to the "More" fold.
        let mut pirate = mk("pirate_weather", "forecast", false, false);
        pirate.upgrade_reason = Some("free key sharpens temp and wind".to_string());
        pirate.upgrade_available = true;
        pirate.key_tier = "free_key".to_string();
        assert!(
            is_recommended_section(&pirate),
            "Pirate with an upgrade is recommended-section"
        );

        let nws = mk("nws", "observation", true, true);
        assert!(
            is_recommended_section(&nws),
            "a region authority is recommended"
        );

        let mut openweather = mk("openweather", "forecast", false, false);
        openweather.key_tier = "paid".to_string();
        assert!(
            !is_recommended_section(&openweather),
            "a paid non-upgrade source falls to the More fold"
        );

        // Met.no outside the Nordics: not recommended, no upgrade, region
        // inappropriate -> the More fold.
        let mut metno = mk("met_norway", "forecast", false, false);
        metno.region_appropriate = false;
        assert!(!is_recommended_section(&metno));
    }

    #[test]
    fn only_weatherkit_needs_the_full_editor() {
        // WeatherKit is the multi-field kind that cannot use the single-secret
        // one-click; the single-api_key kinds keep it.
        assert!(needs_full_editor("weatherkit"));
        assert!(!needs_full_editor("pirate_weather"));
        assert!(!needs_full_editor("openweather"));
        assert!(!needs_full_editor("nws"));
        assert!(!needs_full_editor("open_meteo"));
        assert!(!needs_full_editor("noaa_mrms"));
    }

    #[test]
    fn noaa_mrms_is_keyless() {
        // NOAA MRMS is a keyless cloud rain service (a single toggle enables it),
        // alongside Open-Meteo, NWS, and Met.no.
        assert!(is_keyless("noaa_mrms"));
        assert!(is_keyless("nws"));
        assert!(is_keyless("open_meteo"));
        assert!(is_keyless("met_norway"));
        assert!(!is_keyless("pirate_weather"));
        assert!(!is_keyless("openweather"));
        assert!(!is_keyless("weatherkit"));
    }

    #[test]
    fn snapshot_rain_word_never_says_live() {
        // The lead line's honest word for the live rain owner nature: Measured /
        // Radar / Forecast, never "live".
        assert_eq!(snapshot_rain_word("measured"), "Measured");
        assert_eq!(snapshot_rain_word("radar_qpe"), "Radar");
        assert_eq!(snapshot_rain_word("model"), "Forecast");
        for n in ["measured", "radar_qpe", "model", "unknown"] {
            assert!(!snapshot_rain_word(n).to_lowercase().contains("live"));
        }
    }

    #[test]
    fn friendly_field_word_uses_the_lowercase_capability_label() {
        // The "Feeding <field> now" word reads the shared capability label,
        // lowercased, so it tracks the merge key and never drifts.
        assert_eq!(friendly_field_word("wind_mph"), "wind");
        assert_eq!(friendly_field_word("air_temp_f"), "temp");
        assert_eq!(friendly_field_word("rain_today_in"), "rain");
        // An unknown key falls back to itself rather than panicking.
        assert_eq!(friendly_field_word("not_a_field"), "not_a_field");
    }

    #[test]
    fn homeowner_status_word_maps_the_contract_words() {
        // The OFF / gated / add-key cases are flag-driven, never the raw enum.
        assert_eq!(
            homeowner_status_word("offline", false, true, false, false, None),
            ("dim", "Off".to_string()),
            "a not-enabled keyless source reads Off, never the raw offline enum"
        );
        assert_eq!(
            homeowner_status_word("offline", false, false, false, false, None),
            ("warn", "Add key to turn on".to_string()),
            "a not-enabled keyed source reads Add key to turn on"
        );
        assert_eq!(
            homeowner_status_word("offline", false, true, true, false, None),
            ("dim", "Not in your area".to_string()),
            "a region-gated source reads Not in your area"
        );

        // ACTIVE: the word names the exact field it is feeding.
        assert_eq!(
            homeowner_status_word("active", true, true, false, true, None),
            ("owner", "Feeding rain now".to_string())
        );
        assert_eq!(
            homeowner_status_word("active", true, true, false, false, Some("wind".to_string())),
            ("owner", "Feeding wind now".to_string())
        );

        // The calm non-owning states.
        assert_eq!(
            homeowner_status_word("standby", true, true, false, false, None),
            ("neutral", "On, standby".to_string())
        );
        assert_eq!(
            homeowner_status_word("watching", true, true, false, false, None),
            ("neutral", "Watching, no rain".to_string())
        );
        assert_eq!(
            homeowner_status_word("falling_through", true, true, false, false, None),
            ("neutral", "Quiet here right now".to_string())
        );

        // The ONLY fault word: a genuinely-unreachable ENABLED source.
        assert_eq!(
            homeowner_status_word("offline", true, true, false, false, None),
            ("fault", "Not reachable right now".to_string())
        );

        // No calm state ever leaks "stale" / "offline" / "error" to a homeowner.
        for status in ["active", "watching", "standby", "falling_through"] {
            let (_, word) = homeowner_status_word(status, true, true, false, false, None);
            let lower = word.to_lowercase();
            assert!(
                !lower.contains("stale") && !lower.contains("offline") && !lower.contains("error"),
                "{status} leaked a raw fault word: {word}"
            );
        }
    }

    #[test]
    fn watching_and_falling_through_render_equally_calm() {
        // CONGRUENCE (the acceptable divergence the cloud-status spec allows): for
        // the MRMS-quiet case the catalog yields `watching` while /api/health may
        // still yield `falling_through` via its prior-owner OBSERVATION history (a
        // source that WAS the owner and is still emitting). That is fine ONLY IF the
        // homeowner-facing WORD is equally calm for both, so the user never sees a
        // discrepancy. Both must render with the SAME neutral tone (different calm
        // wording is fine; a `watching` reading and a `falling_through` reading must
        // never look like one is a problem and the other is not).
        let (watching_slug, _) = homeowner_status_word("watching", true, true, false, false, None);
        let (falling_slug, _) =
            homeowner_status_word("falling_through", true, true, false, false, None);
        assert_eq!(
            watching_slug, falling_slug,
            "watching and falling_through must share the same calm slug so the two surfaces read equally calm"
        );
        assert_eq!(
            watching_slug, "neutral",
            "and that shared calm slug is the neutral (non-fault) tone"
        );
    }

    // ----- The LOCAL station chain inputs (spec 5) -----

    #[test]
    fn lan_station_kinds_are_the_real_stations_only() {
        // A LAN station the merge ranks at ~100; a cloud kind or a generic mapping
        // source is NOT a station and must not seed a station chain link.
        for k in [
            "tempest_udp",
            "tempest_ws",
            "ecowitt_local",
            "ecowitt_gw_poll",
            "davis_wll",
        ] {
            assert!(is_lan_station_kind(k), "{k} is a LAN station");
        }
        for k in [
            "open_meteo",
            "nws",
            "noaa_mrms",
            "pirate_weather",
            "mqtt",
            "ha_passthrough",
        ] {
            assert!(!is_lan_station_kind(k), "{k} is not a LAN station");
        }
    }

    #[test]
    fn station_field_keys_cover_the_station_capability_set() {
        // A full station carries the whole capability set; the Tempest/Ecowitt
        // families also carry lightning, while Davis omits it. Every key must be a
        // canonical WeatherField so a station chain link lands on a real field.
        let tempest = station_field_keys("tempest_udp");
        assert!(tempest.contains(&"wind_mph") && tempest.contains(&"rain_today_in"));
        assert!(
            tempest.contains(&"lightning_count"),
            "a Tempest carries lightning"
        );
        let davis = station_field_keys("davis_wll");
        assert!(davis.contains(&"wind_mph"));
        assert!(
            !davis.contains(&"lightning_count"),
            "Davis WLL has no lightning channel"
        );
        for kind in ["tempest_udp", "ecowitt_gw_poll", "davis_wll"] {
            for key in station_field_keys(kind) {
                assert!(
                    parse_field_name(key).is_some(),
                    "station field key '{key}' ({kind}) is a real WeatherField"
                );
            }
        }
        // An unknown kind reads empty (guarded by is_lan_station_kind in callers).
        assert!(station_field_keys("open_meteo").is_empty());
    }

    // ----- The position-aware backup chain (spec 5, the full fix) -----

    /// A terse station-entry builder for the chain tests.
    fn st(id: &str, priority: i32, fields: &[&str]) -> StationEntry {
        StationEntry {
            id: id.to_string(),
            friendly_name: format!("Station {id}"),
            priority,
            fields: fields.iter().map(|s| s.to_string()).collect(),
        }
    }

    /// A cloud entry that emits a single field at a given region priority.
    fn cloud_for(kind: &str, field: &str, region_priority: i32, honesty_rank: i32) -> CloudEntry {
        let mut e = mk(kind, "forecast", false, true);
        e.live_current_fields = vec![field.to_string()];
        e.region_priority = region_priority;
        e.honesty_rank = honesty_rank;
        e
    }

    #[test]
    fn primary_station_leads_the_chain_for_every_field_it_covers() {
        // THE bug fix (spec 5): a PRIMARY station (priority 100) must read FIRST on
        // a field it carries, ahead of any cloud, even when it is NOT the live owner
        // this instant (a wind-shadowed Tempest whose wind is currently pinned to a
        // cloud). The merged candidate list sorts by TRUE priority, so the station
        // lands at index 0 (the head), never appended as a terminal link.
        let clouds = vec![
            cloud_for("noaa_mrms", "wind_mph", 75, 55),
            cloud_for("open_meteo", "wind_mph", 50, 20),
        ];
        let stations = vec![st("tempest_yard", 100, &["wind_mph"])];
        // owner is the cloud (the station does not live-own wind right now).
        let chain = merge_field_chain("wind_mph", &clouds, &stations, Some("open_meteo"));
        assert_eq!(chain.len(), 3, "station + two clouds all emit wind");
        assert!(chain[0].is_station, "the station leads at index 0");
        assert_eq!(chain[0].priority, 100);
        // Position-derived role: the head reads NO suffix even though it is a
        // station (the old is_station bug would have suffixed it).
        assert_eq!(chain_link_suffix(0, chain.len()), "");
        // The genuine terminal lowest link reads "(backstop)".
        assert_eq!(
            chain_link_suffix(chain.len() - 1, chain.len()),
            " (backstop)"
        );
        // The clouds follow in priority order behind the station.
        assert_eq!(chain[1].name, cloud_title("noaa_mrms"));
        assert_eq!(chain[2].name, cloud_title("open_meteo"));
    }

    #[test]
    fn live_owner_is_flagged_at_its_true_position() {
        // When the station IS the live owner, it is both the head AND bolded; no
        // suffix. When a cloud owns the field, the station still leads by priority
        // but is not bolded (is_owner false), and the cloud owner is bolded in place.
        let clouds = vec![cloud_for("open_meteo", "air_temp_f", 50, 20)];
        let stations = vec![st("tempest_yard", 100, &["air_temp_f"])];

        let owned_by_station =
            merge_field_chain("air_temp_f", &clouds, &stations, Some("tempest_yard"));
        assert!(owned_by_station[0].is_station && owned_by_station[0].is_owner);
        assert_eq!(chain_link_suffix(0, owned_by_station.len()), "");

        let owned_by_cloud =
            merge_field_chain("air_temp_f", &clouds, &stations, Some("open_meteo"));
        assert!(owned_by_cloud[0].is_station && !owned_by_cloud[0].is_owner);
        assert!(
            owned_by_cloud[1].is_owner,
            "the cloud owner is bolded in place"
        );
    }

    #[test]
    fn single_link_chain_has_no_backstop_suffix() {
        // A field with exactly one candidate (a lone cloud, no station) is both head
        // and only option: no "(backstop)" suffix, since there is nothing behind it.
        let clouds = vec![cloud_for("open_meteo", "uv_index", 50, 20)];
        let chain = merge_field_chain("uv_index", &clouds, &[], Some("open_meteo"));
        assert_eq!(chain.len(), 1);
        assert_eq!(chain_link_suffix(0, chain.len()), "");
    }

    #[test]
    fn chain_link_suffix_is_position_derived_not_station_derived() {
        // The role is POSITION-derived: only the terminal link of a multi-link chain
        // is the backstop; index 0 (even a station) and a single-link chain are not.
        assert_eq!(chain_link_suffix(0, 1), "");
        assert_eq!(chain_link_suffix(0, 3), "");
        assert_eq!(chain_link_suffix(1, 3), "");
        assert_eq!(chain_link_suffix(2, 3), " (backstop)");
    }

    #[test]
    fn station_only_field_reads_as_its_sole_head() {
        // A field only the station carries (e.g. lightning, no cloud emits it) is a
        // single-link chain headed by the station, no suffix, never "no backup".
        let stations = vec![st("tempest_yard", 100, &["lightning_count"])];
        let chain = merge_field_chain("lightning_count", &[], &stations, None);
        assert_eq!(chain.len(), 1);
        assert!(chain[0].is_station);
        assert_eq!(chain_link_suffix(0, chain.len()), "");
    }
}
