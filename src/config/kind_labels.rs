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
