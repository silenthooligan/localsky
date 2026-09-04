// SQLite persistence. Phase 4 wires migrations + config snapshots first;
// Phase 4B+ wires the new runs schema, sensor_history, verdict_history,
// and the DB-backed edge detector.
//
//   runner.rs          - hand-rolled migration runner (M0001..)
//   migrations/*.sql   - baked into the binary via include_str!
//   config_snapshots.rs - last 20 config versions for rollback (M0002)
//
// Future modules (Phase 4B+):
//   pool.rs            - Connection wrapper (spawn_blocking entrypoints)
//   runs.rs            - run history CRUD; replaces history/db.rs
//   sensor_history.rs  - generic source/field/value time series
//   verdict_history.rs - daily decision log with inputs_json for replay
//   push_subscriptions.rs - moved from src/push/store.rs
//   edge_detector.rs   - DB-backed zone-edge state (no in-memory loss on restart)

pub mod active_runs;
pub mod config_snapshots;
pub mod forecast_observations;
pub mod irrigation_control;
pub mod runner;
pub mod runs;
pub mod sensor_history;
pub mod tuning_dismissals;
pub mod tuning_report;
pub mod verdict_history;
pub mod weather_sampler;

pub use active_runs::{ActiveRun, ActiveRunsError, ActiveRunsStore};
pub use config_snapshots::ConfigSnapshotStore;
pub use forecast_observations::{
    Et0LedgerDay, ForecastObservationsError, ForecastObservationsStore, ObservedRainDay,
    ObservedRainWindow,
};
pub use irrigation_control::{IrrigationControlState, IrrigationControlStore};
pub use runner::{run as run_migrations, Migration, MigrationError, MIGRATIONS};
pub use runs::{NewRun, RunRow, RunsError, RunsStore};
pub use sensor_history::{Reading, SensorHistoryError, SensorHistoryStore};
pub use tuning_dismissals::{
    DismissalRow, InviteState, TuningDismissalsError, TuningDismissalsStore, INSTALL_SCOPE,
    SNOOZE_DAYS, SOIL_INVITE_FIELD,
};
pub use tuning_report::{TuningReportStateError, TuningReportStateStore};
pub use verdict_history::{NewVerdict, VerdictHistoryError, VerdictHistoryStore, VerdictRow};
pub use weather_sampler::spawn_weather_sampler;
