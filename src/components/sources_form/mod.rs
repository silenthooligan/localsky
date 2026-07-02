// Reusable source add/edit form, shared by the Sensors hub (inline,
// no-navigation add/edit), the Settings raw editor, and the setup wizard.
// The panel owns only the draft signals + the assembled entry; the caller
// decides how to persist (config PUT for the hub/settings, wizard-draft
// PUT for setup), so the same widget serves every surface.

use leptos::prelude::*;

use crate::components::ui::{Button, FormField, Panel};
use crate::docs::doc_url;

pub mod field_map_forms;
pub mod field_schema;
pub mod soil_forms;
pub use field_map_forms::{
    DeviceFieldMapEditor, HaFieldMapEditor, WeatherFieldMapEditor, WEATHER_FIELD_OPTIONS,
};
pub use field_schema::{source_fields, SourceConfigForm};
pub use soil_forms::{EcowittSoilCalibration, MqttSoilSubscriptions};

/// Short, vendor-specific soil setup hint for the source kinds that carry soil
/// data. Returns nothing for kinds without a soil path, so the form stays lean.
/// Each hint names the exact LocalSky steps for that path and links to the
/// getting-started doc, so a rookie never has to guess at the wiring. Kept in
/// lockstep with `first-soil-sensor.md`.
fn soil_path_hint(kind: &str) -> impl IntoView {
    let text: Option<&'static str> = match kind {
        "ecowitt_gw_poll" => Some(
            "Ecowitt soil path: LocalSky polls your gateway over the LAN (no cloud). \
             Find the gateway's IP in the Ecowitt WS View app or your router, and pair \
             your soil probes to the gateway there. Set that IP as the host below; the \
             probes then appear as channels under Settings, Sensors, ready to bind to a zone.",
        ),
        "ecowitt_local" => Some(
            "Ecowitt push path: this is a push source, so set the listen path in the Ingest path \
             field above (and a Shared secret if you want one), then point the gateway's \
             Customized server (Ecowitt protocol) at that path. Probes paired to the gateway then \
             arrive as soil channels under Settings, Sensors, ready to bind to a zone.",
        ),
        "mqtt" => Some(
            "MQTT soil path: have your probe publish soil moisture to a topic, then add a \
             soil subscription below with that topic, the JSON field (if the payload is an \
             object), and the zone it measures. Finish by picking this source's channel as \
             the zone's soil sensor in the zone editor.",
        ),
        "ha_passthrough" => Some(
            "Home Assistant soil path: fill the Home Assistant URL and Long-lived token fields \
             above to bridge HA. Soil probes HA already owns are then bound straight from the \
             zone editor (Settings, Zones, pick the zone, Soil moisture sensor): they list as \
             ha:sensor.<entity>. The field_map (in the advanced JSON box) is for weather fields, \
             not soil.",
        ),
        _ => None,
    };
    text.map(|t| {
        view! {
            <p class="sensors-section__hint" style="margin-bottom: var(--space-3)">
                {t}
                " "
                <a href=doc_url("first-soil-sensor") target="_blank" rel="noopener noreferrer"
                    style="color: var(--accent)">
                    "Add your first soil sensor →"
                </a>
            </p>
        }
    })
}

/// A concise "what this is + how to set it up" line shown ABOVE the connection
/// form for each kind, with a direct link to the right guide. Answers the
/// up-front questions ("do I need an IP? a key? how do I find it?") so a
/// beginner isn't guessing. Kept short and direct (the field helptext carries
/// the per-field detail).
fn connection_hint(kind: &str) -> impl IntoView {
    let (text, doc): (Option<&'static str>, &'static str) = match kind {
        "tempest_udp" => (
            Some("Tempest broadcasts its readings over your LAN on its own -- there is no device IP to enter. Add it and leave the listen address at the default; values appear on the Sensors hub within a minute."),
            "sensors",
        ),
        "tempest_ws" => (
            Some("Tempest cloud needs a personal access token from tempestwx.com (Settings -> Data Authorizations) plus your station id."),
            "sensors",
        ),
        "davis_wll" => (
            Some("Enter the WeatherLink Live device's own IP (from your router). LocalSky polls it directly on the LAN -- no cloud account."),
            "sensors",
        ),
        "ecowitt_gw_poll" => (
            Some("Find the gateway's IP in the Ecowitt WS View app, then enter it below. LocalSky reads its local API -- no Ecowitt cloud account needed."),
            "soil-sensors",
        ),
        "ecowitt_local" => (
            Some("A PUSH source: set the ingest path below, then point the gateway's Customized server (Ecowitt protocol) at LocalSky."),
            "soil-sensors",
        ),
        "open_meteo" | "nws" | "met_norway" => (
            Some("Free, no API key. Pulls live conditions for your location, no station needed."),
            "forecast",
        ),
        "openweather" | "pirate_weather" | "weatherkit" => (
            Some("Needs a free or paid sign-in key from the provider. Pulls live conditions for your location, no station needed."),
            "forecast",
        ),
        "synoptic" => (
            Some("Needs a free API token from synopticdata.com. Pulls the nearest real station's live conditions (wind, pressure, temperature, humidity), no station of your own needed."),
            "forecast",
        ),
        "mqtt" => (
            Some("Subscribe to topics your devices publish. Add the broker below, then map topics to readings (soil/weather)."),
            "sensors",
        ),
        "prometheus" | "influxdb" => (
            Some("Pull readings you already store in your monitoring stack: add the server below, then map queries to readings in the advanced config."),
            "sensors",
        ),
        "ha_passthrough" => (
            Some("Bridge readings from Home Assistant: add the HA URL + a long-lived token, then map HA entities to readings in Field mappings."),
            "home-assistant",
        ),
        _ => (None, "sensors"),
    };
    text.map(|t| {
        view! {
            <p class="sensors-section__hint" style="margin-bottom: var(--space-3)">
                {t}
                " "
                <a href=doc_url(doc) target="_blank" rel="noopener noreferrer"
                    style="color: var(--accent); white-space: nowrap">
                    "Setup guide \u{2192}"
                </a>
            </p>
        }
    })
}

/// The source kinds the form offers, as (value, label) pairs.
pub fn kind_options() -> Vec<(String, String)> {
    [
        ("tempest_udp", "Tempest UDP"),
        ("tempest_ws", "Tempest cloud"),
        ("davis_wll", "Davis WLL"),
        ("ecowitt_local", "Ecowitt LAN (push)"),
        ("ecowitt_gw_poll", "Ecowitt gateway (poll)"),
        ("ambient_weather", "AmbientWeather"),
        ("netatmo", "Netatmo"),
        ("yolink", "YoLink"),
        ("lacrosse", "LaCrosse View"),
        ("tuya_cloud", "Tuya / RainPoint"),
        ("open_meteo", "Open-Meteo"),
        ("nws", "NWS (US)"),
        ("met_norway", "Met.no"),
        ("synoptic", "Synoptic / MesoWest"),
        ("openweather", "OpenWeather"),
        ("pirate_weather", "PirateWeather"),
        ("weatherkit", "Apple WeatherKit"),
        ("mqtt", "MQTT"),
        ("http_webhook", "HTTP webhook"),
        ("rest_poll", "REST poll (any API)"),
        ("prometheus", "Prometheus"),
        ("influxdb", "InfluxDB"),
        ("ha_passthrough", "HA passthrough"),
        // blitzortung is intentionally NOT offered: Blitzortung.org requires
        // explicit permission before using their community lightning feed, which
        // we do not yet have. The adapter + config exist, but the source stays
        // out of the picker (like the deferred esphome_native controller) so it
        // cannot go live until permission is granted; re-add this line then.
        ("demo_replay", "Demo"),
    ]
    .into_iter()
    .map(|(v, l)| (v.to_string(), l.to_string()))
    .collect()
}

