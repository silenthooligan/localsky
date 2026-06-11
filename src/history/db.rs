// SQLite schema, open, and CRUD for the run-history table.
//
// The connection is held inside an `Arc<Mutex<Connection>>` so handlers
// can grab it from spawn_blocking. Single writer + readers — no need
// for a real pool at irrigation-traffic levels (a few inserts a day,
// a few reads per page-load).

use crate::history::types::{DecisionRecord, DecisionWindow, HistoryWindow, RunRecord};
use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

pub struct HistoryDb {
    conn: Arc<Mutex<Connection>>,
}

impl HistoryDb {
    /// Open or create the SQLite file at the given path and run all
    /// versioned migrations. Idempotent: re-opening an already-migrated
    /// database is a no-op.
    pub fn open(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let mut conn =
            Connection::open(&path).with_context(|| format!("open sqlite at {path:?}"))?;
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "synchronous", "NORMAL").ok();
        // Apply v2 migrations. This evolves a legacy v0.1 database to
        // the v2 schema in-place (runs gets zone_slug etc; decisions
        // gets renamed to verdict_history) and creates fresh tables on
        // a clean install.
        let applied = crate::persistence::run_migrations(&mut conn)
            .with_context(|| "applying schema migrations")?;
        if !applied.is_empty() {
            tracing::info!(applied = ?applied, "applied schema migrations");
        }
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn handle(&self) -> Arc<Mutex<Connection>> {
        self.conn.clone()
    }
}

// Schema lives in src/persistence/migrations/ now; HistoryDb just opens
// the connection and the migration runner handles tables.

/// Insert a completed run, ignoring duplicates by (zone_slug,
/// start_epoch, controller_id). The HA-refresher source always tags
/// these as `ha_refresher` / `ha_service_call` since they reflect HA's
/// own status sensors, not a LocalSky-initiated dispatch.
pub async fn record_run(conn: Arc<Mutex<Connection>>, rec: RunRecord) -> Result<()> {
    tokio::task::spawn_blocking(move || -> Result<()> {
        let conn = conn.blocking_lock();
        let end_epoch = rec.start_epoch + rec.duration_s;
        conn.execute(
            "INSERT OR IGNORE INTO runs
                (zone_slug, start_epoch, end_epoch, duration_s,
                 source, controller_id, status, skip_reason)
             VALUES (?1, ?2, ?3, ?4,
                     'ha_refresher', 'ha_service_call',
                     CASE WHEN ?5 IS NULL THEN 'completed' ELSE 'aborted' END,
                     ?5)",
            params![
                rec.zone,
                rec.start_epoch,
                end_epoch,
                rec.duration_s,
                rec.skip_reason,
            ],
        )?;
        Ok(())
    })
    .await
    .context("spawn_blocking join failed")?
}

/// Insert a verdict-change row into verdict_history. INSERT OR IGNORE
/// on the UNIQUE(epoch) index means same-second double-writes from a
/// busy refresher loop are silently absorbed. inputs_json is left as
/// `{}` here; the v2 engine writes the full blob through its own path.
pub async fn record_decision(
    conn: Arc<Mutex<Connection>>,
    rec: DecisionRecord,
    trace_json: String,
) -> Result<()> {
    tokio::task::spawn_blocking(move || -> Result<()> {
        let conn = conn.blocking_lock();
        conn.execute(
            "INSERT OR IGNORE INTO verdict_history
                (epoch, date_local, verdict, reason, inputs_json, trace_json)
             VALUES (?1, strftime('%Y-%m-%d', ?1, 'unixepoch'), ?2, ?3, '{}', ?4)",
            params![rec.epoch, rec.verdict, rec.reason, trace_json],
        )?;
        Ok(())
    })
    .await
    .context("spawn_blocking join failed")?
}

