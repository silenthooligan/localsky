// WeatherSource port. Every weather data adapter (Tempest UDP, Tempest WS,
// Open-Meteo, Ecowitt LAN, NWS, etc.) implements this trait.
//
// Adapters own their own polling/listener task and publish into a shared
// SourceBus. The engine merges across sources using per-field priority().

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