/// The kind picker organized by the user's MENTAL MODEL instead of a flat list
/// of 24 protocol names. Each group carries a short caption answering "is this
/// me?" and lists its kinds in onboarding order (the recommended zero-config
/// pick first in its group). This is the ordering the grouped picker renders;
/// `kind_options()` stays the canonical flat list (tests + other surfaces).
///
/// Returns `(group title, group caption, [kind value, ...])`. Every kind in
/// `kind_options()` appears in exactly one group; the grouped picker asserts
/// that coverage in a test so a newly-added kind can't silently vanish from the
/// UI.
pub fn kind_groups() -> Vec<(&'static str, &'static str, Vec<&'static str>)> {
    vec![
        (
            "Cloud weather (no hardware)",
            "Live conditions for your exact spot, pulled from weather services. No station to buy. Free options need no account. Start here if you have no weather hardware.",
            // open_meteo FIRST: the recommended zero-config default.
            vec![
                "open_meteo",
                "nws",
                "met_norway",
                "synoptic",
                "openweather",
                "pirate_weather",
                "weatherkit",
            ],
        ),
        (
            "Local weather station",
            "A station or gateway on your own network. LocalSky talks to it directly (no cloud account).",
            vec![
                "tempest_udp",
                "tempest_ws",
                "davis_wll",
                "ecowitt_local",
                "ecowitt_gw_poll",
            ],
        ),
        (
            "Cloud weather station",
            "Your station's data, read through the vendor's cloud using your account.",
            vec!["ambient_weather", "netatmo", "lacrosse"],
        ),
        (
            "Sensors & data bridges",
            "Pull readings from systems you already run: MQTT, Home Assistant, a monitoring stack, or smart-home clouds.",
            vec![
                "mqtt",
                "http_webhook",
                "rest_poll",
                "prometheus",
                "influxdb",
                "ha_passthrough",
                "yolink",
                "tuya_cloud",
            ],
        ),
        (
            "Advanced",
            "Specialized feeds for specific extras.",
            // blitzortung omitted pending Blitzortung.org usage permission (see kind_options).
            vec!["demo_replay"],
        ),
    ]
}

/// True for the one recommended zero-config default (Open-Meteo): free, no API
/// key, works the moment it's added. The picker marks it so a hardware-less user
/// knows exactly where to start.
pub fn is_recommended_default(kind: &str) -> bool {
    kind == "open_meteo"
}

/// A plain-language "what this does" line for a kind, written for someone who is
/// NOT sure what they're looking at. Combines (a) what it provides (7-day
/// forecast / live current conditions / soil moisture / lightning / ...) with
/// (b) the one fact that decides whether they can use it right now: FREE & no
/// key, NEEDS AN API KEY/ACCOUNT, or works-immediately. Kept to a sentence or
/// two so it stays scannable under the picker.
pub fn kind_blurb(kind: &str) -> &'static str {
    match kind {
        // ---- Weather forecast (cloud, no hardware) ----
        "open_meteo" => {
            "Free 7-day + hourly forecast for your location. No API key, works immediately. \
             The best choice if you don't have hardware."
        }
        "nws" => {
            "Free US (National Weather Service) forecast for your location. No API key; \
             just needs a contact string to identify you to the free API."
        }
        "met_norway" => {
            "Free worldwide forecast from MET Norway. No API key; just needs a contact \
             string to identify you to the free API."
        }
        "synoptic" => {
            "Live current conditions from the nearest real station in Synoptic's dense \
             mesonet (MesoWest). Needs a free API token from synopticdata.com; reports \
             wind, pressure, temperature and humidity, no rain gauge."
        }
        "openweather" => {
            "Worldwide forecast from OpenWeather. Needs a free API key from your \
             openweathermap.org account."
        }
        "pirate_weather" => {
            "Worldwide forecast from Pirate Weather (a Dark Sky-style API). Needs a free \
             API key from your pirateweather.net account."
        }
        "weatherkit" => {
            "Apple's WeatherKit forecast. Needs an Apple Developer account and a signing \
             key (~5-10 min of setup); not free unless you already have the account."
        }
        // ---- Local weather station (on your network) ----
        "tempest_udp" => {
            "Live current conditions from a WeatherFlow Tempest on your LAN. Free, no key, \
             no device IP, the Tempest broadcasts on its own and readings appear within a minute."
        }
        "tempest_ws" => {
            "Live current conditions from your Tempest via WeatherFlow's cloud. Needs a \
             free personal token + your station id (use this only if the LAN option won't reach the hub)."
        }
        "davis_wll" => {
            "Live current conditions (plus soil/leaf stations) from a Davis WeatherLink \
             Live on your LAN. Free, no cloud account, just the device's IP."
        }
        "ecowitt_local" => {
            "Live conditions PLUS soil moisture + leaf wetness from an Ecowitt gateway, \
             pushed to LocalSky over your LAN. Free, no cloud account; you point the gateway at LocalSky."
        }
        "ecowitt_gw_poll" => {
            "Live conditions PLUS soil moisture + leaf wetness from an Ecowitt gateway, \
             read from its local API. Free, no cloud account, just the gateway's IP."
        }
        // ---- Cloud weather station (your account) ----
        "ambient_weather" => {
            "Your Ambient Weather station's readings, via the Ambient cloud. Needs the \
             API + application keys from your ambientweather.net account."
        }
        "netatmo" => {
            "Your Netatmo station's readings, via the Netatmo cloud. Needs an app + OAuth \
             credentials from your Netatmo developer account."
        }
        "lacrosse" => {
            "Your La Crosse View station's readings, via the La Crosse cloud. Signs in \
             with your La Crosse View account email + password."
        }
        // ---- Sensors & data bridges ----
        "mqtt" => {
            "Subscribe to topics your devices already publish over MQTT, mapping them to weather \
             fields or per-zone soil moisture. Free; needs your broker's address."
        }
        "http_webhook" => {
            "Let any device POST readings to a LocalSky URL, then map the JSON to weather \
             or soil fields. Free; no account, you choose the path."
        }
        "rest_poll" => {
            "Poll any HTTP/JSON API on a schedule and map fields to readings (weather or \
             soil). Free; bring whatever URL + key the API itself needs."
        }
        "prometheus" => {
            "Pull readings you already store in Prometheus via PromQL queries. Free; needs \
             your Prometheus server URL."
        }
        "influxdb" => {
            "Pull readings you already store in InfluxDB via InfluxQL queries. Free; needs \
             your InfluxDB server URL (and a token for v2)."
        }
        "ha_passthrough" => {
            "Bridge sensors Home Assistant already owns (weather entities or soil probes). \
             Needs your HA URL + a long-lived token."
        }
        "yolink" => {
            "Read YoLink temperature/humidity/soil sensors via the YoLink cloud. Needs a \
             UAID + Secret Key created in the YoLink phone app."
        }
        "tuya_cloud" => {
            "Read Tuya / Smart Life / RainPoint sensors via the Tuya cloud. Needs an \
             Access ID + Secret from a Tuya IoT (iot.tuya.com) cloud project."
        }
        // ---- Advanced ----
        "blitzortung" => {
            "Nearby lightning strikes from the Blitzortung.org community network. Free, no \
             key; opt-in, display-only (never a safety feature)."
        }
        "demo_replay" => {
            "Synthetic demo data for trying LocalSky without any real source. No setup; \
             not for production use."
        }
        _ => "Provides weather data to LocalSky.",
    }
}

