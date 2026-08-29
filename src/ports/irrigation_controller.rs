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
    /// The controller reports a water-level / seasonal-adjust percentage in
    /// `ControllerStatus.water_level_pct` (OpenSprinkler's `wl`). Defaults
    /// false: most adapters return `water_level_pct: None`, and publishing a
    /// fabricated level for them is exactly the defect this bit gates.
    /// `#[serde(default)]` keeps older serialized caps deserializing.
    #[serde(default)]
    pub water_level: bool,
    /// `stop_zone` stops ONLY the named zone. True for every adapter except
    /// Rachio, whose public API's only stop operation is device-wide
    /// `stop_water`: on a `per_zone_stop: false` controller a zone-stop
    /// stops all watering on the device, and every stop consumer (reaper,
    /// manual Stop) must treat it as a device-wide stop and say so. The
    /// smart-morning reaper also widens its enforcement grace for these
    /// controllers (cloud latency; see `CLOUD_DEVICE_STOP_GRACE_S`).
    /// `#[serde(default = "default_true_cap")]` keeps older serialized caps
    /// deserializing as "per-zone stop works", the pre-existing behavior.
    #[serde(default = "default_true_cap")]
    pub per_zone_stop: bool,
}

fn default_true_cap() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ZoneRuntimeStatus {
    pub slug: String,
    pub running: bool,
    pub remaining_s: Option<u32>,
    pub last_run_epoch: Option<i64>,
    /// False when the adapter could NOT determine live running state on this
    /// poll (a cloud response shape it did not recognize, or the running
    /// endpoint failed while the rest of the status succeeded). In that case
    /// `running` carries the last KNOWN value rather than a fabricated
    /// "idle", so the run-edge observer never records a false falling edge
    /// from a parse surprise. `#[serde(default = "default_running_known")]`
    /// keeps older serialized statuses deserializing as known.
    #[serde(default = "default_running_known")]
    pub running_known: bool,
}

fn default_running_known() -> bool {
    true
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
    /// A flow sensor is physically CONNECTED to the controller, distinct from
    /// the type-level `Capabilities.flow_meter` (does the controller *support*
    /// flow) and from `flow_gpm` (is it reading flow *right now*). True only
    /// when the controller's own configuration reports a flow sensor wired in
    /// (OpenSprinkler: sensor input 1 set to flow type, `sn1t` == 2,
    /// corroborated by a present click-rate). Controllers that can't report
    /// presence leave this false, so a user with no flow meter never sees a
    /// phantom "detected" sensor. `#[serde(default)]` keeps older snapshots and
    /// any partial JSON deserializing to false.
    #[serde(default)]
    pub flow_connected: bool,
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
    /// True when this controller never actuates hardware (a dry-run
    /// stand-in): running state it reports is pretend water, and the
    /// run-edge observer must record it as such (source 'dry_run') so
    /// it never counts as watering evidence.
    fn simulated(&self) -> bool {
        false
    }
    /// The remaining request allowance the controller's upstream last
    /// reported, when the adapter tracks one (a cloud controller with a
    /// daily budget). `None` for adapters talking to local hardware, and
    /// `None` before the upstream has reported a number: absent stays
    /// absent rather than becoming a zero that reads as "exhausted".
    /// Carried in the action endpoint's rate-limit error body so an
    /// exhausted budget says so instead of arriving as a bare status.
    fn rate_limit_remaining(&self) -> Option<String> {
        None
    }
    /// The zone slugs this controller can actually dispatch, i.e. the keys
    /// of its zone map. Empty for adapters that carry no map of their own
    /// (they derive a per-zone binding from `controller_station` at build
    /// time, so an unbound zone is already visible in the config).
    ///
    /// The action endpoint puts these in the unknown-zone 400 body. A cloud
    /// controller's map is keyed by the slugified VENDOR zone name, while
    /// dispatch looks it up by the LocalSky zone slug, and nothing forces
    /// the two to agree. Without the keys in the error, a user whose zone
    /// is named differently from the vendor's has no way to see why the
    /// lookup missed.
    fn mapped_zone_slugs(&self) -> Vec<String> {
        Vec::new()
    }
    /// How long this controller's live status readback can lag a dispatch,
    /// in seconds, for an adapter that polls its upstream on a throttle.
    /// `None` when state is read on demand, so a dispatched change is
    /// visible on the next refresher tick. The action endpoint returns it
    /// on a successful zone action so the UI can say a run was accepted
    /// and confirmation is still pending, rather than implying failure
    /// when its own confirm window is shorter than this interval.
    fn status_poll_interval_s(&self) -> Option<u32> {
        None
    }
    async fn run_zone(&self, slug: &str, duration_s: u32) -> ControllerResult<RunHandle>;
    async fn stop_zone(&self, slug: &str) -> ControllerResult<()>;
    async fn stop_all(&self) -> ControllerResult<()>;
    async fn status(&self) -> ControllerResult<ControllerStatus>;
    /// Backfill from the controller's own history if it supports the query.
    /// Adapters that can't query history return an empty Vec.
    async fn run_history(&self, since_epoch: i64) -> ControllerResult<Vec<RunRecord>>;

    /// Enumerate the controller's zones/stations for onboarding (the
    /// wizard's "scan zones", auto-populates ZoneConfig). Adapters that
    /// can't enumerate (MQTT/ESPHome/HA) return `Unsupported` by default.
    async fn discover_zones(&self) -> ControllerResult<Vec<DiscoveredZone>> {
        Err(ControllerError::Unsupported("zone discovery".into()))
    }
}
