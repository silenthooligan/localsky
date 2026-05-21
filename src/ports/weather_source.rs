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
    ForecastDaily,
    ForecastHourly,
    Pop,
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
    async fn run(
        self: Arc<Self>,
        bus: SourceBus,
        shutdown: ShutdownSignal,
    ) -> anyhow::Result<()>;
}