/// Sensible default current-conditions priority for a newly-added source,
/// keyed by kind so a fresh setup follows the convention without manual tuning:
/// a dedicated weather station outranks a local sensor gateway, which outranks a
/// cloud service. Per-field merge means a lower-priority partial source still
/// contributes the fields a higher source doesn't, so these only break ties on
/// SHARED fields (e.g. a station + a barometer both reporting pressure).
pub fn default_priority_for_kind(kind: &str) -> i32 {
    match kind {
        // Dedicated LAN weather stations reporting a full observation set.
        "tempest_udp" | "tempest_ws" | "davis_wll" | "ecowitt_local" => 100,
        // Local sensor gateways / generic local feeds: often partial (soil,
        // barometer, a few channels) so they sit just under a full station.
        "ecowitt_gw_poll" | "mqtt" | "http_webhook" | "rest_poll" | "prometheus" | "influxdb"
        | "ha_passthrough" | "yolink" | "tuya_cloud" => 90,
        // Synoptic is a REAL nearest-station observation (denser than NWS), so it
        // seeds above the model/forecast cloud tier, matching region.rs's
        // station-authority rank (70). It is not a forecast kind, so
        // normalize_new_cloud_sources leaves this seed as the final priority.
        "synoptic" => 70,
        // Cloud current/forecast services back up the local sensors.
        "open_meteo" | "nws" | "met_norway" | "openweather" | "pirate_weather" | "weatherkit"
        | "ambient_weather" | "netatmo" | "lacrosse" => 50,
        // Single-purpose / fallback feeds.
        "blitzortung" => 50,
        "demo_replay" => 10,
        _ => 50,
    }
}

/// Icon registry name (ui::Icon) for a source kind.
pub fn kind_icon(kind: &str) -> &'static str {
    match kind {
        "tempest_udp" | "tempest_ws" => "wind",
        "davis_wll" => "thermometer",
        "open_meteo" | "nws" | "openweather" | "pirate_weather" | "met_norway" | "weatherkit"
        | "synoptic" => "cloud",
        "ecowitt_local" | "ecowitt_gw_poll" => "sources",
        "mqtt" => "download",
        "http_webhook" => "download",
        "rest_poll" => "cloud",
        "prometheus" => "sources",
        "influxdb" => "sources",
        "ha_passthrough" => "home",
        "ambient_weather" => "cloud-sun",
        "netatmo" => "cloud-drizzle",
        "yolink" => "sources",
        "lacrosse" => "cloud-sun",
        "tuya_cloud" => "zap",
        "blitzortung" => "zap",
        "demo_replay" => "play",
        _ => "sources",
    }
}

pub fn kind_pretty(kind: &str) -> &'static str {
    match kind {
        "tempest_udp" => "Tempest UDP (LAN)",
        "tempest_ws" => "Tempest WebSocket (cloud)",
        "davis_wll" => "Davis WeatherLink Live",
        "open_meteo" => "Open-Meteo",
        "nws" => "NWS (US weather service)",
        "noaa_mrms" => "NOAA MRMS",
        "openweather" => "OpenWeather",
        "pirate_weather" => "Pirate Weather",
        "met_norway" => "Met.no (Norway)",
        "synoptic" => "Synoptic Data (MesoWest)",
        "weatherkit" => "Apple WeatherKit",
        "ecowitt_local" => "Ecowitt local POST (push)",
        "ecowitt_gw_poll" => "Ecowitt gateway local-API poll",
        "mqtt" => "MQTT subscribe",
        "http_webhook" => "HTTP webhook receiver",
        "rest_poll" => "Generic REST API poll",
        "prometheus" => "Prometheus instant-query",
        "influxdb" => "InfluxDB (InfluxQL)",
        "ha_passthrough" => "Home Assistant passthrough",
        // The cloud weather STATION tier: the user's OWN station, cloud-routed.
        // Named as a personal station so the kind picker / provenance never
        // reads as an anonymous "cloud" service.
        "ambient_weather" => "Ambient Weather (your station)",
        "netatmo" => "Netatmo (your station)",
        "yolink" => "YoLink cloud",
        "lacrosse" => "La Crosse (your station)",
        "tuya_cloud" => "Tuya / Smart Life cloud",
        "blitzortung" => "Blitzortung community lightning",
        "demo_replay" => "Demo replay (synthetic)",
        _ => "Unknown",
    }
}

/// Short, plain-language summary of what a source kind actually brings to
/// LocalSky, derived from each adapter's declared WeatherField set + caps
/// (see `src/sources/*` and `ports/weather_source.rs`). Surfaced in the kind
/// picker so a user knows whether a device is weather-only, forecast-only, or
/// a MIXED device (weather + soil moisture + leaf wetness, like Ecowitt)
/// BEFORE they choose it. This is labeling only; every kind stays onboardable.
pub fn kind_caps(kind: &str) -> &'static str {
    match kind {
        // Mixed LAN stations/gateways: full weather observation set PLUS
        // native soil-moisture channels and leaf wetness.
        "ecowitt_local" | "ecowitt_gw_poll" => "Weather + Soil moisture + Leaf wetness",
        // Davis WLL exposes weather plus soil/leaf sensor stations.
        "davis_wll" => "Weather + soil/leaf",
        // Tempest: full local weather station, no soil/leaf.
        "tempest_udp" => "Weather station (local)",
        "tempest_ws" => "Weather station (cloud)",
        // Cloud-hosted personal weather stations.
        "ambient_weather" | "netatmo" | "lacrosse" => "Weather station (cloud)",
        // Cloud weather services: live current conditions plus forecast for
        // the configured location, no live yard sensors.
        "open_meteo" | "nws" | "met_norway" | "openweather" | "pirate_weather" | "weatherkit" => {
            "Live conditions + forecast (cloud)"
        }
        // Synoptic is a real nearest-station observation only (no forecast, and
        // its requested vars carry no rain gauge).
        "synoptic" => "Live conditions, no rain (cloud station)",
        // Bridges: capabilities follow whatever device/entity you map.
        "tuya_cloud" | "mqtt" | "ha_passthrough" => "Weather and/or soil (depends on device)",
        // Single-purpose feeds.
        "blitzortung" => "Lightning only",
        "demo_replay" => "Synthetic demo data",
        // Generic ingest: depends entirely on what you point it at.
        "http_webhook" | "rest_poll" | "prometheus" | "influxdb" | "yolink" => {
            "Weather and/or soil (depends on device)"
        }
        _ => "Weather data",
    }
}

