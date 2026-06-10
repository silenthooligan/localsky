// Device model (Phase D of the LocalSky <-> HA device-parity effort).
//
// LocalSky has always modelled flat *sources* (weather adapters) and a
// separate ControllerRegistry (irrigation). Neither gives the Music-
// Assistant-style "device" view the operator expects: a physical/logical
// unit (a gateway, a hub, a controller, a cloud account) that GROUPS the
// child sensors or zones it provides, with a stable identity and an origin
// (did LocalSky discover it natively, or is it mirrored from Home
// Assistant?).
//
// This module is the foundation. `Device` is a serializable topology node;
// `DeviceRegistry` (registry.rs) holds the live set behind arc-swap exactly
// like SourceRegistry/ControllerRegistry; `build_devices` (builder.rs)
// derives the set from the configured sources + controllers so the registry
// is populated from what already exists (no behavior change). Later phases
// add native gateway discovery (E), HA device import + MQTT publish-out (F),
// and the unified device UI (G).

pub mod builder;
pub mod ha_import;
pub mod reconcile;
pub mod registry;

pub use builder::build_devices;
pub use registry::DeviceRegistry;

use serde::Serialize;

/// What kind of thing a device is. Drives the icon + grouping in the UI and
/// the publish/import rules in Phase F.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceKind {
    /// A LAN weather station gateway/hub LocalSky reads directly
    /// (Tempest hub, Ecowitt GW, Davis WLL, YoLink/LaCrosse hub).
    WeatherGateway,
    /// A cloud weather service or account (Open-Meteo, NWS, OpenWeather,
    /// Ambient Weather, Netatmo, Tuya). Not a physical box on the LAN.
    WeatherCloud,
    /// An irrigation controller (OpenSprinkler, Rachio, B-hyve, ...). Its
    /// children are zones rather than sensors.
    IrrigationController,
    /// The Home Assistant bridge itself (the ha_passthrough source). A
    /// single device representing "everything reached via HA".
    HaBridge,
    /// A generic ingest endpoint (MQTT topic set, HTTP webhook, demo feed)
    /// with no specific hardware identity.
    Virtual,
}

/// Whether LocalSky owns this device natively or is mirroring it from Home
/// Assistant. The dedup/echo rules in Phase F key off this.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeviceOrigin {
    /// Discovered/configured directly in LocalSky (a native source or
    /// controller). The MA-style "added here" side.
    Native,
    /// Imported from Home Assistant (Phase F). Present so the UI can badge
    /// it and so publish-out never echoes it back.
    HomeAssistant,
}

/// What a device child is: a sensor reading (with a coarse role) or an
/// irrigation zone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DeviceChildKind {
    /// A sensor capability/reading. `role` is the coarse bucket the picker
    /// + UI group on (temperature, humidity, wind, rain, soil, ...).
    Sensor { role: String },
    /// An irrigation zone on a controller.
    Zone,
}

/// One child of a device: a sensor it provides or a zone it drives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DeviceChild {
    /// Canonical address (`source:<src>:<key>` for native sensors,
    /// `ha:<entity>` once HA import lands, or a zone slug for zones). The
    /// same address scheme zone soil_sensor_id already uses.
    pub id: String,
    /// Friendly label for lists.
    pub label: String,
    #[serde(flatten)]
    pub kind: DeviceChildKind,
}

impl DeviceChild {
    pub fn sensor(
        id: impl Into<String>,
        label: impl Into<String>,
        role: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            kind: DeviceChildKind::Sensor { role: role.into() },
        }
    }
    pub fn zone(id: impl Into<String>, label: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            kind: DeviceChildKind::Zone,
        }
    }
}

/// A device: a gateway / hub / controller / cloud account / bridge that
/// groups the sensors or zones it provides.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Device {
    /// Stable id, namespaced by backing: `source:<id>` or `controller:<id>`
    /// (and `ha:<unique_id>` once Phase F imports HA devices).
    pub id: String,
    pub kind: DeviceKind,
    /// Friendly name (defaults derived from the source/controller; operator
    /// override comes with the device UI in Phase G).
    pub name: String,
    /// Hardware/service model string when known ("Tempest", "GW2000",
    /// "OpenSprinkler"). None until discovery (E) fills it in for some.
    pub model: Option<String>,
    /// Stable hardware identity (mac / serial / hub id) used for the
    /// cross-source dedup in Phase F. None when not yet known.
    pub identity: Option<String>,
    pub origin: DeviceOrigin,
    /// The backing source or controller id, when this device wraps one.
    pub source_id: Option<String>,
    /// Reachability, when known. None = not yet wired (Phase E/F enrich it
    /// from source reachability events); the UI renders "unknown".
    pub online: Option<bool>,
    /// Epoch of the last reading/contact, when known.
    pub last_seen_epoch: Option<i64>,
    /// Set on a native device that Phase F3 reconciled with an HA-imported
    /// copy of the same physical hardware: the two are shown as one card
    /// (this native one) badged "+ HA". The HA duplicate is dropped.
    #[serde(default)]
    pub also_in_ha: bool,
    /// Sensors or zones this device provides.
    pub children: Vec<DeviceChild>,
}

