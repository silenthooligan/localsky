// IrrigationController port. Every controller adapter (OpenSprinkler direct,
// HA service call, ESPHome native, Rachio cloud, DryRun) implements this.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum ControllerError {
    #[error("controller offline")]
    Offline,
    #[error("zone unknown: {0}")]
    ZoneUnknown(String),
    #[error("rate limited")]
    RateLimited,
    #[error("auth failed")]
    AuthFailed,
    #[error("controller returned error: {0}")]
    Remote(String),
    #[error("transport error: {0}")]
    Transport(String),
    /// Adapter construction failed before any network call (typically
    /// the HTTP client builder rejected the config, e.g. TLS root
    /// loading failure). Distinct from Transport so operators can tell
    /// "I never got to make the request" apart from "the request
    /// failed." Returned from controller new() functions; runtime
    /// composition logs and skips the controller rather than panicking
    /// the whole container.
    #[error("init failed: {0}")]
    Init(String),
    /// The adapter doesn't support this operation (e.g. zone discovery on
    /// a fire-and-forget MQTT controller).
    #[error("unsupported: {0}")]
    Unsupported(String),
}

pub type ControllerResult<T> = Result<T, ControllerError>;

/// A zone/station enumerated from a controller during onboarding (the
/// wizard's "scan zones"). `station_id` is the controller-native id to
/// store in `ZoneConfig.controller_station` (OpenSprinkler: 1-based
/// station number as a string; Rachio: zone uuid).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DiscoveredZone {
    pub station_id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControllerCaps {
    pub flow_meter: bool,
    pub rain_sensor: bool,
    pub master_valve: bool,
    pub multi_zone_parallel: bool,
    pub history_query: bool,
    pub remote_program_upload: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZoneRuntimeStatus {
    pub slug: String,
    pub running: bool,
    pub remaining_s: Option<u32>,
    pub last_run_epoch: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControllerStatus {
    pub reachable: bool,
    pub master_enabled: Option<bool>,
    pub water_level_pct: Option<f64>,
    pub rain_sensor_tripped: Option<bool>,
    pub current_program: Option<String>,
    pub zone_states: Vec<ZoneRuntimeStatus>,
    pub flow_gpm: Option<f64>,
    pub firmware: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunHandle {
    pub controller_id: String,
    pub zone_slug: String,
    pub started_epoch: i64,
    pub planned_duration_s: u32,
    /// Provider-specific reference for cancellation (e.g. OpenSprinkler
    /// station index, ESPHome switch entity_id, HA service call ID).
    pub provider_ref: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunRecord {
    pub zone_slug: String,
    pub start_epoch: i64,
    pub end_epoch: Option<i64>,
    pub duration_s: Option<u32>,
    pub source: String,
}

#[async_trait]
pub trait IrrigationController: Send + Sync {
    fn id(&self) -> &str;
    fn supports(&self) -> ControllerCaps;
    async fn run_zone(&self, slug: &str, duration_s: u32) -> ControllerResult<RunHandle>;
    async fn stop_zone(&self, slug: &str) -> ControllerResult<()>;
    async fn stop_all(&self) -> ControllerResult<()>;
    async fn status(&self) -> ControllerResult<ControllerStatus>;
    /// Backfill from the controller's own history if it supports the query.
    /// Adapters that can't query history return an empty Vec.
    async fn run_history(&self, since_epoch: i64) -> ControllerResult<Vec<RunRecord>>;

    /// Enumerate the controller's zones/stations for onboarding (the
    /// wizard's "scan zones" — auto-populates ZoneConfig). Adapters that
    /// can't enumerate (MQTT/ESPHome/HA) return `Unsupported` by default.
    async fn discover_zones(&self) -> ControllerResult<Vec<DiscoveredZone>> {
        Err(ControllerError::Unsupported("zone discovery".into()))
    }
}