/// Friendly display name for a CLOUD WEATHER SERVICE kind, written for a
/// non-expert: it answers "what is NWS / OpenWeather?" in plain words. This is
/// the name shown next to the tier chip in the per-field picker and the wizard,
/// so a user who has never heard the acronym still understands. Returns the kind
/// string itself for non-cloud-service kinds (the per-field picker only calls
/// this for the cloud services it lists).
pub fn cloud_service_name(kind: &str) -> &'static str {
    match kind {
        "open_meteo" => "Open-Meteo",
        "nws" => "NWS (US National Weather Service)",
        "openweather" => "OpenWeather",
        "met_norway" => "Met.no (Norwegian Meteorological Institute)",
        "synoptic" => "Synoptic Data (MesoWest station network)",
        "weatherkit" => "WeatherKit (Apple)",
        "pirate_weather" => "Pirate Weather",
        "noaa_mrms" => "NOAA MRMS",
        // The cloud weather STATION tier: the user's OWN station routed through
        // the vendor cloud, so name it as a personal station, not an anonymous
        // service. Without these arms friendly_source_name fell through to the
        // generic "Cloud weather service" label / raw id.
        "ambient_weather" => "Ambient Weather (your station)",
        "netatmo" => "Netatmo (your station)",
        "lacrosse" => "La Crosse (your station)",
        _ => "Cloud weather service",
    }
}

/// THE shared id/kind -> friendly display-name resolver, used at every
/// PRESENTATION boundary that would otherwise show a raw kind/id string
/// ("open_meteo", "nws") where a person expects a name ("Open-Meteo", "NWS").
/// It prefers the human cloud-service name (so a cloud kind reads as a brand),
/// and falls back to `kind_pretty` for the local stations/gateways/bridges. The
/// internal merge key stays the raw id; this is for display only. Callers:
/// the Sensors tab provenance, the conditions provenance build in api/health.rs,
/// and the per-field picker candidates. Returns an owned String so it composes
/// with the id-keyed lookups (a raw label that maps to nothing stays itself).
pub fn friendly_source_name(kind: &str) -> String {
    // A cloud weather service gets its brand name; everything else (local
    // stations, gateways, bridges, generic ingest) gets the pretty kind label.
    // kind_pretty returns "Unknown" for an unrecognized kind, so fall back to
    // the raw string itself in that case rather than hiding it behind "Unknown".
    match cloud_service_name(kind) {
        // cloud_service_name only names the cloud services; its catch-all is the
        // generic "Cloud weather service", which means "not a known cloud kind".
        "Cloud weather service" => {
            let pretty = kind_pretty(kind);
            if pretty == "Unknown" {
                kind.to_string()
            } else {
                pretty.to_string()
            }
        }
        named => named.to_string(),
    }
}

/// THE shared plain-language descriptor for a cloud weather service, the single
/// source of truth reused by BOTH the Settings per-field picker (data_sources.rs)
/// and the wizard add-source kind picker (sources_form KindPicker). For each
/// service it states, in one line a non-expert can act on: what it is, free or
/// paid, whether an API key is needed, and the coverage. So a user who does not
/// know what NWS or OpenWeather are learns it at the exact point of choice.
///
/// Returns `None` for kinds that are not cloud weather services (local stations,
/// bridges, lightning), so callers can show this descriptor only where it
/// applies. No em dashes anywhere (commas / colons / parentheses / periods).
pub fn cloud_service_descriptor(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "open_meteo" => {
            "Free, no account or API key. Worldwide. Model based current \
             conditions and forecast. The easiest choice if you have no weather \
             hardware."
        }
        "nws" => {
            "Free, no key. United States only. Government current conditions and \
             forecast."
        }
        "openweather" => {
            "Free tier with a free API key. Worldwide. Current conditions and \
             forecast."
        }
        "met_norway" => "Free, no key. Worldwide. Current conditions and forecast.",
        "synoptic" => {
            "Free with a free API token from synopticdata.com. Worldwide, densest \
             in the United States. Real station current conditions for wind, \
             pressure, temperature and humidity (no rain)."
        }
        "weatherkit" => {
            "Needs an Apple developer key. Worldwide. Current conditions and \
             forecast."
        }
        "pirate_weather" => {
            "Free tier with a free API key. Worldwide. Current conditions and \
             forecast."
        }
        _ => return None,
    })
}

/// Human label for a source TIER chip in the per-field picker. The tier id comes
/// from the candidate API (`get_field_sources`): "device" for a local physical
/// sensor on the network, "cloud" for a cloud weather service supplying a current
/// value for the field, "forecast" for a source that only forecasts the field.
pub fn tier_chip_label(tier: &str) -> &'static str {
    match tier {
        "device" => "Your device",
        "cloud" => "Cloud service",
        "forecast" => "Forecast",
        _ => "Source",
    }
}

/// One-line, plain-language descriptor shown beside a candidate in the per-field
/// picker, so selecting it reads plainly. For a cloud weather service this is the
/// shared `cloud_service_descriptor` (free/paid, key, coverage); for a local
/// physical sensor it explains it is a live device reading; for anything else a
/// safe generic line. `field_label` is the human field name ("rain", "wind") so
/// the line can speak in the user's terms.
pub fn candidate_descriptor(tier: &str, kind: &str, field_label: &str) -> String {
    let field = field_label.to_lowercase();
    match tier {
        "device" => format!("A live reading from your own {field} sensor on the network."),
        "cloud" => {
            // The shared cloud-service facts, prefaced so it reads as "use this
            // service's current value as your source, no hardware needed".
            let facts = cloud_service_descriptor(kind)
                .unwrap_or("Free cloud weather service. Worldwide. Current conditions.");
            format!(
                "Use {svc} current {field} as your {field} source, no {field} hardware needed. {facts}",
                svc = cloud_service_name(kind),
            )
        }
        _ => format!("Provides {field} for LocalSky."),
    }
}

