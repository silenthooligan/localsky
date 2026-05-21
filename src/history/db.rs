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
    /// Open or create the SQLite file at the given path. Schema is
    /// applied with CREATE IF NOT EXISTS so this is idempotent.
    pub fn open(path: PathBuf) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let conn = Connection::open(&path)
            .with_context(|| format!("open sqlite at {path:?}"))?;
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "synchronous", "NORMAL").ok();
        conn.execute_batch(SCHEMA)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    pub fn handle(&self) -> Arc<Mutex<Connection>> {
        self.conn.clone()
    }
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS runs (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    zone          TEXT    NOT NULL,
    start_epoch   INTEGER NOT NULL,
    duration_s    INTEGER NOT NULL,
    skip_reason   TEXT,
    UNIQUE(zone, start_epoch)
);
CREATE INDEX IF NOT EXISTS idx_runs_zone_start ON runs(zone, start_epoch DESC);
CREATE INDEX IF NOT EXISTS idx_runs_start ON runs(start_epoch DESC);

-- Web Push subscriptions (Phase 5). One row per browser/device + origin
-- combination. endpoint is unique because the browser hands us a stable
-- URL per subscription; re-subscribing on the same device re-uses the row
-- (INSERT OR REPLACE) so we don't accumulate dead duplicates.
CREATE TABLE IF NOT EXISTS push_subscriptions (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    endpoint    TEXT    NOT NULL UNIQUE,
    p256dh      TEXT    NOT NULL,
    auth        TEXT    NOT NULL,
    created_at  INTEGER NOT NULL,
    last_seen   INTEGER NOT NULL
);

-- Skip-check verdict transitions. One row per (verdict, reason) change so
-- "did we actually skip on day X" is answerable in 30 days. UNIQUE(epoch)
-- prevents a tight loop from ever producing duplicate rows.
CREATE TABLE IF NOT EXISTS decisions (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    epoch       INTEGER NOT NULL,
    verdict     TEXT    NOT NULL,
    reason      TEXT    NOT NULL,
    UNIQUE(epoch)
);
CREATE INDEX IF NOT EXISTS idx_decisions_epoch ON decisions(epoch DESC);
"#;

/// Insert a completed run, ignoring duplicates by (zone, start_epoch).
/// Called from spawn_blocking — pass the Arc<Mutex<Connection>>.
pub async fn record_run(
    conn: Arc<Mutex<Connection>>,
    rec: RunRecord,
) -> Result<()> {
    tokio::task::spawn_blocking(move || -> Result<()> {
        let conn = conn.blocking_lock();
        conn.execute(
            "INSERT OR IGNORE INTO runs (zone, start_epoch, duration_s, skip_reason)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                rec.zone,
                rec.start_epoch,
                rec.duration_s,
                rec.skip_reason
            ],
        )?;
        Ok(())
    })
    .await
    .context("spawn_blocking join failed")?
}

/// Insert a verdict-change row. INSERT OR IGNORE on the (epoch) unique
/// index means same-second double-writes from a busy refresher loop are
/// silently absorbed.
pub async fn record_decision(
    conn: Arc<Mutex<Connection>>,
    rec: DecisionRecord,
) -> Result<()> {
    tokio::task::spawn_blocking(move || -> Result<()> {
        let conn = conn.blocking_lock();
        conn.execute(
            "INSERT OR IGNORE INTO decisions (epoch, verdict, reason)
             VALUES (?1, ?2, ?3)",
            params![rec.epoch, rec.verdict, rec.reason],
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
            "SELECT epoch, verdict, reason
             FROM decisions
             WHERE epoch >= ?1 AND epoch < ?2
             ORDER BY epoch ASC",
        )?;
        let decisions: Vec<DecisionRecord> = stmt
            .query_map(params![from_epoch, to_epoch], |row| {
                Ok(DecisionRecord {
                    epoch: row.get(0)?,
                    verdict: row.get(1)?,
                    reason: row.get(2)?,
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
        let mut stmt = conn.prepare(
            "SELECT zone, start_epoch, duration_s, skip_reason
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
