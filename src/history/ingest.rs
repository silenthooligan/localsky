// Per-zone run-edge detector. The refresher task already polls HA
// every 10s; on each cycle it calls `IngestState::observe(snapshot)`
// which compares the running flag of each zone against the previous
// observation and writes a row to SQLite when a zone goes from
// running→idle.
//
// Sub-10s blips are missed (acceptable; a tap-test for less than 10s
// isn't a real run). For runs that span the poll boundary, we record
// the start at the FIRST observation that saw the zone running and
// duration as (now - start).

use crate::ha::snapshot::IrrigationSnapshot;
use crate::history::db::{record_decision, record_run};
use crate::history::types::{DecisionRecord, RunRecord};
use rusqlite::Connection;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Default)]
pub struct IngestState {
    /// Per-zone slug → (epoch the zone was first seen running, whether a
    /// non-actuating dry-run controller was reporting it). None means we
    /// last saw it idle (or never). On running→idle we take the start,
    /// write the row (source 'dry_run' when the latch says the water was
    /// pretend), and clear.
    seen_running: HashMap<String, (i64, bool)>,
    /// Last observed (verdict, reason) pair. None until the first poll
    /// builds a valid skip_check; thereafter holds the most recent
    /// transition so we can detect changes against the next poll.
    last_decision: Option<(String, String)>,
}