/// Fetch every verdict transition in [from_epoch, to_epoch). Ordered
/// ascending so the dashboard can stream-draw a timeline.
pub async fn decisions_window(
    conn: Arc<Mutex<Connection>>,
    from_epoch: i64,
    to_epoch: i64,
) -> Result<DecisionWindow> {
    tokio::task::spawn_blocking(move || -> Result<DecisionWindow> {
        let conn = conn.blocking_lock();
        let mut stmt = conn.prepare(
            "SELECT epoch, verdict, reason, trace_json
             FROM verdict_history
             WHERE epoch >= ?1 AND epoch < ?2
             ORDER BY epoch ASC",
        )?;
        let decisions: Vec<DecisionRecord> = stmt
            .query_map(params![from_epoch, to_epoch], |row| {
                let trace_json: String = row.get(3)?;
                let trace = if trace_json.is_empty() {
                    None
                } else {
                    serde_json::from_str(&trace_json).ok()
                };
                Ok(DecisionRecord {
                    epoch: row.get(0)?,
                    verdict: row.get(1)?,
                    reason: row.get(2)?,
                    trace,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(DecisionWindow {
            from_epoch,
            to_epoch,
            decisions,
        })
    })
    .await
    .context("spawn_blocking join failed")?
}

/// Fetch every run in the [from_epoch, to_epoch) window, ordered by
/// start_epoch ascending so the Gantt renderer can stream-draw left-
/// to-right.
pub async fn window(
    conn: Arc<Mutex<Connection>>,
    from_epoch: i64,
    to_epoch: i64,
) -> Result<HistoryWindow> {
    tokio::task::spawn_blocking(move || -> Result<HistoryWindow> {
        let conn = conn.blocking_lock();
        // COALESCE on duration_s because the v2 schema allows NULL for
        // rows in the 'running' or 'intended' transient states; the
        // legacy window view treats those as zero-length until they
        // complete and the row is updated.
        let mut stmt = conn.prepare(
            "SELECT zone_slug, start_epoch, COALESCE(duration_s, 0), skip_reason
             FROM runs
             WHERE start_epoch >= ?1 AND start_epoch < ?2
             ORDER BY start_epoch ASC",
        )?;
        let runs: Vec<RunRecord> = stmt
            .query_map(params![from_epoch, to_epoch], |row| {
                Ok(RunRecord {
                    zone: row.get(0)?,
                    start_epoch: row.get(1)?,
                    duration_s: row.get(2)?,
                    skip_reason: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(HistoryWindow {
            from_epoch,
            to_epoch,
            runs,
        })
    })
    .await
    .context("spawn_blocking join failed")?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ha::snapshot::{DecisionTrace, RuleEval};

    fn mem() -> Arc<Mutex<Connection>> {
        let mut c = Connection::open_in_memory().unwrap();
        crate::persistence::run_migrations(&mut c).unwrap();
        Arc::new(Mutex::new(c))
    }

    #[tokio::test]
    async fn decision_trace_roundtrips() {
        let db = mem();
        let trace = DecisionTrace {
            verdict: "skip".into(),
            reason: "Already wet (0.10\" today)".into(),
            degraded: false,
            rules: vec![RuleEval {
                id: "already_wet".into(),
                label: "Already wet today".into(),
                category: "weather".into(),
                detail: "0.10\" today vs 0.05\" floor".into(),
                outcome: "fired".into(),
                verdict: Some("skip".into()),
            }],
        };
        let rec = DecisionRecord {
            epoch: 1_700_000_000,
            verdict: "skip".into(),
            reason: "Already wet (0.10\" today)".into(),
            trace: None,
        };
        record_decision(db.clone(), rec, serde_json::to_string(&trace).unwrap())
            .await
            .unwrap();

        let w = decisions_window(db.clone(), 1_600_000_000, 1_800_000_000)
            .await
            .unwrap();
        assert_eq!(w.decisions.len(), 1);
        assert_eq!(w.decisions[0].verdict, "skip");
        assert_eq!(w.decisions[0].trace.clone().expect("trace present"), trace);
    }

    #[tokio::test]
    async fn legacy_empty_trace_reads_as_none() {
        let db = mem();
        let rec = DecisionRecord {
            epoch: 1_700_000_500,
            verdict: "run".into(),
            reason: String::new(),
            trace: None,
        };
        record_decision(db.clone(), rec, String::new())
            .await
            .unwrap();
        let w = decisions_window(db.clone(), 1_600_000_000, 1_800_000_000)
            .await
            .unwrap();
        assert!(w.decisions[0].trace.is_none());
    }
}
