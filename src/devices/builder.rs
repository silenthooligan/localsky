// Derive the Device set from the configured sources + controllers + zones.
//
// Phase D is topology-only: one device per enabled weather source (its
// children are the WeatherFields it advertises in SourceCaps) and one per
// enabled irrigation controller (its children are the zones bound to it).
// Online/last_seen + concrete per-channel sensors arrive in Phase E (native
// discovery), and HA-imported devices in Phase F. All devices here are
// origin = Native.

use crate::config::schema::{Config, ControllerKind, SourceKind};
use crate::ports::weather_source::WeatherField;

use super::{Device, DeviceChild, DeviceKind, DeviceOrigin};

/// Build the device topology from config. A device's sensor children are the
/// fields its source kind typically provides (`kind_fields`); concrete
/// per-channel sensors (e.g. an Ecowitt gateway's four soil probes) replace
/// these once native discovery lands in Phase E. Deriving from config (not a
/// live SourceRegistry) is deliberate: the merge runtime isn't the live path
/// yet, and several source kinds the operator cares about (TempestUdp,
/// EcowittLocal, OpenMeteo) aren't expressed as constructed sources today.
pub fn build_devices(config: &Config) -> Vec<Device> {
    let mut devices = Vec::new();

    // --- Weather / sensor sources ---
    for entry in config.sources.iter().filter(|s| s.enabled) {
        let (kind, model, name) = classify_source(&entry.source, &entry.id);
        let children: Vec<DeviceChild> = kind_fields(&entry.source)
            .iter()
            .map(|f| {
                DeviceChild::sensor(
                    format!("source:{}:{}", entry.id, super::field_key(f)),
                    super::field_label(f),
                    super::field_role(f),
                )
            })
            .collect();
        devices.push(Device {
            id: format!("source:{}", entry.id),
            kind,
            name,
            model,
            identity: source_identity(&entry.source),
            origin: DeviceOrigin::Native,
            source_id: Some(entry.id.clone()),
            online: None,
            last_seen_epoch: None,
            children,
        });
    }

    // --- Irrigation controllers ---
    for entry in config.controllers.iter().filter(|c| c.enabled) {
        let (model, name) = classify_controller(&entry.controller, &entry.id);
        // Zones bound to this controller. A zone with an empty controller_id
        // belongs to the default controller.
        let children: Vec<DeviceChild> = config
            .zones
            .iter()
            .filter(|(_, z)| {
                z.controller_id == entry.id || (z.controller_id.is_empty() && entry.default)
            })
            .map(|(slug, z)| DeviceChild::zone(slug.replace('-', "_"), z.display_name.clone()))
            .collect();
        devices.push(Device {
            id: format!("controller:{}", entry.id),
            kind: DeviceKind::IrrigationController,
            name,
            model,
            identity: None,
            origin: DeviceOrigin::Native,
            source_id: Some(entry.id.clone()),
            online: None,
            last_seen_epoch: None,
            children,
        });
    }

    devices.sort_by(|a, b| a.id.cmp(&b.id));
    devices
}

