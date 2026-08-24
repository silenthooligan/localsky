// Tuning-report notification state. Holds the last time the weekly
// "tuning report ready" push was emitted, so the scheduler's 7-day
// dedupe survives the restart-heavy GitOps deploy cadence (the M0008
// header documents the same reasoning for the pause state).
//
// Single row (id = 1, enforced by the M0014 CHECK). Reads default to 0
// (never notified) on any error: a DB hiccup can at worst delay a
// notification, never duplicate one, because the scheduler persists the
// stamp BEFORE it emits.

use std::sync::Arc;

use rusqlite::{params, Connection};
use thiserror::Error;
use tokio::sync::Mutex;

#[derive(Debug, Error)]
pub enum TuningReportStateError {
    #[error("sqlite: {0}")]
    Sqlite(String),
}

#[derive(Debug, Clone)]
pub struct TuningReportStateStore {
    conn: Arc<Mutex<Connection>>,
}

impl TuningReportStateStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// UTC epoch of the last "report ready" notification; 0 = never.
    /// Errors read as 0 so a transient failure delays rather than spams.
    pub async fn last_notified_epoch(&self) -> i64 {
        let c = self.conn.clone();
        let res = tokio::task::spawn_blocking(move || -> rusqlite::Result<i64> {
            let conn = c.blocking_lock();
            conn.query_row(
                "SELECT last_notified_epoch FROM tuning_report_state WHERE id = 1",
                [],
                |r| r.get(0),
            )
        })
        .await;
        match res {
            Ok(Ok(epoch)) => epoch,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "tuning_report_state read failed; treating as never notified");
                0
            }
            Err(e) => {
                tracing::warn!(error = %e, "tuning_report_state read join failed; treating as never notified");
                0
            }
        }
    }

    /// Stamp the last-notified epoch. Called BEFORE the (lossy) push emit
    /// so a crash between the two loses at most one notification, never
    /// duplicates one.
    pub async fn set_last_notified_epoch(&self, epoch: i64) -> Result<(), TuningReportStateError> {
        let c = self.conn.clone();
        let epoch = epoch.max(0);
        tokio::task::spawn_blocking(move || -> rusqlite::Result<()> {
            let conn = c.blocking_lock();
            conn.execute(
                "INSERT INTO tuning_report_state (id, last_notified_epoch, updated_at_epoch)
                 VALUES (1, ?1, strftime('%s','now'))
                 ON CONFLICT(id) DO UPDATE SET
                    last_notified_epoch = excluded.last_notified_epoch,
                    updated_at_epoch = excluded.updated_at_epoch",
                params![epoch],
            )?;
            Ok(())
        })
        .await
        .map_err(|e| TuningReportStateError::Sqlite(format!("join: {e}")))?
        .map_err(|e| TuningReportStateError::Sqlite(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::runner;

    async fn fresh_store() -> TuningReportStateStore {
        let mut c = Connection::open_in_memory().unwrap();
        runner::run(&mut c).unwrap();
        TuningReportStateStore::new(Arc::new(Mutex::new(c)))
    }

    #[tokio::test]
    async fn defaults_to_never_notified_and_roundtrips() {
        let s = fresh_store().await;
        assert_eq!(s.last_notified_epoch().await, 0);
        s.set_last_notified_epoch(1_750_000_000).await.unwrap();
        assert_eq!(s.last_notified_epoch().await, 1_750_000_000);
        // Setters UPSERT: a second stamp replaces the first.
        s.set_last_notified_epoch(1_750_600_000).await.unwrap();
        assert_eq!(s.last_notified_epoch().await, 1_750_600_000);
    }

    #[tokio::test]
    async fn missing_table_reads_as_zero() {
        // A connection with NO migrations applied: get() must not error.
        let c = Connection::open_in_memory().unwrap();
        let s = TuningReportStateStore::new(Arc::new(Mutex::new(c)));
        assert_eq!(s.last_notified_epoch().await, 0);
    }
}
