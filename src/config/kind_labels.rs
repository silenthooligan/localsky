// Canonical kind -> stable tag/label maps. These return the snake_case tag that
// matches each enum's serde representation, used by /api/v1/health, the sensors
// API, and anywhere a source/controller kind needs a stable string. Single
// source of truth: previously health.rs and sensors.rs each carried an
// identical 24-arm match, so adding a source kind meant editing both (and the
// compiler does NOT catch a missed arm in the OTHER file). Centralizing here
// makes the exhaustive match the only place to update.

use crate::config::schema::{ControllerKind, SourceKind};

/// Stable snake_case tag for a source kind (matches its serde tag).
pub fn source_kind_label(kind: &SourceKind) -> &'static str {
    use SourceKind::*;
    match kind {
        TempestUdp(_) => "tempest_udp",
        TempestWs(_) => "tempest_ws",
        OpenMeteo(_) => "open_meteo",
        EcowittLocal(_) => "ecowitt_local",
        EcowittGwPoll(_) => "ecowitt_gw_poll",
        Nws(_) => "nws",
        OpenWeather(_) => "openweather",
        PirateWeather(_) => "pirate_weather",
        MetNorway(_) => "met_norway",
        Synoptic(_) => "synoptic",
        NoaaMrms(_) => "noaa_mrms",
        AmbientWeather(_) => "ambient_weather",
        Netatmo(_) => "netatmo",
        Yolink(_) => "yolink",
        Lacrosse(_) => "lacrosse",
        TuyaCloud(_) => "tuya_cloud",
        DavisWll(_) => "davis_wll",
        HaPassthrough(_) => "ha_passthrough",
        Mqtt(_) => "mqtt",
        HttpWebhook(_) => "http_webhook",
        RestPoll(_) => "rest_poll",
        Prometheus(_) => "prometheus",
        InfluxDb(_) => "influxdb",
        WeatherKit(_) => "weatherkit",
        Blitzortung(_) => "blitzortung",
        DemoReplay(_) => "demo_replay",
    }
}

/// Stable snake_case tag for a controller kind (matches its serde tag).
pub fn controller_kind_label(kind: &ControllerKind) -> &'static str {
    use ControllerKind::*;
    match kind {
        OpensprinklerDirect(_) => "opensprinkler_direct",
        HaServiceCall(_) => "ha_service_call",
        EsphomeNative(_) => "esphome_native",
        Rachio(_) => "rachio",
        Hydrawise(_) => "hydrawise",
        Bhyve(_) => "bhyve",
        Rainbird(_) => "rainbird",
        MqttCommand(_) => "mqtt_command",
        HttpGeneric(_) => "http_generic",
        DryRun(_) => "dry_run",
    }
}

// ---------------------------------------------------------------------------
// Display names. The one table a person sees a source kind through: the
// kind picker, the per-field picker, the Sensors provenance, the health
// report's conditions provenance. These used to live in the sources form
// component, which made the server import a UI module to name a source.
// ---------------------------------------------------------------------------

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

/// The friendly kind of a soil gateway or a sensor source on the sensor
/// pages, with the caller's word for an unknown or empty kind ("Gateway"
/// on the wizard, "Source" in Settings).
pub fn gateway_kind_label(kind: &str, fallback: &str) -> String {
    match kind {
        "ecowitt_gw_poll" | "ecowitt" => "Ecowitt gateway".to_string(),
        "ecowitt_push" => "Ecowitt push".to_string(),
        "mqtt" => "MQTT".to_string(),
        "home_assistant" | "ha" => "Home Assistant".to_string(),
        "opensprinkler" => "OpenSprinkler".to_string(),
        "esphome" => "ESPHome".to_string(),
        "" => fallback.to_string(),
        other => other.replace('_', " "),
    }
}

#[cfg(test)]
mod display_tests {
    use super::*;

    /// Every kind the picker offers has a long name and a friendly name,
    /// and the friendly name never falls through to the raw tag.
    #[test]
    fn every_offered_kind_has_display_names() {
        for (tag, short) in kind_options() {
            assert_ne!(kind_pretty(&tag), "Unknown", "{tag}");
            assert_ne!(friendly_source_name(&tag), tag, "{tag}");
            assert!(!short.is_empty());
            assert!(!kind_caps(&tag).is_empty());
        }
    }
}