/// Map a source kind to `(DeviceKind, model, default name)`.
fn classify_source(kind: &SourceKind, id: &str) -> (DeviceKind, Option<String>, String) {
    use SourceKind as K;
    let (dk, model, name) = match kind {
        K::TempestUdp(_) | K::TempestWs(_) => (DeviceKind::WeatherGateway, "Tempest", "Tempest"),
        K::EcowittLocal(_) => (DeviceKind::WeatherGateway, "Ecowitt", "Ecowitt gateway"),
        K::EcowittGwPoll(_) => (DeviceKind::WeatherGateway, "Ecowitt", "Ecowitt gateway"),
        K::DavisWll(_) => (
            DeviceKind::WeatherGateway,
            "Davis WeatherLink Live",
            "Davis WLL",
        ),
        K::Yolink(_) => (DeviceKind::WeatherGateway, "YoLink", "YoLink hub"),
        K::Lacrosse(_) => (DeviceKind::WeatherGateway, "LaCrosse", "LaCrosse"),
        K::OpenMeteo(_) => (DeviceKind::WeatherCloud, "Open-Meteo", "Open-Meteo"),
        K::Nws(_) => (DeviceKind::WeatherCloud, "NWS", "National Weather Service"),
        K::OpenWeather(_) => (DeviceKind::WeatherCloud, "OpenWeather", "OpenWeather"),
        K::PirateWeather(_) => (DeviceKind::WeatherCloud, "Pirate Weather", "Pirate Weather"),
        K::MetNorway(_) => (DeviceKind::WeatherCloud, "MET Norway", "MET Norway"),
        K::AmbientWeather(_) => (
            DeviceKind::WeatherCloud,
            "Ambient Weather",
            "Ambient Weather",
        ),
        K::Netatmo(_) => (DeviceKind::WeatherCloud, "Netatmo", "Netatmo"),
        K::TuyaCloud(_) => (DeviceKind::WeatherCloud, "Tuya", "Tuya Cloud"),
        K::HaPassthrough(_) => (DeviceKind::HaBridge, "Home Assistant", "Home Assistant"),
        K::Mqtt(_) => (DeviceKind::Virtual, "MQTT", "MQTT source"),
        K::HttpWebhook(_) => (DeviceKind::Virtual, "HTTP", "HTTP webhook"),
        K::DemoReplay(_) => (DeviceKind::Virtual, "Demo", "Demo feed"),
    };
    // Use the operator's source id as the display name when it carries more
    // meaning than the generic kind name (it usually does, e.g. "tempest_lan").
    let name = if id.is_empty() {
        name.to_string()
    } else {
        humanize_id(id)
    };
    (dk, Some(model.to_string()), name)
}

/// Map a controller kind to `(model, default name)`.
fn classify_controller(kind: &ControllerKind, id: &str) -> (Option<String>, String) {
    use ControllerKind as K;
    let model = match kind {
        K::OpensprinklerDirect(_) => "OpenSprinkler",
        K::HaServiceCall(_) => "Home Assistant",
        K::EsphomeNative(_) => "ESPHome",
        K::Rachio(_) => "Rachio",
        K::Hydrawise(_) => "Hydrawise",
        K::Bhyve(_) => "B-hyve",
        K::Rainbird(_) => "Rain Bird",
        K::MqttCommand(_) => "MQTT",
        K::DryRun(_) => "Dry run",
    };
    let name = if id.is_empty() {
        model.to_string()
    } else {
        humanize_id(id)
    };
    (Some(model.to_string()), name)
}

/// Representative fields a source kind provides, used to populate a device's
/// sensor children in Phase D. Approximate by design (the concrete sensor
/// set comes from discovery in Phase E); a bridge / generic ingest returns
/// none because its children depend entirely on what's mapped or imported.
fn kind_fields(kind: &SourceKind) -> Vec<WeatherField> {
    use SourceKind as K;
    use WeatherField as F;
    let station_full = || {
        vec![
            F::AirTempF,
            F::RhPct,
            F::WindMph,
            F::WindGustMph,
            F::WindBearingDeg,
            F::PressureInHg,
            F::RainTodayIn,
            F::RainIntensityInHr,
            F::SolarWm2,
            F::UvIndex,
        ]
    };
    match kind {
        K::TempestUdp(_) | K::TempestWs(_) => {
            let mut v = station_full();
            v.push(F::Illuminance);
            v.push(F::LightningCount);
            v.push(F::LightningDistanceMi);
            v
        }
        K::EcowittLocal(_) | K::EcowittGwPoll(_) | K::DavisWll(_) => station_full(),
        K::AmbientWeather(_) => vec![
            F::AirTempF,
            F::RhPct,
            F::WindMph,
            F::RainTodayIn,
            F::SolarWm2,
            F::UvIndex,
        ],
        K::Netatmo(_) => vec![F::AirTempF, F::RhPct, F::PressureInHg, F::RainTodayIn],
        K::Yolink(_) | K::Lacrosse(_) | K::TuyaCloud(_) => vec![F::AirTempF, F::RhPct],
        K::OpenMeteo(_) => vec![F::ForecastDaily, F::ForecastHourly, F::Pop, F::Et0Today],
        K::Nws(_) => vec![F::ForecastDaily, F::ForecastHourly, F::Pop],
        K::OpenWeather(_) | K::PirateWeather(_) | K::MetNorway(_) => {
            vec![
                F::ForecastDaily,
                F::ForecastHourly,
                F::Pop,
                F::RainIntensityInHr,
            ]
        }
        K::DemoReplay(_) => vec![F::AirTempF, F::RhPct, F::WindMph, F::RainTodayIn],
        // Bridge + generic ingest: children come from what's mapped (F) or
        // posted, not from the kind. Empty until those land.
        K::HaPassthrough(_) | K::Mqtt(_) | K::HttpWebhook(_) => Vec::new(),
    }
}

