// DryRun controller. No-op irrigation adapter that records intent
// instead of dispatching to hardware. Used by demo mode + tests + the
// "what would the engine have done" smoke screen during config changes.
//
// When `simulate_runs` is true, run_zone synthesizes a completed row in
// the runs table (status='completed', duration_s = requested) so the
// dashboard renders activity. Otherwise it just logs.

use std::collections::HashSet;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::Mutex;
use tracing::info;

use crate::config::schema::DryRunConfig;
use crate::persistence::{NewRun, RunsStore};
use crate::ports::irrigation_controller::{
    ControllerCaps, ControllerResult, ControllerStatus, IrrigationController, RunHandle, RunRecord,
    ZoneRuntimeStatus,
};

pub struct DryRunController {
    id: String,
    config: DryRunConfig,
    runs: Option<RunsStore>,
    /// Lightweight in-memory state so `status()` reflects pretend runs.
    pretend_running: Arc<Mutex<HashSet<String>>>,
}

impl DryRunController {
    pub fn new(id: impl Into<String>, config: DryRunConfig, runs: Option<RunsStore>) -> Self {
        Self {
            id: id.into(),
            config,
            runs,
            pretend_running: Arc::new(Mutex::new(HashSet::new())),
        }
    }
}

#[async_trait]
impl IrrigationController for DryRunController {
    fn id(&self) -> &str {
        &self.id
    }

    fn supports(&self) -> ControllerCaps {
        ControllerCaps {
            flow_meter: false,
            rain_sensor: false,
            master_valve: false,
            multi_zone_parallel: true,
            history_query: false,
            remote_program_upload: false,
        }
    }

    async fn run_zone(&self, slug: &str, duration_s: u32) -> ControllerResult<RunHandle> {
        info!(
            controller = self.id,
            zone = slug,
            duration_s,
            "dry_run: would have run zone"
        );
        let start = now_epoch();
        if self.config.simulate_runs {
            if let Some(store) = &self.runs {
                let _ = store
                    .insert_completed(
                        NewRun {
                            zone_slug: slug.to_string(),
                            start_epoch: start,
                            source: "dry_run".to_string(),
                            controller_id: self.id.clone(),
                            planned_duration_s: duration_s,
                            skip_reason: None,
                            et0_mm: None,
                            etc_mm: None,
                            cycle_index: None,
                            cycle_count: None,
                        },
                        start + duration_s as i64,
                        duration_s,
                        None,
                    )
                    .await;
            }
        } else {
            self.pretend_running.lock().await.insert(slug.to_string());
        }
        Ok(RunHandle {
            controller_id: self.id.clone(),
            zone_slug: slug.to_string(),
            started_epoch: start,
            planned_duration_s: duration_s,
            provider_ref: None,
        })
    }

    async fn stop_zone(&self, slug: &str) -> ControllerResult<()> {
        info!(
            controller = self.id,
            zone = slug,
            "dry_run: would have stopped zone"
        );
        self.pretend_running.lock().await.remove(slug);
        Ok(())
    }

    async fn stop_all(&self) -> ControllerResult<()> {
        info!(
            controller = self.id,
            "dry_run: would have stopped all zones"
        );
        self.pretend_running.lock().await.clear();
        Ok(())
    }

    async fn status(&self) -> ControllerResult<ControllerStatus> {
        let running = self.pretend_running.lock().await.clone();
        Ok(ControllerStatus {
            reachable: true,
            master_enabled: Some(true),
            water_level_pct: Some(100.0),
            rain_sensor_tripped: Some(false),
            current_program: None,
            zone_states: running
                .into_iter()
                .map(|slug| ZoneRuntimeStatus {
                    slug,
                    running: true,
                    remaining_s: None,
                    last_run_epoch: None,
                })
                .collect(),
            flow_gpm: None,
            firmware: Some("dry_run".into()),
        })
    }

    async fn run_history(&self, _since_epoch: i64) -> ControllerResult<Vec<RunRecord>> {
        // DryRun doesn't track durable history beyond what the runs
        // table already holds when simulate_runs=true. Return empty so
        // the scheduler doesn't try to reconcile against fake data.
        Ok(Vec::new())
    }

    async fn discover_zones(
        &self,
    ) -> ControllerResult<Vec<crate::ports::irrigation_controller::DiscoveredZone>> {
        // Sample stations so the wizard's full add -> test -> scan ->
        // import-zones flow is experiential with zero hardware. They
        // import as ordinary editable ZoneConfig stubs.
        use crate::ports::irrigation_controller::DiscoveredZone;
        Ok(vec![
            DiscoveredZone {
                station_id: "1".into(),
                name: "Front Lawn".into(),
            },
            DiscoveredZone {
                station_id: "2".into(),
                name: "Back Lawn".into(),
            },
            DiscoveredZone {
                station_id: "3".into(),
                name: "Garden Beds".into(),
            },
        ])
    }
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctrl() -> DryRunController {
        DryRunController::new(
            "dry_run_test",
            DryRunConfig {
                simulate_runs: false,
            },
            None,
        )
    }

    #[tokio::test]
    async fn run_then_status_shows_running() {
        let c = ctrl();
        c.run_zone("back_yard", 600).await.unwrap();
        let s = c.status().await.unwrap();
        assert!(s.reachable);
        assert!(s
            .zone_states
            .iter()
            .any(|z| z.slug == "back_yard" && z.running));
    }

    #[tokio::test]
    async fn stop_clears_running() {
        let c = ctrl();
        c.run_zone("front_yard", 600).await.unwrap();
        c.stop_zone("front_yard").await.unwrap();
        let s = c.status().await.unwrap();
        assert!(s.zone_states.is_empty());
    }

    #[tokio::test]
    async fn stop_all_clears_everything() {
        let c = ctrl();
        c.run_zone("a", 300).await.unwrap();
        c.run_zone("b", 300).await.unwrap();
        c.stop_all().await.unwrap();
        let s = c.status().await.unwrap();
        assert!(s.zone_states.is_empty());
    }

    #[tokio::test]
    async fn caps_advertises_multi_zone_parallel() {
        let c = ctrl();
        let caps = c.supports();
        assert!(caps.multi_zone_parallel);
        assert!(!caps.flow_meter);
    }
}