pub fn default_config_text(kind: &str) -> String {
    match kind {
        "tempest_udp" => "{\n  \"bind_addr\": \"0.0.0.0:50222\"\n}".into(),
        "tempest_ws" => "{\n  \"access_token\": \"YOUR_TEMPEST_TOKEN\"\n}".into(),
        "davis_wll" => "{\n  \"host\": \"weatherlinklive.local\",\n  \"txid\": 1\n}".into(),
        "open_meteo" => "{\n  \"forecast_days\": 7,\n  \"forecast_hours\": 48,\n  \"past_days\": 1,\n  \"include_radar\": true\n}".into(),
        "nws" => "{\n  \"user_agent\": \"localsky/0.2 (you@example.com)\"\n}".into(),
        "met_norway" => "{\n  \"user_agent\": \"localsky/0.2 (you@example.com)\"\n}".into(),
        "synoptic" => "{\n  \"token\": \"YOUR_SYNOPTIC_TOKEN\",\n  \"station_id\": null,\n  \"radius_mi\": 25.0\n}".into(),
        "openweather" => "{\n  \"api_key\": \"YOUR_OWM_KEY\"\n}".into(),
        "pirate_weather" => "{\n  \"api_key\": \"YOUR_PIRATE_KEY\"\n}".into(),
        "weatherkit" => "{\n  \"key_id\": \"YOUR_KEY_ID\",\n  \"team_id\": \"YOUR_TEAM_ID\",\n  \"service_id\": \"com.example.localsky\",\n  \"private_key_pem\": \"-----BEGIN PRIVATE KEY-----\\n...\\n-----END PRIVATE KEY-----\",\n  \"language\": \"en\"\n}".into(),
        "ambient_weather" => "{\n  \"app_key\": \"YOUR_APP_KEY\",\n  \"api_key\": \"YOUR_API_KEY\",\n  \"mac_address\": \"AA:BB:CC:DD:EE:FF\"\n}".into(),
        "netatmo" => "{\n  \"client_id\": \"YOUR_CLIENT_ID\",\n  \"client_secret\": \"YOUR_CLIENT_SECRET\",\n  \"refresh_token\": \"YOUR_REFRESH_TOKEN\",\n  \"device_id\": \"70:ee:50:00:11:22\"\n}".into(),
        "yolink" => "{\n  \"client_id\": \"YOUR_UAID\",\n  \"client_secret\": \"YOUR_SECRET\",\n  \"base_url\": \"https://api.yosmart.com\",\n  \"device_field_map\": [\n    {\n      \"field\": \"AirTempF\",\n      \"device_id\": \"<deviceId from Home.getDeviceList>\",\n      \"device_type\": \"THSensor\",\n      \"state_path\": \"temperature\",\n      \"scale\": 1.0,\n      \"offset\": 0.0\n    }\n  ]\n}".into(),
        "lacrosse" => "{\n  \"email\": \"\",\n  \"password\": \"\",\n  \"device_id\": null\n}".into(),
        "tuya_cloud" => "{\n  \"client_id\": \"YOUR_TUYA_ACCESS_ID\",\n  \"client_secret\": \"YOUR_TUYA_ACCESS_SECRET\",\n  \"base_url\": \"https://openapi.tuyaus.com\",\n  \"device_field_map\": [\n    {\n      \"field\": \"AirTempF\",\n      \"device_id\": \"<deviceId from tuya iot.tuya.com Devices tab>\",\n      \"status_code\": \"temp_current\",\n      \"scale\": 0.18,\n      \"offset\": 32.0\n    }\n  ]\n}".into(),
        "ecowitt_local" => "{\n  \"path\": \"/ingest/ecowitt\",\n  \"shared_secret\": null\n}".into(),
        "ecowitt_gw_poll" => "{\n  \"host\": \"192.0.2.50\",\n  \"poll_interval_s\": 30\n}".into(),
        "mqtt" => "{\n  \"broker_host\": \"broker.local\",\n  \"broker_port\": 1883,\n  \"username\": null,\n  \"password\": null,\n  \"subscriptions\": [\n    {\n      \"topic\": \"sensors/+/soil\",\n      \"field\": \"rh_pct\",\n      \"json_path\": \"moisture\",\n      \"scale\": 1.0,\n      \"offset\": 0.0\n    }\n  ]\n}".into(),
        "http_webhook" => "{\n  \"path\": \"/ingest/webhook/myhook\",\n  \"fields\": [\n    {\"field\": \"air_temp_f\", \"json_path\": \"temperature\", \"scale\": 1.0, \"offset\": 0.0}\n  ]\n}".into(),
        "rest_poll" => "{\n  \"url\": \"https://api.example.com/current?apiKey=YOUR_KEY\",\n  \"method\": \"GET\",\n  \"poll_interval_s\": 300,\n  \"headers\": {},\n  \"fields\": [\n    {\"field\": \"air_temp_f\", \"json_path\": \"current.temp_f\", \"scale\": 1.0, \"offset\": 0.0},\n    {\"zone_slug\": \"garden\", \"json_path\": \"current.soil_pct\", \"scale\": 1.0, \"offset\": 0.0}\n  ]\n}".into(),
        "prometheus" => "{\n  \"url\": \"http://prometheus.local:9090\",\n  \"poll_interval_s\": 60,\n  \"queries\": [\n    {\"field\": \"air_temp_f\", \"query\": \"weather_temp_f{station=\\\"backyard\\\"}\", \"scale\": 1.0, \"offset\": 0.0},\n    {\"zone_slug\": \"garden\", \"query\": \"soil_moisture_pct{zone=\\\"garden\\\"}\", \"scale\": 1.0, \"offset\": 0.0}\n  ]\n}".into(),
        "influxdb" => "{\n  \"url\": \"http://influxdb.local:8086\",\n  \"database\": \"weather\",\n  \"token\": null,\n  \"poll_interval_s\": 60,\n  \"queries\": [\n    {\"field\": \"air_temp_f\", \"query\": \"SELECT last(\\\"temp_f\\\") FROM \\\"weather\\\"\", \"scale\": 1.0, \"offset\": 0.0},\n    {\"zone_slug\": \"garden\", \"query\": \"SELECT last(\\\"soil_pct\\\") FROM \\\"soil\\\" WHERE \\\"zone\\\"='garden'\", \"scale\": 1.0, \"offset\": 0.0}\n  ]\n}".into(),
        "ha_passthrough" => "{\n  \"base_url\": \"http://homeassistant.local:8123\",\n  \"bearer_token\": \"${HA_LONG_LIVED_TOKEN}\",\n  \"field_map\": {}\n}".into(),
        // enabled defaults false on purpose: Blitzortung.org community
        // data is CC BY-SA 4.0, private/non-commercial, display-only.
        // The operator flips it consciously; validation explains terms.
        "blitzortung" => "{\n  \"enabled\": false,\n  \"radius_mi\": 100.0\n}".into(),
        "demo_replay" => "{\n  \"rate\": 10.0\n}".into(),
        _ => "{}".into(),
    }
}

/// Grouped source-kind picker. Renders the 24 kinds under the five mental-model
/// groups from `kind_groups()` (each with a caption), instead of a flat 24-wide
/// pill strip. One option is active at a time; selecting one writes its kind
/// string into `value`. The recommended zero-config default (Open-Meteo) carries
/// a "Recommended" tag in its tile so a hardware-less user knows where to start.
/// Keyboard + screen-reader friendly: a labeled radiogroup per group, native
/// buttons as radios.
#[component]
fn KindPicker(
    /// The selected source kind (two-way; the active tile reflects it).
    value: RwSignal<String>,
) -> impl IntoView {
    view! {
        <div class="source-kind-picker">
            {kind_groups()
                .into_iter()
                .map(|(title, caption, kinds)| {
                    let group_label = title.to_string();
                    view! {
                        <div class="source-kind-group" role="radiogroup" aria-label=group_label>
                            <div class="source-kind-group__head">
                                <span class="source-kind-group__title">{title}</span>
                                <span class="source-kind-group__caption">{caption}</span>
                            </div>
                            <div class="source-kind-group__options">
                                {kinds
                                    .into_iter()
                                    .map(|k| kind_tile(k, value))
                                    .collect_view()}
                            </div>
                        </div>
                    }
                })
                .collect_view()}
        </div>
    }
}

