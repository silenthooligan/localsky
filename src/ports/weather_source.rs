// WeatherSource port. Every weather data adapter (Tempest UDP, Tempest WS,
// Open-Meteo, Ecowitt LAN, NWS, Blitzortung, etc.) implements this trait.
//
// Adapters own their own polling/listener task and publish into a shared
// SourceBus. The engine merges across sources using per-field priority().
// There is one ingress: a station on the LAN and a cloud API publish the
// same events and the same arbiter ranks them.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::broadcast;

/// Fields a source can produce. Used for per-field priority + merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum WeatherField {
    AirTempF,
    DewPointF,
    RhPct,
    WindMph,
    WindGustMph,
    WindBearingDeg,
    SolarWm2,
    UvIndex,
    Illuminance,
    PressureInHg,
    RainTodayIn,
    RainIntensityInHr,
    RainTypeStr,
    LightningCount,
    LightningDistanceMi,
    Et0Today,
    /// Flow rate in US gallons per minute. Sourced from a flow meter
    /// attached to an irrigation controller (Hunter HC, Rachio Wireless
    /// Flow Meter, OpenSprinkler flow sensor, Davis WLL flow port) OR
    /// from a standalone pulse-output meter wired to MQTT/ESPHome.
    FlowGpm,
    /// Cumulative flow today in US gallons. Same sources as FlowGpm;
    /// useful for leak detection (engine watches dGal/dt while no zone
    /// is scheduled to run).
    FlowTotalGalToday,
    ForecastDaily,
    ForecastHourly,
    Pop,
    /// Leaf wetness as a percent (0-100). A surface-wetness reading from a
    /// dedicated leaf-wetness sensor (Davis WLL soil/leaf station, Ecowitt
    /// WH35, agricultural probes). Display + history only; the engine does not
    /// gate irrigation on it, but it is a recognized agronomic reading.
    LeafWetness,
    /// The lowest wind speed over the station's averaging interval, mph.
    WindLullMph,
    /// A station's short-interval wind reading (the Tempest's 3 s
    /// rapid_wind), mph, with its bearing. Display only: the needle and
    /// the "now" bar. Never a merge input for the engine's wind gate.
    RapidWindMph,
    RapidWindBearingDeg,
    /// Station battery, volts.
    BatteryV,
    /// The rain that fell in the last reporting minute, inches. A station
    /// that reports the increment rather than a since-midnight total
    /// publishes this; LocalSky integrates it into `RainTodayIn` on the
    /// deployment's calendar. Never map a per-minute entity to
    /// `RainTodayIn` directly: the store treats that as an accumulation
    /// and records it as measured gauge evidence.
    RainLastMinIn,
    /// Precipitation type code: 0 none, 1 rain, 2 hail.
    PrecipType,
}