/// Coarse role bucket for a `WeatherField`, used to group a weather
/// source's provided fields into sensor children. Mirrors the role strings
/// the discovered-sensor picker already uses.
#[cfg(feature = "ssr")]
pub fn field_role(field: &crate::ports::weather_source::WeatherField) -> &'static str {
    use crate::ports::weather_source::WeatherField as F;
    match field {
        F::AirTempF | F::DewPointF => "temperature",
        F::RhPct => "humidity",
        F::WindMph | F::WindGustMph | F::WindBearingDeg => "wind",
        F::SolarWm2 | F::UvIndex | F::Illuminance => "light",
        F::PressureInHg => "pressure",
        F::RainTodayIn | F::RainIntensityInHr | F::RainTypeStr => "rain",
        F::LightningCount | F::LightningDistanceMi => "lightning",
        F::Et0Today => "et",
        F::FlowGpm | F::FlowTotalGalToday => "flow",
        F::ForecastDaily | F::ForecastHourly | F::Pop => "forecast",
    }
}

/// Human label for a `WeatherField` (sensor-child label in the device view).
#[cfg(feature = "ssr")]
pub fn field_label(field: &crate::ports::weather_source::WeatherField) -> &'static str {
    use crate::ports::weather_source::WeatherField as F;
    match field {
        F::AirTempF => "Air temperature",
        F::DewPointF => "Dew point",
        F::RhPct => "Humidity",
        F::WindMph => "Wind speed",
        F::WindGustMph => "Wind gust",
        F::WindBearingDeg => "Wind direction",
        F::SolarWm2 => "Solar radiation",
        F::UvIndex => "UV index",
        F::Illuminance => "Illuminance",
        F::PressureInHg => "Pressure",
        F::RainTodayIn => "Rain today",
        F::RainIntensityInHr => "Rain intensity",
        F::RainTypeStr => "Precipitation type",
        F::LightningCount => "Lightning strikes",
        F::LightningDistanceMi => "Lightning distance",
        F::Et0Today => "Reference ET0",
        F::FlowGpm => "Flow rate",
        F::FlowTotalGalToday => "Flow total today",
        F::ForecastDaily => "Daily forecast",
        F::ForecastHourly => "Hourly forecast",
        F::Pop => "Precip probability",
    }
}

/// Stable key for a `WeatherField`, used as the `<key>` in a child sensor's
/// `source:<src>:<key>` address. snake_case of the variant.
#[cfg(feature = "ssr")]
pub fn field_key(field: &crate::ports::weather_source::WeatherField) -> &'static str {
    use crate::ports::weather_source::WeatherField as F;
    match field {
        F::AirTempF => "air_temp_f",
        F::DewPointF => "dew_point_f",
        F::RhPct => "rh_pct",
        F::WindMph => "wind_mph",
        F::WindGustMph => "wind_gust_mph",
        F::WindBearingDeg => "wind_bearing_deg",
        F::SolarWm2 => "solar_wm2",
        F::UvIndex => "uv_index",
        F::Illuminance => "illuminance",
        F::PressureInHg => "pressure_inhg",
        F::RainTodayIn => "rain_today_in",
        F::RainIntensityInHr => "rain_intensity_in_hr",
        F::RainTypeStr => "rain_type",
        F::LightningCount => "lightning_count",
        F::LightningDistanceMi => "lightning_distance_mi",
        F::Et0Today => "et0_today",
        F::FlowGpm => "flow_gpm",
        F::FlowTotalGalToday => "flow_total_gal_today",
        F::ForecastDaily => "forecast_daily",
        F::ForecastHourly => "forecast_hourly",
        F::Pop => "pop",
    }
}
