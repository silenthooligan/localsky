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
pub mod ecowitt_gw_mgmt;
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

// ─────────────────────────────────────────────────────────────────────
// Outbound User-Agent identity for the keyless authorities (api.weather.gov,
// api.met.no). Both require an operator-identifying UA in their terms.
// ─────────────────────────────────────────────────────────────────────

/// The historical placeholder identities the prefills used to ship
/// ("localsky/0.2 (you@example.com)", "LocalSky (you@example.com)", ...).
/// Any example.com contact is a template, not an identity; empty means
/// "auto-derive". Also matched: the OLD auto-fill the v0.7.10-v0.7.12
/// region seeder / wizard PERSISTED into configs, "localsky/<version>
/// (+https://github.com/silenthooligan/localsky)": build-frozen, so an
/// upgraded install would otherwise report a stale version to the agencies
/// forever. None of these is ever sent verbatim.
pub fn user_agent_is_placeholder(ua: &str) -> bool {
    let t = ua.trim();
    t.is_empty()
        || t.to_ascii_lowercase().contains("example.com")
        || (t.starts_with("localsky/")
            && t.ends_with("(+https://github.com/silenthooligan/localsky)"))
}

/// The DERIVED User-Agent: package/version plus a per-install identifier,
/// so the agencies see a real, current identity without LocalSky ever
/// collecting a contact string. The instance id is the stable per-install
/// uuid (mDNS/HACS identity); before init (tests, first boot) the UA is
/// version + project URL only.
pub fn derived_user_agent() -> String {
    let version = env!("CARGO_PKG_VERSION");
    match crate::instance::get() {
        Some(uuid) => {
            let short = &uuid[..uuid.len().min(8)];
            format!("localsky/{version} (instance-{short}; +https://localsky.io)")
        }
        None => format!("localsky/{version} (+https://localsky.io)"),
    }
}

/// The User-Agent to actually SEND for a keyless-authority source: the
/// operator's own contact string when they set a real one, else the derived
/// instance identity. The configured value is never rewritten; substitution
/// happens at request time only, so an operator can still clear or edit the
/// field later.
pub fn resolve_outbound_user_agent(configured: &str) -> String {
    if user_agent_is_placeholder(configured) {
        derived_user_agent()
    } else {
        configured.trim().to_string()
    }
}

#[cfg(test)]
mod user_agent_tests {
    use super::*;

    #[test]
    fn placeholders_and_empty_derive_a_real_identity() {
        // The exact strings historical prefills shipped, plus empty/space.
        for ua in [
            "",
            "   ",
            "localsky/0.2 (you@example.com)",
            "LocalSky (you@example.com)",
            "me@EXAMPLE.COM",
            // The auto-fill v0.7.10-v0.7.12 persisted into localsky.toml:
            // build-frozen version, must re-derive after upgrade.
            "localsky/0.7.10 (+https://github.com/silenthooligan/localsky)",
            "localsky/0.7.11 (+https://github.com/silenthooligan/localsky)",
            "localsky/0.7.12 (+https://github.com/silenthooligan/localsky)",
        ] {
            assert!(user_agent_is_placeholder(ua), "placeholder missed: {ua:?}");
            let sent = resolve_outbound_user_agent(ua);
            assert!(
                sent.starts_with(&format!("localsky/{}", env!("CARGO_PKG_VERSION"))),
                "derived UA carries the current version: {sent}"
            );
            assert!(
                !sent.contains("example.com"),
                "a placeholder never goes on the wire: {sent}"
            );
        }
    }

    #[test]
    fn a_real_operator_contact_passes_through_verbatim() {
        let ua = "Jane's Weather Wall (jane@buttondown.dev)";
        assert!(!user_agent_is_placeholder(ua));
        assert_eq!(resolve_outbound_user_agent(ua), ua);
    }

    #[test]
    fn derived_ua_version_matches_cargo_pkg_version() {
        let ua = derived_user_agent();
        let expect_prefix = format!("localsky/{} (", env!("CARGO_PKG_VERSION"));
        assert!(ua.starts_with(&expect_prefix), "ua = {ua}");
        assert!(ua.contains("+https://localsky.io"), "ua = {ua}");
    }
}
