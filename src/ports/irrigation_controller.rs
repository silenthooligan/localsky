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
}

pub type ControllerResult<T> = Result<T, ControllerError>;

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
}