/// Best-effort stable hardware identity from the source config, used for the
/// cross-source dedup in Phase F. Only the few kinds that carry one in
/// config today return Some; the rest gain identity at discovery time (E).
fn source_identity(kind: &SourceKind) -> Option<String> {
    match kind {
        SourceKind::TempestUdp(c) => c.hub_serial.clone(),
        // The gateway host is a stable-enough identity until discovery (E2)
        // resolves the MAC.
        SourceKind::EcowittGwPoll(c) => Some(c.host.clone()),
        _ => None,
    }
}

/// Turn a snake/kebab id into a Title Case label ("tempest_lan" -> "Tempest
/// Lan", "back-yard" -> "Back Yard").
fn humanize_id(id: &str) -> String {
    id.split(['_', '-'])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::Config;

    // A minimal config with one Open-Meteo source + one dry-run controller +
    // one zone bound to it. Open-Meteo needs no network, dry-run is inert.
    fn cfg() -> Config {
        let toml = r#"
schema_version = 1

[deployment]
mode = "standalone"

[deployment.location]
lat = 30.0
lon = -81.0

[[sources]]
id = "open_meteo"
kind = "open_meteo"
[sources.config]

[[controllers]]
id = "os1"
default = true
kind = "opensprinkler_direct"
[controllers.config]
host = "192.0.2.60"
password_md5 = "a6d82bced638de3def1e9bbb4983225c"

[zones.back_yard]
display_name = "Back Yard"
area_sqft = 1000.0
species = "st_augustine"
soil_texture = "sandy_loam"
slope_pct = 0.0
sun_exposure = "full"
sprinkler_type = "rotor"
controller_id = "os1"
controller_station = "1"
"#;
        toml::from_str(toml).expect("test config parses")
    }

    #[test]
    fn builds_a_device_per_source_and_controller() {
        let devices = build_devices(&cfg());
        assert_eq!(
            devices.len(),
            2,
            "one source device + one controller device"
        );
        // Sorted by id: controller:os1 < source:open_meteo
        assert_eq!(devices[0].id, "controller:os1");
        assert_eq!(devices[0].kind, DeviceKind::IrrigationController);
        assert_eq!(devices[0].model.as_deref(), Some("OpenSprinkler"));
        assert_eq!(devices[1].id, "source:open_meteo");
        assert_eq!(devices[1].kind, DeviceKind::WeatherCloud);
        // Open-Meteo advertises forecast + et0 capability children.
        assert!(devices[1]
            .children
            .iter()
            .any(|c| c.id == "source:open_meteo:et0_today"));
    }

    #[test]
    fn controller_groups_its_zone() {
        let devices = build_devices(&cfg());
        let ctrl = devices.iter().find(|d| d.id == "controller:os1").unwrap();
        assert_eq!(ctrl.children.len(), 1);
        assert_eq!(ctrl.children[0].id, "back_yard");
        assert_eq!(ctrl.children[0].label, "Back Yard");
    }

    #[test]
    fn humanize_id_titlecases() {
        assert_eq!(humanize_id("tempest_lan"), "Tempest Lan");
        assert_eq!(humanize_id("back-yard"), "Back Yard");
    }
}
