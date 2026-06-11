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
    /// Per-zone slug → epoch the zone was first seen running. None
    /// means we last saw it idle (or never). On running→idle we
    /// take this value as the run's start, write the row, and clear.
    seen_running: HashMap<String, i64>,
    /// Last observed (verdict, reason) pair. None until the first poll
    /// builds a valid skip_check; thereafter holds the most recent
    /// transition so we can detect changes against the next poll.
    last_decision: Option<(String, String)>,
}

impl IngestState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Inspect a freshly-built snapshot, write any completed runs and
    /// verdict transitions. `db` is the SQLite handle; `snapshot` is the
    /// in-memory state from the refresher's last successful poll.
    pub async fn observe(&mut self, db: &Arc<Mutex<Connection>>, snapshot: &IrrigationSnapshot) {
        let now = snapshot.last_refresh_epoch;
        for zone in &snapshot.zones {
            let was_running = self.seen_running.contains_key(&zone.slug);
            if zone.running && !was_running {
                // Start of a run.
                self.seen_running.insert(zone.slug.clone(), now);
            } else if !zone.running && was_running {
                // End of a run — emit the row.
                let start = self.seen_running.remove(&zone.slug).unwrap_or(now);
                let duration = (now - start).max(0);
                let rec = RunRecord {
                    zone: zone.slug.clone(),
                    start_epoch: start,
                    duration_s: duration,
                    skip_reason: None,
                };
                if let Err(e) = record_run(db.clone(), rec).await {
                    tracing::warn!("history insert failed: {e:#}");
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
            return;
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
    }
}