impl WeatherField {
    /// The field's name on every operator-facing wire: the
    /// `field_source_overrides` config key, the MQTT and webhook field
    /// mapping, the settings pickers. snake_case of the variant with its
    /// unit suffix.
    pub fn name(self) -> &'static str {
        use WeatherField::*;
        match self {
            AirTempF => "air_temp_f",
            DewPointF => "dew_point_f",
            RhPct => "rh_pct",
            WindMph => "wind_mph",
            WindGustMph => "wind_gust_mph",
            WindBearingDeg => "wind_bearing_deg",
            SolarWm2 => "solar_w_m2",
            UvIndex => "uv_index",
            Illuminance => "illuminance",
            PressureInHg => "pressure_in_hg",
            RainTodayIn => "rain_today_in",
            RainIntensityInHr => "rain_intensity_in_hr",
            RainTypeStr => "rain_type_str",
            LightningCount => "lightning_count",
            LightningDistanceMi => "lightning_distance_mi",
            Et0Today => "et0_today",
            FlowGpm => "flow_gpm",
            FlowTotalGalToday => "flow_total_gal_today",
            ForecastDaily => "forecast_daily",
            ForecastHourly => "forecast_hourly",
            Pop => "pop",
            LeafWetness => "leaf_wetness_pct",
            WindLullMph => "wind_lull_mph",
            RapidWindMph => "rapid_wind_mph",
            RapidWindBearingDeg => "rapid_wind_bearing_deg",
            BatteryV => "battery_v",
            RainLastMinIn => "rain_in_last_min",
            PrecipType => "precip_type",
        }
    }

    /// The inverse of `name`. None for a spelling no field has (a typo, a
    /// removed field); callers ignore rather than fail.
    pub fn parse(name: &str) -> Option<WeatherField> {
        ALL.iter().copied().find(|f| f.name() == name)
    }

    /// The human label a reading carries in the device view and the
    /// sensors pages.
    pub fn label(self) -> &'static str {
        use WeatherField::*;
        match self {
            AirTempF => "Air temperature",
            DewPointF => "Dew point",
            RhPct => "Humidity",
            WindMph => "Wind speed",
            WindGustMph => "Wind gust",
            WindBearingDeg => "Wind direction",
            SolarWm2 => "Solar radiation",
            UvIndex => "UV index",
            Illuminance => "Illuminance",
            PressureInHg => "Pressure",
            RainTodayIn => "Rain today",
            RainIntensityInHr => "Rain intensity",
            RainTypeStr => "Precipitation type",
            LightningCount => "Lightning strikes",
            LightningDistanceMi => "Lightning distance",
            Et0Today => "Reference ET0",
            FlowGpm => "Flow rate",
            FlowTotalGalToday => "Flow total today",
            LeafWetness => "Leaf wetness",
            ForecastDaily => "Daily forecast",
            ForecastHourly => "Hourly forecast",
            Pop => "Precip probability",
            WindLullMph => "Wind lull",
            RapidWindMph => "Wind now",
            RapidWindBearingDeg => "Wind direction now",
            BatteryV => "Station battery",
            RainLastMinIn => "Rain last minute",
            PrecipType => "Precipitation type",
        }
    }

    /// The key a bus reading is recorded under in sensor_history. The same
    /// spelling as `name` except wind and pressure, which match the
    /// sampler, the weather API and the manifest (`wind_avg_mph`,
    /// `pressure_inhg`) so a bus source's wind and pressure show in the
    /// sparkline history too.
    pub fn history_key(self) -> &'static str {
        use WeatherField::*;
        match self {
            WindMph => "wind_avg_mph",
            PressureInHg => "pressure_inhg",
            other => other.name(),
        }
    }
}

/// Every field, in declaration order.
pub const ALL: &[WeatherField] = &[
    WeatherField::AirTempF,
    WeatherField::DewPointF,
    WeatherField::RhPct,
    WeatherField::WindMph,
    WeatherField::WindGustMph,
    WeatherField::WindBearingDeg,
    WeatherField::SolarWm2,
    WeatherField::UvIndex,
    WeatherField::Illuminance,
    WeatherField::PressureInHg,
    WeatherField::RainTodayIn,
    WeatherField::RainIntensityInHr,
    WeatherField::RainTypeStr,
    WeatherField::LightningCount,
    WeatherField::LightningDistanceMi,
    WeatherField::Et0Today,
    WeatherField::FlowGpm,
    WeatherField::FlowTotalGalToday,
    WeatherField::ForecastDaily,
    WeatherField::ForecastHourly,
    WeatherField::Pop,
    WeatherField::LeafWetness,
    WeatherField::WindLullMph,
    WeatherField::RapidWindMph,
    WeatherField::RapidWindBearingDeg,
    WeatherField::BatteryV,
    WeatherField::RainLastMinIn,
    WeatherField::PrecipType,
];

#[cfg(test)]
mod field_name_tests {
    use super::*;

    #[test]
    fn every_field_round_trips_through_its_name() {
        for f in ALL {
            assert_eq!(WeatherField::parse(f.name()), Some(*f), "{}", f.name());
            assert!(!f.label().is_empty());
        }
        assert_eq!(WeatherField::parse("no_such_field"), None);
        assert_eq!(WeatherField::WindMph.history_key(), "wind_avg_mph");
        assert_eq!(WeatherField::RhPct.history_key(), "rh_pct");
    }
}

