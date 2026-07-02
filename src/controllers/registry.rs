// ControllerRegistry. Holds the configured set of IrrigationController
// instances and exposes default_or_named() for dispatch. Construction
// is driven by config.controllers[]; the registry is hot-swappable so a
// PUT /api/config that changes the default controller takes effect on
// the next dispatch without restarting the runtime.
//
// IMPORTANT: the registry is only populated when BOTH a config file and
// the persistence DB are available (build_controllers needs a RunsStore).
// With no DB mounted the registry stays empty and ALL watering dispatch
// (scheduled and manual) is dead: schedulers log "no default controller"
// per attempt and POST /action answers 503. main.rs logs a boot-time
// tracing::error! when controllers are configured but the DB is missing,
// so the gap is loud instead of a silent dry lawn.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;

use crate::ports::irrigation_controller::IrrigationController;

#[derive(Clone)]
pub struct ControllerRegistry {
    inner: Arc<ArcSwap<RegistryState>>,
}

#[derive(Default)]
struct RegistryState {
    by_id: HashMap<String, Arc<dyn IrrigationController>>,
    default_id: Option<String>,
}

impl ControllerRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(ArcSwap::from_pointee(RegistryState::default())),
        }
    }

    /// Replace the registry contents atomically. Used by the hot-reload
    /// path when config.controllers changes.
    pub fn set(&self, controllers: Vec<(Arc<dyn IrrigationController>, bool /* default */)>) {
        let mut by_id = HashMap::new();
        let mut default_id = None;
        for (c, is_default) in controllers {
            if is_default {
                default_id = Some(c.id().to_string());
            }
            by_id.insert(c.id().to_string(), c);
        }
        // If no explicit default, pick the first one (deterministic via sort).
        if default_id.is_none() {
            let mut keys: Vec<&String> = by_id.keys().collect();
            keys.sort();
            default_id = keys.into_iter().next().cloned();
        }
        self.inner
            .store(Arc::new(RegistryState { by_id, default_id }));
    }

    pub fn get(&self, id: &str) -> Option<Arc<dyn IrrigationController>> {
        self.inner.load().by_id.get(id).cloned()
    }

    pub fn default(&self) -> Option<Arc<dyn IrrigationController>> {
        let s = self.inner.load();
        s.default_id
            .as_ref()
            .and_then(|id| s.by_id.get(id).cloned())
    }

    /// Pick `id` if provided, otherwise the default. Convenience for the
    /// /api/irrigation dispatch path which accepts an optional controller
    /// override in the request payload.
    pub fn default_or_named(&self, id: Option<&str>) -> Option<Arc<dyn IrrigationController>> {
        match id {
            Some(name) => self.get(name),
            None => self.default(),
        }
    }

    pub fn ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.inner.load().by_id.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Boot reconciliation (P0-1): close every zone on every registered
    /// controller. Called once at startup, before the first dispatch window, so a
    /// valve left open by a crash or redeploy (especially the MQTT path, whose
    /// shutoff is an in-process timer that dies with the process) is closed on the
    /// next boot. A legitimately-running cycle is re-evaluated by the scheduler;
    /// cutting a run short is the safe direction versus a stranded-open valve.
    /// Best-effort: an unreachable controller is logged and returned in the failed
    /// list, never fatal. Returns the ids whose stop_all errored.
    pub async fn reconcile_stop_all(&self) -> Vec<String> {
        // Clone the Arcs out before any await so the ArcSwap load guard is not
        // held across the controller network calls.
        let controllers: Vec<(String, Arc<dyn IrrigationController>)> = {
            let s = self.inner.load();
            s.by_id
                .iter()
                .map(|(id, c)| (id.clone(), c.clone()))
                .collect()
        };
        let mut failed = Vec::new();
        for (id, c) in controllers {
            match c.stop_all().await {
                Ok(()) => tracing::info!(controller = %id, "boot reconcile: closed all zones"),
                Err(e) => {
                    tracing::warn!(controller = %id, error = %e, "boot reconcile: stop_all failed (controller unreachable?)");
                    failed.push(id);
                }
            }
        }
        failed
    }
}