/// One selectable tile in the grouped picker. A free function (not inline view!
/// nesting) so each tile monomorphizes in its own boundary, keeping recursion
/// depth flat per the no-deep-nesting guidance.
fn kind_tile(kind: &'static str, value: RwSignal<String>) -> impl IntoView {
    let label = kind_options()
        .into_iter()
        .find(|(v, _)| v == kind)
        .map(|(_, l)| l)
        .unwrap_or_else(|| kind.to_string());
    let recommended = is_recommended_default(kind);
    view! {
        <button
            class="source-kind-tile"
            class:source-kind-tile--active=move || value.get() == kind
            class:source-kind-tile--recommended=recommended
            role="radio"
            aria-checked=move || (value.get() == kind).to_string()
            type="button"
            on:click=move |_| value.set(kind.to_string())
        >
            <span class="source-kind-tile__label">{label}</span>
            {recommended.then(|| view! {
                <span class="source-kind-tile__tag">"Recommended"</span>
            })}
        </button>
    }
}

/// Normalize a source id AS THE USER TYPES: lowercase, and map every character
/// that is not ASCII alphanumeric to an underscore, so "My Station" becomes
/// "my_station" live. Deliberately does NOT collapse or trim underscores here,
/// so typing "ecowitt_gw" is not fought mid-word; that cleanup runs on save.
pub fn normalize_source_id_input(s: &str) -> String {
    s.chars()
        .map(|c| {
            let lc = c.to_ascii_lowercase();
            if lc.is_ascii_alphanumeric() {
                lc
            } else {
                '_'
            }
        })
        .collect()
}

/// Full normalization applied ON SAVE: the live-typing pass plus collapsing runs
/// of underscores and trimming leading/trailing ones, so the stored id is a
/// clean snake_case slug ("My  Station_" -> "my_station").
pub fn normalize_source_id_full(s: &str) -> String {
    let mut out = String::new();
    let mut prev_us = false;
    for c in normalize_source_id_input(s).chars() {
        if c == '_' {
            if !prev_us {
                out.push('_');
            }
            prev_us = true;
        } else {
            out.push(c);
            prev_us = false;
        }
    }
    out.trim_matches('_').to_string()
}

