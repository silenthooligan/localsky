// WeatherSource adapters + registry.
//
// Adapters shipped:
//   ambient_weather.rs - api.ambientweather.net (cloud-routed LAN station)
//   blitzortung.rs   - Blitzortung.org community lightning (opt-in,
//                      display-only; feeds TempestStore, not the bus)
//   davis_wll.rs     - Davis WeatherLink Live LAN gateway (VP2 / Vue)
//   demo_replay.rs   - synthetic data for demo mode
//   ecowitt_local.rs - LAN gateway POST receiver
//   ha_passthrough.rs - any HA sensor entity -> WeatherField (meta)
//   http_webhook.rs  - generic JSON POST receiver
//   lacrosse.rs      - LaCrosse View cloud (lacrosseview.com)
//   met_norway.rs    - api.met.no (global, free)
//   mqtt_subscribe.rs - any MQTT broker, topic->field mapping
//   netatmo.rs       - api.netatmo.com Weather Station cloud
//   noaa_mrms.rs     - NOAA MRMS radar QPE grid (US, keyless; stub adapter)
//   nws.rs           - api.weather.gov (US, free)
//   openweather.rs   - api.openweathermap.org One Call API 3.0
//   pirate_weather.rs - api.pirateweather.net (Dark-Sky-compatible)
//   synoptic.rs      - api.synopticdata.com (MesoWest nearest-station obs)
//   tempest_ws.rs    - swd.weatherflow.com Tempest cloud (REST poll)
//   tuya_cloud.rs    - openapi.tuyaXX.com (RainPoint, Smart Life, OEMs)
//   yolink.rs        - YoSmart YoLink cloud (api.yosmart.com)
//
// Adapters declared in schema but not yet built:
//   tempest_udp (legacy path in src/tempest/*)
//   open_meteo (legacy path in src/forecast/*)
//
// Foundation modules:
//   registry.rs - SourceRegistry behind arc-swap

pub mod ambient_weather;
pub mod blitzortung;
pub mod bus_recorder;
pub mod cloud_catalog;
pub mod davis_wll;
pub mod demo_replay;
pub mod ecowitt_gw_poll;
pub mod ecowitt_local;
pub mod forecast_bridge;
pub mod ha_passthrough;
pub mod http_webhook;
pub mod influxdb;
pub mod lacrosse;
pub mod met_norway;
pub mod mqtt_subscribe;
pub mod netatmo;
pub mod noaa_mrms;
pub mod nws;
pub mod openweather;
pub mod pirate_weather;
pub mod prometheus;
pub mod registry;
pub mod rest_poll;
pub mod snapshot_bridge;
pub mod synoptic;
pub mod tempest_ws;
pub mod tuya_cloud;
pub mod units;
pub mod weatherkit;
pub mod yolink;

pub use ambient_weather::AmbientWeather;
pub use bus_recorder::SourceLastSeen;
pub use bus_recorder::SourceReachability;
pub use davis_wll::DavisWll;
pub use demo_replay::DemoReplay;
pub use ecowitt_local::EcowittLocal;
pub use ha_passthrough::HaPassthrough;
pub use http_webhook::HttpWebhook;
pub use influxdb::InfluxDb;
pub use lacrosse::Lacrosse;
pub use met_norway::MetNorway;
pub use mqtt_subscribe::MqttSubscribe;
pub use netatmo::Netatmo;
pub use noaa_mrms::NoaaMrms;
pub use nws::Nws;
pub use openweather::OpenWeather;
pub use pirate_weather::PirateWeather;
pub use prometheus::Prometheus;
pub use registry::SourceRegistry;
pub use rest_poll::RestPoll;
pub use synoptic::Synoptic;
pub use tempest_ws::TempestWs;
pub use tuya_cloud::TuyaCloud;
pub use weatherkit::WeatherKit;
pub use yolink::Yolink;