impl Default for ControllerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::schema::DryRunConfig;
    use crate::controllers::dry_run::DryRunController;

    fn dry(id: &str) -> Arc<dyn IrrigationController> {
        Arc::new(DryRunController::new(
            id,
            DryRunConfig {
                simulate_runs: false,
            },
            None,
        ))
    }

    #[test]
    fn default_returns_marked_controller() {
        let r = ControllerRegistry::new();
        r.set(vec![(dry("a"), false), (dry("b"), true), (dry("c"), false)]);
        assert_eq!(r.default().unwrap().id(), "b");
    }

    #[test]
    fn default_falls_back_to_first_sorted_id() {
        let r = ControllerRegistry::new();
        r.set(vec![(dry("b"), false), (dry("a"), false)]);
        // No explicit default -> first sorted id wins.
        assert_eq!(r.default().unwrap().id(), "a");
    }

    #[test]
    fn default_or_named_routes_correctly() {
        let r = ControllerRegistry::new();
        r.set(vec![(dry("primary"), true), (dry("backup"), false)]);
        assert_eq!(r.default_or_named(None).unwrap().id(), "primary");
        assert_eq!(r.default_or_named(Some("backup")).unwrap().id(), "backup");
        assert!(r.default_or_named(Some("missing")).is_none());
    }

    #[test]
    fn set_replaces_atomic() {
        let r = ControllerRegistry::new();
        r.set(vec![(dry("a"), true)]);
        assert_eq!(r.default().unwrap().id(), "a");
        r.set(vec![(dry("b"), true)]);
        assert_eq!(r.default().unwrap().id(), "b");
        assert!(r.get("a").is_none());
    }

    #[test]
    fn ids_returns_sorted() {
        let r = ControllerRegistry::new();
        r.set(vec![
            (dry("z"), false),
            (dry("a"), false),
            (dry("m"), false),
        ]);
        assert_eq!(
            r.ids(),
            vec!["a".to_string(), "m".to_string(), "z".to_string()]
        );
    }

    // --- P0-1 boot reconciliation -------------------------------------------
    use crate::ports::irrigation_controller::{
        ControllerCaps, ControllerError, ControllerResult, ControllerStatus, RunHandle, RunRecord,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Counts stop_all calls; optionally fails to model an unreachable controller.
    struct Recorder {
        id: String,
        stop_all_calls: Arc<AtomicUsize>,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl IrrigationController for Recorder {
        fn id(&self) -> &str {
            &self.id
        }
        fn supports(&self) -> ControllerCaps {
            ControllerCaps {
                flow_meter: false,
                rain_sensor: false,
                master_valve: false,
                multi_zone_parallel: false,
                history_query: false,
                remote_program_upload: false,
            }
        }
        async fn run_zone(&self, slug: &str, duration_s: u32) -> ControllerResult<RunHandle> {
            Ok(RunHandle {
                controller_id: self.id.clone(),
                zone_slug: slug.to_string(),
                started_epoch: 0,
                planned_duration_s: duration_s,
                provider_ref: None,
            })
        }
        async fn stop_zone(&self, _slug: &str) -> ControllerResult<()> {
            Ok(())
        }
        async fn stop_all(&self) -> ControllerResult<()> {
            self.stop_all_calls.fetch_add(1, Ordering::SeqCst);
            if self.fail {
                Err(ControllerError::Transport("unreachable at boot".into()))
            } else {
                Ok(())
            }
        }
        async fn status(&self) -> ControllerResult<ControllerStatus> {
            Ok(ControllerStatus {
                reachable: true,
                master_enabled: None,
                water_level_pct: None,
                rain_sensor_tripped: None,
                current_program: None,
                zone_states: vec![],
                flow_gpm: None,
                flow_connected: false,
                firmware: None,
            })
        }
        async fn run_history(&self, _since_epoch: i64) -> ControllerResult<Vec<RunRecord>> {
            Ok(vec![])
        }
    }

    fn rec(id: &str, calls: Arc<AtomicUsize>, fail: bool) -> Arc<dyn IrrigationController> {
        Arc::new(Recorder {
            id: id.to_string(),
            stop_all_calls: calls,
            fail,
        })
    }

    #[tokio::test]
    async fn reconcile_stop_all_closes_every_controller() {
        let calls = Arc::new(AtomicUsize::new(0));
        let r = ControllerRegistry::new();
        r.set(vec![
            (rec("a", calls.clone(), false), true),
            (rec("b", calls.clone(), false), false),
        ]);
        let failed = r.reconcile_stop_all().await;
        assert!(failed.is_empty());
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "stop_all on every controller"
        );
    }

    #[tokio::test]
    async fn reconcile_stop_all_reports_unreachable_without_failing() {
        let calls = Arc::new(AtomicUsize::new(0));
        let r = ControllerRegistry::new();
        r.set(vec![
            (rec("ok", calls.clone(), false), true),
            (rec("bad", calls.clone(), true), false),
        ]);
        let failed = r.reconcile_stop_all().await;
        assert_eq!(failed, vec!["bad".to_string()]);
        assert_eq!(
            calls.load(Ordering::SeqCst),
            2,
            "both attempted, best-effort"
        );
    }
}