#[derive(Debug, Clone, Default)]
pub struct SourceCaps {
    pub live_current: bool,
    pub hourly_forecast_hours: u32,
    pub daily_forecast_days: u32,
    pub radar_tiles: bool,
    pub et0_native: bool,
    pub fields: HashSet<WeatherField>,
}

#[derive(Debug, Clone)]
pub enum SourceEvent {
    /// A declared model's result, consumed only by the track bridge. It cannot
    /// become the merged forecast or a current observation. Generation rejects
    /// late replies from removed/reconfigured tracks.
    ForecastTrack {
        track_id: String,
        generation: u64,
        result: Result<crate::forecast::snapshot::ForecastSnapshot, Box<crate::failure::Failure>>,
    },
    /// Live observation update. The engine fans this into MergedSnapshot.
    Observation {
        source_id: String,
        fields: Vec<(WeatherField, f64)>,
        at_epoch: i64,
    },
    /// A zone-qualified channel reading that is NOT a global WeatherField:
    /// a per-zone soil-moisture probe. The merge bus is typed by
    /// WeatherField, which cannot disambiguate "the back yard's soil" from
    /// "the front yard's soil" (both would be `RhPct`), so a zone-bound soil
    /// subscription emits this instead. The bus recorder persists it to
    /// sensor_history verbatim under `key`, making it a discoverable soil
    /// channel that a zone binds via `source:<source_id>:<key>` exactly like
    /// a native Ecowitt `soilmoisture<N>` channel. `key` is the canonical
    /// soil-channel key (e.g. `soilmoisture_<zone_slug>`).
    KeyedReading {
        source_id: String,
        key: String,
        value: f64,
        at_epoch: i64,
    },
    /// A full forecast snapshot from a forecast-capable source (Open-Meteo,
    /// NWS, OpenWeather, PirateWeather, Met.no). The `forecast_bridge` merges
    /// these into the shared ForecastStore using per-source priority, so the
    /// user's CHOSEN forecast source drives the forecast instead of a single
    /// hardcoded provider. Carries the whole snapshot (daily + hourly +
    /// past_daily + timezone) the source built from its own API response.
    Forecast {
        source_id: String,
        snapshot: crate::forecast::snapshot::ForecastSnapshot,
        at_epoch: i64,
    },
    /// Reachability change. The engine surfaces this in per-source status badges.
    Reachability { source_id: String, reachable: bool },
    /// Current poll failure, or a successful poll clearing it. Measurement
    /// freshness is independent; this event never refreshes observations.
    Diagnostic {
        source_id: String,
        failure: Option<Box<super::source_error::SourceFailure>>,
        at_epoch: i64,
    },
    /// Lightning strikes a detection network reported: the station's own
    /// detector one at a time, the community network in batches. The
    /// live store keeps the last hour in a ring for the dashboard and the
    /// radar layer; nothing in the engine gates on a strike.
    Strikes {
        source_id: String,
        strikes: Vec<crate::tempest::packets::StrikeEvent>,
    },
    /// Which hardware is talking: the station and hub serials a LAN
    /// station reports in every packet. Published on the first packet and
    /// whenever either changes; the footer shows them and "a station is
    /// present" reads off them.
    Identity {
        source_id: String,
        station_serial: String,
        hub_serial: String,
    },
}

pub type SourceBus = broadcast::Sender<SourceEvent>;

/// Cooperative shutdown for spawned source tasks. Receivers await this and
/// drop their loops; the runtime aborts any task still alive 5s after.
pub type ShutdownSignal = tokio::sync::watch::Receiver<bool>;

#[async_trait]
pub trait WeatherSource: Send + Sync {
    fn id(&self) -> &str;
    fn capabilities(&self) -> SourceCaps;
    /// Per-field merge priority. Higher wins. Sources unable to produce the
    /// field return i32::MIN.
    fn priority(&self, field: WeatherField) -> i32;
    async fn run(self: Arc<Self>, bus: SourceBus, shutdown: ShutdownSignal) -> anyhow::Result<()>;
}
