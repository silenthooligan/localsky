// Verdict history store. Replaces the legacy `decisions` table with
// `verdict_history` (M0005): adds date_local + inputs_json columns so
// any historical decision can be replayed through the current engine.

use std::sync::Arc;

use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::Mutex;

#[derive(Debug, Error)]
pub enum VerdictHistoryError {
    #[error("sqlite: {0}")]
    Sqlite(String),
    #[error("inputs serialize: {0}")]
    Serialize(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerdictRow {
    pub id: i64,
    pub epoch: i64,
    pub date_local: String,
    pub verdict: String,
    pub reason: String,
    pub inputs_json: String,
}

#[derive(Debug, Clone)]
pub struct NewVerdict {
    pub epoch: i64,
    pub date_local: String,
    pub verdict: String,
    pub reason: String,
    /// Optional raw Inputs blob for replay. Serialize the
    /// engine::skip_rules::Inputs struct to JSON. Pass an empty object
    /// string ("{}") if not capturing.
    pub inputs_json: String,
}

#[derive(Clone)]
pub struct VerdictHistoryStore {
    conn: Arc<Mutex<Connection>>,
}

impl VerdictHistoryStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    pub async fn insert(&self, v: NewVerdict) -> Result<(), VerdictHistoryError> {
        let c = self.conn.clone();
        tokio::task::spawn_blocking(move || -> rusqlite::Result<()> {
            let conn = c.blocking_lock();
            conn.execute(
                "INSERT OR IGNORE INTO verdict_history(epoch, date_local, verdict, reason, inputs_json)
                 VALUES (?, ?, ?, ?, ?)",
                params![v.epoch, v.date_local, v.verdict, v.reason, v.inputs_json],
            )?;
            Ok(())
        })
        .await
        .map_err(|e| VerdictHistoryError::Sqlite(format!("join: {e}")))?
        .map_err(|e| VerdictHistoryError::Sqlite(e.to_string()))
    }

    pub async fn window(
        &self,
        from_epoch: i64,
        to_epoch: i64,
    ) -> Result<Vec<VerdictRow>, VerdictHistoryError> {
        let c = self.conn.clone();
        tokio::task::spawn_blocking(move || -> rusqlite::Result<Vec<VerdictRow>> {
            let conn = c.blocking_lock();
            let mut stmt = conn.prepare(
                "SELECT id, epoch, date_local, verdict, reason, inputs_json
                 FROM verdict_history
                 WHERE epoch >= ? AND epoch < ?
                 ORDER BY epoch ASC",
            )?;
            let rows = stmt
                .query_map(params![from_epoch, to_epoch], |r| {
                    Ok(VerdictRow {
                        id: r.get(0)?,
                        epoch: r.get(1)?,
                        date_local: r.get(2)?,
                        verdict: r.get(3)?,
                        reason: r.get(4)?,
                        inputs_json: r.get(5)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
        .map_err(|e| VerdictHistoryError::Sqlite(format!("join: {e}")))?
        .map_err(|e| VerdictHistoryError::Sqlite(e.to_string()))
    }

    /// All verdicts for a specific local date. Useful for the daily
    /// dashboard verdict tile.
    pub async fn for_date(
        &self,
        date_local: String,
    ) -> Result<Vec<VerdictRow>, VerdictHistoryError> {
        let c = self.conn.clone();
        tokio::task::spawn_blocking(move || -> rusqlite::Result<Vec<VerdictRow>> {
            let conn = c.blocking_lock();
            let mut stmt = conn.prepare(
                "SELECT id, epoch, date_local, verdict, reason, inputs_json
                 FROM verdict_history WHERE date_local = ?
                 ORDER BY epoch ASC",
            )?;
            let rows = stmt
                .query_map(params![date_local], |r| {
                    Ok(VerdictRow {
                        id: r.get(0)?,
                        epoch: r.get(1)?,
                        date_local: r.get(2)?,
                        verdict: r.get(3)?,
                        reason: r.get(4)?,
                        inputs_json: r.get(5)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
        .map_err(|e| VerdictHistoryError::Sqlite(format!("join: {e}")))?
        .map_err(|e| VerdictHistoryError::Sqlite(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::runner;

    async fn fresh_store() -> VerdictHistoryStore {
        let mut c = Connection::open_in_memory().unwrap();
        runner::run(&mut c).unwrap();
        VerdictHistoryStore::new(Arc::new(Mutex::new(c)))
    }

    #[tokio::test]
    async fn legacy_decisions_carry_into_verdict_history() {
        // Simulate a legacy v0.1 DB with rows in `decisions`, then run
        // migrations. M0005 should copy rows forward.
        let mut c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE decisions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                epoch INTEGER NOT NULL,
                verdict TEXT NOT NULL,
                reason TEXT NOT NULL,
                UNIQUE(epoch)
            );
            INSERT INTO decisions(epoch, verdict, reason)
                VALUES (1700000000, 'skip', 'Already wet'), (1700086400, 'run', '');",
        )
        .unwrap();
        runner::run(&mut c).unwrap();

        let count: i64 = c
            .query_row("SELECT COUNT(*) FROM verdict_history", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);
        // legacy decisions table dropped
        let still_there: i64 = c
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='decisions'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(still_there, 0);
    }

    #[tokio::test]
    async fn insert_then_for_date_roundtrip() {
        let s = fresh_store().await;
        s.insert(NewVerdict {
            epoch: 1700000000,
            date_local: "2023-11-14".into(),
            verdict: "skip".into(),
            reason: "Rain expected".into(),
            inputs_json: "{}".into(),
        })
        .await
        .unwrap();
        let rows = s.for_date("2023-11-14".into()).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].verdict, "skip");
    }

    #[tokio::test]
    async fn window_queries_inclusive_from_exclusive_to() {
        let s = fresh_store().await;
        for i in 0..5 {
            s.insert(NewVerdict {
                epoch: 1000 + i,
                date_local: "x".into(),
                verdict: "run".into(),
                reason: "".into(),
                inputs_json: "{}".into(),
            })
            .await
            .unwrap();
        }
        let win = s.window(1001, 1004).await.unwrap();
        assert_eq!(win.len(), 3);
        assert_eq!(win.first().unwrap().epoch, 1001);
        assert_eq!(win.last().unwrap().epoch, 1003);
    }
}