impl IngestState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Which zone slugs are currently reported running by a SIMULATED
    /// (never-actuating) controller. A non-simulating DryRunController
    /// surfaces pretend_running through the native readback; without
    /// this check the observer would persist genuine-looking
    /// 'ha_refresher' rows for water that never fell, and the balance
    /// would credit it.
    pub async fn simulated_running_slugs(
        controllers: &crate::controllers::registry::ControllerRegistry,
    ) -> std::collections::HashSet<String> {
        let mut out = std::collections::HashSet::new();
        for id in controllers.ids() {
            let Some(c) = controllers.get(&id) else {
                continue;
            };
            if !c.simulated() {
                continue;
            }
            if let Ok(status) = c.status().await {
                for z in status.zone_states {
                    if z.running {
                        out.insert(z.slug);
                    }
                }
            }
        }
        out
    }

    /// Inspect a freshly-built snapshot, write any completed runs and
    /// verdict transitions. `db` is the SQLite handle; `snapshot` is the
    /// in-memory state from the refresher's last successful poll.
    /// `simulated_running` marks zones whose running state comes from a
    /// dry-run controller this tick (empty on HA-sourced installs).
    ///
    /// Returns how many RUN rows were written this tick (falling edges),
    /// so the caller can invalidate anything cached over the runs table
    /// AFTER the new evidence is persisted, never a tick before it.
    pub async fn observe(
        &mut self,
        db: &Arc<Mutex<Connection>>,
        snapshot: &IrrigationSnapshot,
        simulated_running: &std::collections::HashSet<String>,
    ) -> usize {
        let mut runs_written = 0usize;
        let now = snapshot.last_refresh_epoch;
        for zone in &snapshot.zones {
            let was_running = self.seen_running.contains_key(&zone.slug);
            if zone.running && !was_running {
                // Start of a run. Latch whether the water is pretend at
                // the rising edge (the controller set can change while a
                // run is in flight; the entry state is the honest one).
                self.seen_running.insert(
                    zone.slug.clone(),
                    (now, simulated_running.contains(&zone.slug)),
                );
            } else if !zone.running && was_running {
                // End of a run, emit the row.
                let (start, dry_run) = self.seen_running.remove(&zone.slug).unwrap_or((now, false));
                // APPROXIMATE duration. Both `start` and `now` are
                // snapshot.last_refresh_epoch values, so this is the span
                // between the first poll that saw the zone running and the
                // first poll that saw it idle: it is quantized to the ~10s
                // refresher boundary and can read up to one poll short or long
                // of the true on-time. This matters most for a cycle-soak run,
                // whose valve toggles on/off per segment: the observer records
                // one such approximate row per ON segment rather than a single
                // whole-cycle row, so several short rows is expected here and
                // not a runtime error. Treat every observer-written
                // duration_s as approximate (~10s); the scheduler's own rows
                // (history::db, source "smart_morning") carry the planned
                // intent and are the exact figure when one is needed.
                let duration = (now - start).max(0);
                let rec = RunRecord {
                    zone: zone.slug.clone(),
                    start_epoch: start,
                    duration_s: duration,
                    skip_reason: None,
                    // Pretend water from a non-simulating dry-run
                    // controller is recorded honestly as such, never as
                    // watering evidence.
                    source: if dry_run {
                        "dry_run".to_string()
                    } else {
                        "ha_refresher".to_string()
                    },
                    status: String::new(),
                };
                match record_run(db.clone(), rec).await {
                    Ok(()) => runs_written += 1,
                    Err(e) => tracing::warn!("history insert failed: {e:#}"),
                }
            }
        }

        // Persist verdict transitions. Compare both verdict and reason so
        // a "skip -> skip with new reason" still records (e.g. the reason
        // shifted from "Tomorrow rain" to "Live wind"). The very first
        // observation seeds last_decision without writing; we only care
        // about post-startup transitions to avoid a duplicate row on
        // every container restart.
        let verdict = snapshot.skip_check.verdict.clone();
        let reason = snapshot.skip_check.reason.clone();
        if verdict.is_empty() {
            return runs_written;
        }
        let current = (verdict, reason);
        let changed = match &self.last_decision {
            None => false,
            Some(prev) => *prev != current,
        };
        if changed {
            let (v, r) = current.clone();
            let rec = DecisionRecord {
                epoch: now,
                verdict: v,
                reason: r,
                trace: None,
            };
            // Persist the structured trace captured at decision time so the
            // Rule Lab can replay why this day decided the way it did.
            let trace_json = snapshot
                .decision_trace
                .as_ref()
                .and_then(|t| serde_json::to_string(t).ok())
                .unwrap_or_default();
            if let Err(e) = record_decision(db.clone(), rec, trace_json).await {
                tracing::warn!("decision insert failed: {e:#}");
            }
        }
        self.last_decision = Some(current);
        runs_written
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ha::snapshot::ZoneState;

    fn mem() -> Arc<Mutex<Connection>> {
        let mut c = Connection::open_in_memory().unwrap();
        crate::persistence::run_migrations(&mut c).unwrap();
        Arc::new(Mutex::new(c))
    }

    fn snap(epoch: i64, running: bool) -> IrrigationSnapshot {
        IrrigationSnapshot {
            last_refresh_epoch: epoch,
            zones: vec![ZoneState {
                slug: "front".into(),
                running,
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    /// The tick sequence the balance-cache invalidation keys on: the
    /// rising edge writes nothing, the falling edge writes the run row
    /// and reports it, so the caller invalidates on the tick where the
    /// row actually exists (one tick after the run was last seen
    /// running), never a tick before.
    #[tokio::test]
    async fn observe_reports_run_rows_on_the_falling_edge() {
        let db = mem();
        let mut ingest = IngestState::new();
        let none = std::collections::HashSet::new();
        assert_eq!(ingest.observe(&db, &snap(1_000, false), &none).await, 0);
        // Rising edge: latched, nothing written yet.
        assert_eq!(ingest.observe(&db, &snap(1_010, true), &none).await, 0);
        // Still running: nothing written.
        assert_eq!(ingest.observe(&db, &snap(1_020, true), &none).await, 0);
        // Falling edge: the completed row lands and is reported.
        assert_eq!(ingest.observe(&db, &snap(1_030, false), &none).await, 1);
        // Idle again: nothing further.
        assert_eq!(ingest.observe(&db, &snap(1_040, false), &none).await, 0);
    }
}
