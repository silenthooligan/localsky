// WeatherSource adapters + merge engine + registry.
//
// Adapters shipped:
//   ambient_weather.rs - api.ambientweather.net (cloud-routed LAN station)
//   davis_wll.rs     - Davis WeatherLink Live LAN gateway (VP2 / Vue)
//   demo_replay.rs   - synthetic data for demo mode
//   ecowitt_local.rs - LAN gateway POST receiver
//   ha_passthrough.rs - any HA sensor entity -> WeatherField (meta)
//   http_webhook.rs  - generic JSON POST receiver
//   lacrosse.rs      - LaCrosse View cloud (lacrosseview.com)
//   met_norway.rs    - api.met.no (global, free)
//   mqtt_subscribe.rs - any MQTT broker, topic->field mapping
//   netatmo.rs       - api.netatmo.com Weather Station cloud
//   nws.rs           - api.weather.gov (US, free)
//   openweather.rs   - api.openweathermap.org One Call API 3.0
//   pirate_weather.rs - api.pirateweather.net (Dark-Sky-compatible)
//   tempest_ws.rs    - swd.weatherflow.com Tempest cloud (REST poll)
//   tuya_cloud.rs    - openapi.tuyaXX.com (RainPoint, Smart Life, OEMs)
//   yolink.rs        - YoSmart YoLink cloud (api.yosmart.com)
//
// Adapters declared in schema but not yet built:
//   tempest_udp (legacy path in src/tempest/*)
//   open_meteo (legacy path in src/forecast/*)
//
// Foundation modules:
//   merge.rs    - MergedSnapshot + FieldValue + MergePolicy + merge_field
//   registry.rs - SourceRegistry behind arc-swap

pub mod ambient_weather;
pub mod bus_recorder;
pub mod davis_wll;
pub mod demo_replay;
pub mod ecowitt_gw_poll;
pub mod ecowitt_local;
pub mod ha_passthrough;
pub mod http_webhook;
pub mod lacrosse;
pub mod merge;
pub mod met_norway;
pub mod mqtt_subscribe;
pub mod netatmo;
pub mod nws;
pub mod openweather;
pub mod pirate_weather;
pub mod registry;
pub mod tempest_ws;
pub mod tuya_cloud;
pub mod yolink;

pub use ambient_weather::AmbientWeather;
pub use bus_recorder::SourceLastSeen;
pub use davis_wll::DavisWll;
pub use demo_replay::DemoReplay;
pub use ecowitt_local::EcowittLocal;
pub use ha_passthrough::HaPassthrough;
pub use http_webhook::HttpWebhook;
pub use lacrosse::Lacrosse;
pub use merge::{default_policy, merge_field, FieldValue, MergePolicy, MergedSnapshot};
pub use met_norway::MetNorway;
pub use mqtt_subscribe::MqttSubscribe;
pub use netatmo::Netatmo;
pub use nws::Nws;
pub use openweather::OpenWeather;
pub use pirate_weather::PirateWeather;
pub use registry::SourceRegistry;
pub use tempest_ws::TempestWs;
pub use tuya_cloud::TuyaCloud;
pub use yolink::Yolink;