/// A self-contained add/edit form for one source. Seeds from `existing`
/// (None = add a new source). On save it parses the config JSON, assembles
/// the `{id, priority, enabled, kind, config}` entry (plus `old_id` when an
/// existing source was renamed), and hands it to `on_commit`, the caller
/// persists. `on_cancel` dismisses the form.
#[component]
pub fn SourceEditorPanel(
    #[prop(default = None)] existing: Option<serde_json::Value>,
    on_commit: Callback<serde_json::Value>,
    on_cancel: Callback<()>,
    /// Zone slugs (slug, display_name) offered in the MQTT soil-subscription
    /// per-zone binding dropdown. Empty by default (the dropdown then offers
    /// only "no zone"); the Sensors hub passes the live zone list.
    #[prop(optional, into)]
    zone_slugs: Option<Memo<Vec<(String, String)>>>,
    /// Ids of the OTHER configured sources (everything except the one being
    /// edited). Used to reject a rename that collides with a sibling up front
    /// (with a clear in-form message) instead of corrupting the local config and
    /// bouncing off a server 422.
    #[prop(optional)]
    sibling_ids: Vec<String>,
) -> impl IntoView {
    // "edit" = the seed carries a real id (lock the id field). A seed with no
    // id but a kind/config (e.g. "adopt this discovered gateway") is a
    // prefilled ADD: the id stays editable and we keep the seeded config.
    let editing = existing
        .as_ref()
        .and_then(|s| s.get("id"))
        .and_then(|v| v.as_str())
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    // Read only by the hydrate-gated template-swap effect below.
    #[allow(unused_variables)]
    let has_seed_config = existing.as_ref().and_then(|s| s.get("config")).is_some();
    let seed_id = existing
        .as_ref()
        .and_then(|s| s.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let seed_kind = existing
        .as_ref()
        .and_then(|s| s.get("kind"))
        .and_then(|v| v.as_str())
        .unwrap_or("ecowitt_local")
        .to_string();
    let seed_priority = existing
        .as_ref()
        .and_then(|s| s.get("priority"))
        .and_then(|v| v.as_i64())
        .unwrap_or(50) as i32;
    let seed_enabled = existing
        .as_ref()
        .and_then(|s| s.get("enabled"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let seed_config = existing
        .as_ref()
        .and_then(|s| s.get("config"))
        .map(|c| serde_json::to_string_pretty(c).unwrap_or_else(|_| "{}".into()))
        .unwrap_or_else(|| default_config_text(&seed_kind));

    // The id as it was when the form opened, so a rename can tell the caller
    // which slot to replace and which references to migrate.
    let original_id = seed_id.clone();
    let id = RwSignal::new(seed_id);
    // True once the user actually types in the ID field. Without this, saving an
    // unrelated field (priority/enabled) on a source whose STORED id is a legacy
    // non-slug (e.g. "ecowitt-gw", valid but not clean) would full-normalize the
    // id on save, differ from the original, and trigger a spurious rename the
    // user never asked for. We only rename when the ID field was edited.
    let id_touched = RwSignal::new(false);
    let kind = RwSignal::new(seed_kind);
    let priority = RwSignal::new(seed_priority);
    let enabled = RwSignal::new(seed_enabled);
    let config_text = RwSignal::new(seed_config);
    let error = RwSignal::new(String::new());
    // Zone slugs for the MQTT soil-subscription per-zone dropdown. None = no
    // zone list supplied; default to empty so the dropdown still renders.
    let zone_slugs = zone_slugs.unwrap_or_else(|| Memo::new(|_| Vec::new()));

    // When composing a fresh source (not editing, no seeded config), swap the
    // JSON template as the kind changes. Skip when a config was seeded (adopt)
    // so the prefilled host isn't clobbered.
    #[cfg(feature = "hydrate")]
    if !editing && !has_seed_config {
        Effect::new(move |_| {
            let k = kind.get();
            config_text.set(default_config_text(&k));
        });
    }

    let sibling_ids_for_save = sibling_ids.clone();
    let on_save = move |_| {
        // The id we will store. If the user never edited the ID field, keep the
        // stored id VERBATIM (do not full-normalize a legacy non-slug id, which
        // would look like a rename the user never asked for). Only when they
        // actually typed do we normalize to a clean slug.
        let id_v = if id_touched.get() {
            normalize_source_id_full(&id.get())
        } else {
            original_id.clone()
        };
        if id_v.is_empty() {
            error.set("Source id is required".into());
            return;
        }
        let is_rename = editing && original_id != id_v;
        // Reject a rename that collides with a sibling up front, in-form, instead
        // of corrupting the local config + bouncing off a server 422.
        if is_rename && sibling_ids_for_save.iter().any(|s| s == &id_v) {
            error.set(format!(
                "A source with id \"{id_v}\" already exists. Pick a different id."
            ));
            return;
        }
        let cfg_value: serde_json::Value = match serde_json::from_str(&config_text.get()) {
            Ok(v) => v,
            Err(e) => {
                error.set(format!("Config JSON parse error: {e}"));
                return;
            }
        };
        error.set(String::new());
        let mut payload = serde_json::json!({
            "id": id_v,
            "priority": priority.get(),
            "enabled": enabled.get(),
            "kind": kind.get(),
            "config": cfg_value,
        });
        // On a RENAME carry the old id so the caller replaces the right slot,
        // migrates every reference (per-reading picks, forecast source, zone soil
        // bindings), and resolves the entry's redacted secrets from the old id.
        if is_rename {
            payload["old_id"] = serde_json::Value::String(original_id.clone());
        }
        on_commit.run(payload);
    };

    view! {
        <div class="source-editor">
            <h3 class="source-editor__title">
                {if editing { "Edit weather source" } else { "Add a weather source" }}
            </h3>
            // IDENTITY: what this source fundamentally IS (its kind, an identity
            // that is locked once the source exists) plus the short name you
            // control (its id, editable and migrated on rename).
            <Panel title="Identity".to_string()>
                <FormField
                    label="ID".to_string()
                    helptext="A short slug you control (e.g. ecowitt_gw, tempest_lan). Anything you type is normalized to snake_case as you go. You CAN rename it while editing: the rename migrates your per-reading picks, forecast source, and zone soil bindings to the new id automatically, so nothing breaks.".to_string()
                    error=Signal::derive(|| None::<String>)
                >
                    <input
                        type="text"
                        class="ui-input"
                        placeholder="e.g. ecowitt_gw"
                        prop:value=move || id.get()
                        on:input=move |ev| {
                            id_touched.set(true);
                            id.set(normalize_source_id_input(&event_target_value(&ev)));
                        }
                    />
                </FormField>

                <FormField
                    label=(if editing { "Type" } else { "What kind of weather source is this?" }).to_string()
                    helptext=(if editing {
                        ""
                    } else {
                        "Pick the group that matches what you have. No hardware? Start with a Weather forecast."
                    })
                    .to_string()
                    error=Signal::derive(|| None::<String>)
                >
                    // On ADD the type is chosen here. On EDIT the type is IDENTITY
                    // and LOCKED: changing a live source's kind would strand its
                    // existing config keys (an incompatible mismatch that can break
                    // the save), so we show what it is + how to change it (remove +
                    // re-add) instead of the picker.
                    {if editing {
                        view! {
                            <p class="sensors-section__hint">
                                "The type is fixed once a source exists. To use a different type, remove this source and add a new one."
                            </p>
                        }
                        .into_any()
                    } else {
                        view! { <KindPicker value=kind/> }.into_any()
                    }}
                    // What-this-is panel: on ADD it updates as you pick; on EDIT it
                    // explains the existing source (so "what am I editing" is clear).
                    <div class="source-pick" aria-live="polite">
                        <p class="source-caps">
                            <span class="source-caps__badge">{move || kind_caps(&kind.get())}</span>
                            <span class="source-caps__name">{move || kind_pretty(&kind.get())}</span>
                        </p>
                        <p class="source-pick__blurb">{move || kind_blurb(&kind.get())}</p>
                        // Cloud weather services get the SHARED plain-language
                        // descriptor (free/paid, API key, coverage), the same copy
                        // the Settings per-field picker shows, so a non-expert knows
                        // what NWS or OpenWeather are and whether they need a key.
                        {move || cloud_service_descriptor(&kind.get()).map(|d| view! {
                            <p class="source-pick__cloud">
                                <span class="source-tier-chip source-tier-chip--cloud">"Cloud service"</span>
                                {d}
                            </p>
                        })}
                        {move || is_recommended_default(&kind.get()).then(|| view! {
                            <p class="source-pick__zero-config">
                                <span class="source-pick__zero-config-tag">"Recommended"</span>
                                "Free, no API key, works immediately. The best choice if you "
                                "don't have hardware."
                            </p>
                        })}
                    </div>
                </FormField>
            </Panel>

            // Structured soil forms for the kinds that previously required
            // hand-edited TOML. Each operates on the same `config_text` signal
            // the raw editor below uses, so the existing config PUT persists
            // them. The sync is two-way for the section each form owns
            // (subscriptions / soil_calibration): editing a card rewrites that
            // key in the textarea, and editing that key in the textarea
            // re-seeds the cards. The forms never touch the other keys
            // (broker/auth, host/poll), so the raw editor stays authoritative
            // for everything else.
            // PRIMARY editing surface: labeled, per-kind base-config fields
            // (host/port/base_url/tokens/api keys/poll cadence/model, ...),
            // rendered from the declarative field_schema and two-way-synced to
            // `config_text`. This is what an operator touches for every kind;
            // the JSON-advanced textarea below is the escape hatch for keys not
            // in the schema (and re-seeds these inputs when edited directly).
            <Panel title="Connection".to_string()>
                // Up-front "what this is + how to set it up" + a direct guide link,
                // so a beginner knows whether they need an IP, a key, etc.
                {move || connection_hint(&kind.get())}
                <SourceConfigForm config_text=config_text kind=Signal::derive(move || kind.get())/>
            </Panel>

            // Per-vendor soil setup hint, shown for the kinds that carry soil
            // data. Short, accurate, and specific to the path the user picked,
            // so a newcomer knows the exact steps before touching the form.
            {move || soil_path_hint(&kind.get())}

            {move || (kind.get() == "mqtt").then(|| view! {
                <Panel title="Soil subscriptions".to_string()>
                    <MqttSoilSubscriptions config_text=config_text zone_slugs=zone_slugs/>
                </Panel>
            })}
            {move || (kind.get() == "ecowitt_gw_poll").then(|| view! {
                <Panel title="Soil channel calibration".to_string()>
                    <EcowittSoilCalibration config_text=config_text/>
                </Panel>
            })}
            {move || matches!(kind.get().as_str(), "http_webhook" | "rest_poll").then(|| view! {
                <Panel title="Field mappings".to_string()>
                    <WeatherFieldMapEditor config_text=config_text zone_slugs=zone_slugs/>
                </Panel>
            })}
            {move || matches!(kind.get().as_str(), "yolink" | "tuya_cloud").then(|| view! {
                <Panel title="Device field mappings".to_string()>
                    <DeviceFieldMapEditor config_text=config_text zone_slugs=zone_slugs kind=kind.get()/>
                </Panel>
            })}
            {move || (kind.get() == "ha_passthrough").then(|| view! {
                <Panel title="Field mappings".to_string()>
                    <HaFieldMapEditor config_text=config_text/>
                </Panel>
            })}

            // BEHAVIOR: how this source competes for a reading (the Auto-mode
            // order, secondary to per-reading pinning) and whether it runs at all.
            <Panel title="Behavior".to_string()>
                <FormField
                    label="Default rank (advanced)".to_string()
                    helptext="You normally never touch this. The REAL priority control is the drag-to-reorder chain under Devices, 'Which source provides each reading': the order you set there IS the priority the engine uses per reading. This number only seeds the STARTING order for a reading you have not reordered yet (100 = local station, 50 = cloud/forecast, 10 = fallback), with automatic failover if a source goes stale.".to_string()
                    error=Signal::derive(|| None::<String>)
                >
                    <input
                        type="number"
                        class="ui-input"
                        min="-100"
                        max="200"
                        prop:value=move || priority.get().to_string()
                        on:input=move |ev| {
                            if let Ok(v) = event_target_value(&ev).parse::<i32>() {
                                priority.set(v);
                            }
                        }
                    />
                </FormField>

                <FormField
                    label="Enabled".to_string()
                    helptext="Unchecked sources stay configured but don't poll/receive.".to_string()
                    error=Signal::derive(|| None::<String>)
                >
                    <label style="display: flex; gap: 0.5rem; align-items: center; min-height: 44px;">
                        <input
                            type="checkbox"
                            prop:checked=move || enabled.get()
                            on:input=move |ev| enabled.set(event_target_checked(&ev))
                        />
                        "Enable this source"
                    </label>
                </FormField>
            </Panel>

            // ADVANCED: the raw config JSON escape hatch, demoted into a fold so
            // the labeled fields above read as the obvious primary editor. Still
            // two-way synced, so unusual keys + power users are covered.
            <details class="settings-section-fold">
                <summary class="settings-section-fold__summary">
                    "Advanced: raw config JSON"
                    <span class="settings-section-fold__hint">"the labeled fields above are the primary editor"</span>
                </summary>
                <div class="settings-section-fold__body">
                    <FormField
                        label="Config (JSON)".to_string()
                        helptext="Escape hatch for keys not in the labeled forms above. Stays in sync both ways, so you rarely need it; use it only for hand-tuning or keys without a widget yet.".to_string()
                        error=Signal::derive(|| None::<String>)
                    >
                        <textarea
                            class="ui-input"
                            style="min-height: 180px; font-family: var(--font-mono); font-size: 0.85rem;"
                            prop:value=move || config_text.get()
                            on:input=move |ev| config_text.set(event_target_value(&ev))
                        ></textarea>
                    </FormField>
                </div>
            </details>

            {move || {
                let e = error.get();
                (!e.is_empty()).then(|| view! { <p class="source-editor__error">{e}</p> })
            }}

            <div class="settings-form-actions">
                <Button variant="ghost" on_click=Callback::new(move |_| on_cancel.run(()))>
                    "Cancel"
                </Button>
                <Button variant="primary" on_click=Callback::new(on_save)>
                    {if editing { "Save changes" } else { "Add source" }}
                </Button>
            </div>
        </div>
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn every_kind_is_in_exactly_one_group() {
        // The grouped picker must show ALL of kind_options(): a kind that falls
        // out of every group would silently vanish from the add-source UI.
        let flat: BTreeSet<String> = kind_options().into_iter().map(|(v, _)| v).collect();
        let mut grouped: Vec<&'static str> = Vec::new();
        for (_title, _caption, kinds) in kind_groups() {
            grouped.extend(kinds);
        }
        let grouped_set: BTreeSet<String> = grouped.iter().map(|k| k.to_string()).collect();
        // No duplicates across (or within) groups.
        assert_eq!(
            grouped.len(),
            grouped_set.len(),
            "a kind appears in more than one picker group"
        );
        // Exact coverage of the canonical flat list.
        assert_eq!(
            grouped_set, flat,
            "grouped picker kinds must exactly match kind_options()"
        );
    }

    #[test]
    fn open_meteo_leads_its_group_and_is_recommended() {
        // Open-Meteo is the zero-config default: first in the cloud-weather group
        // and the single recommended pick.
        let cloud = kind_groups()
            .into_iter()
            .find(|(title, _, _)| *title == "Cloud weather (no hardware)")
            .expect("cloud weather group exists");
        assert_eq!(
            cloud.2.first().copied(),
            Some("open_meteo"),
            "Open-Meteo must be first in its group"
        );
        assert!(is_recommended_default("open_meteo"));
        // It is the ONLY recommended default.
        for (kind, _label) in kind_options() {
            assert_eq!(
                is_recommended_default(&kind),
                kind == "open_meteo",
                "only open_meteo should be the recommended default (got {kind})"
            );
        }
    }

    #[test]
    fn every_kind_has_a_real_blurb() {
        // Each kind needs a plain-language "what this does" line; the generic
        // fallback would mean a kind slipped through unlabeled.
        for (kind, label) in kind_options() {
            let blurb = kind_blurb(&kind);
            assert_ne!(
                blurb, "Provides weather data to LocalSky.",
                "kind `{kind}` ({label}) has only the generic blurb fallback"
            );
            assert!(!blurb.is_empty());
        }
    }

    #[test]
    fn kind_pretty_cloud_kinds_drop_the_forecast_suffix() {
        // Cloud sources are live current-conditions feeds, so kind_pretty must
        // read as a plain service name, never "... forecast" (the lagging copy).
        assert_eq!(kind_pretty("open_meteo"), "Open-Meteo");
        assert_eq!(kind_pretty("nws"), "NWS (US weather service)");
        assert_eq!(kind_pretty("openweather"), "OpenWeather");
        assert_eq!(kind_pretty("pirate_weather"), "Pirate Weather");
        assert_eq!(kind_pretty("met_norway"), "Met.no (Norway)");
        assert_eq!(kind_pretty("weatherkit"), "Apple WeatherKit");
        for kind in [
            "open_meteo",
            "nws",
            "openweather",
            "pirate_weather",
            "met_norway",
            "weatherkit",
        ] {
            assert!(
                !kind_pretty(kind).to_lowercase().contains("forecast"),
                "kind_pretty(`{kind}`) still says forecast; cloud sources are live current conditions"
            );
        }
    }

    #[test]
    fn friendly_source_name_humanizes_at_the_presentation_boundary() {
        // Cloud services get their brand name (the owner's pain: "open_meteo"
        // should read "Open-Meteo" on the Sensors tab + conditions footer).
        assert_eq!(friendly_source_name("open_meteo"), "Open-Meteo");
        assert_eq!(
            friendly_source_name("nws"),
            "NWS (US National Weather Service)"
        );
        // Local stations / gateways / bridges fall back to the pretty kind label,
        // never the raw tag.
        assert_eq!(friendly_source_name("tempest_udp"), "Tempest UDP (LAN)");
        assert_eq!(
            friendly_source_name("ecowitt_gw_poll"),
            "Ecowitt gateway local-API poll"
        );
        // A label the maps don't know passes through unchanged (the friendly
        // resolver never hides an id behind "Unknown").
        assert_eq!(friendly_source_name("Tempest"), "Tempest");
        assert_eq!(friendly_source_name("some_future_kind"), "some_future_kind");
        // Every canonical add-source kind resolves to a non-empty, non-"Unknown"
        // name, so no configured source ever renders a raw tag.
        for (kind, _label) in kind_options() {
            let name = friendly_source_name(&kind);
            assert!(!name.is_empty(), "kind `{kind}` resolved to an empty name");
            assert_ne!(name, "Unknown", "kind `{kind}` leaked the Unknown fallback");
        }
    }
}
