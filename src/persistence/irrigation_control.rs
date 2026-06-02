// Standalone control-surface persistence. Holds the vacation pause +
// one-day override that, in HA mode, live in HA helpers. The native
// (no-HA) snapshot builder reads this each refresh so a standalone deploy
// can be paused; the POST /action handler writes it.
//
// Single row (id = 1, enforced by the M0008 CHECK). Reads default to "no
// pause / auto override" when the row or DB is unavailable, so a read
// failure can never accidentally *create* a pause or override.

use std::sync::Arc;

use rusqlite::{params, Connection};
use thiserror::Error;
use tokio::sync::Mutex;

#[derive(Debug, Error)]
pub enum IrrigationControlError {
    #[error("sqlite: {0}")]
    Sqlite(String),
}

/// The native control surface: vacation pause + one-day override. Mirrors
/// the two HA helpers (`input_datetime.irrigation_pause_until` +
/// `input_select.irrigation_override_tomorrow`) so `build_from_map` can
/// consume either source identically.
#[derive(Debug, Clone)]
pub struct IrrigationControlState {
    /// UTC epoch the vacation pause runs until; 0 = no pause.
    pub pause_until_epoch: i64,
    /// One-day override for tomorrow's verdict: "none" | "skip" | "run".
    pub override_tomorrow: String,
}

impl Default for IrrigationControlState {
    fn default() -> Self {
        Self {
            pause_until_epoch: 0,
            override_tomorrow: "none".to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct IrrigationControlStore {
    conn: Arc<Mutex<Connection>>,
}

impl IrrigationControlStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// Load the control surface. Returns the safe default (no pause, auto
    /// override) if the singleton row is missing or the query errors, so a
    /// transient DB hiccup never fabricates a pause.
    pub async fn get(&self) -> IrrigationControlState {
        let c = self.conn.clone();
        let res = tokio::task::spawn_blocking(move || -> rusqlite::Result<(i64, String)> {
            let conn = c.blocking_lock();
            conn.query_row(
                "SELECT pause_until_epoch, override_tomorrow
                 FROM irrigation_control WHERE id = 1",
                [],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
            )
        })
        .await;
        match res {
            Ok(Ok((pause_until_epoch, override_tomorrow))) => IrrigationControlState {
                pause_until_epoch,
                override_tomorrow,
            },
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "irrigation_control read failed; using safe default");
                IrrigationControlState::default()
            }
            Err(e) => {
                tracing::warn!(error = %e, "irrigation_control read join failed; using safe default");
                IrrigationControlState::default()
            }
        }
    }

    /// Set the vacation-pause expiry (UTC epoch). 0 clears the pause.
    pub async fn set_pause_until(&self, epoch: i64) -> Result<(), IrrigationControlError> {
        let c = self.conn.clone();
        let epoch = epoch.max(0);
        tokio::task::spawn_blocking(move || -> rusqlite::Result<()> {
            let conn = c.blocking_lock();
            conn.execute(
                "INSERT INTO irrigation_control (id, pause_until_epoch, updated_at_epoch)
                 VALUES (1, ?1, strftime('%s','now'))
                 ON CONFLICT(id) DO UPDATE SET
                    pause_until_epoch = excluded.pause_until_epoch,
                    updated_at_epoch  = excluded.updated_at_epoch",
                params![epoch],
            )?;
            Ok(())
        })
        .await
        .map_err(|e| IrrigationControlError::Sqlite(format!("join: {e}")))?
        .map_err(|e| IrrigationControlError::Sqlite(e.to_string()))
    }

    /// Set the one-day override for tomorrow. Caller validates the mode is
    /// one of none/skip/run before calling.
    pub async fn set_override_tomorrow(&self, mode: String) -> Result<(), IrrigationControlError> {
        let c = self.conn.clone();
        tokio::task::spawn_blocking(move || -> rusqlite::Result<()> {
            let conn = c.blocking_lock();
            conn.execute(
                "INSERT INTO irrigation_control (id, override_tomorrow, updated_at_epoch)
                 VALUES (1, ?1, strftime('%s','now'))
                 ON CONFLICT(id) DO UPDATE SET
                    override_tomorrow = excluded.override_tomorrow,
                    updated_at_epoch  = excluded.updated_at_epoch",
                params![mode],
            )?;
            Ok(())
        })
        .await
        .map_err(|e| IrrigationControlError::Sqlite(format!("join: {e}")))?
        .map_err(|e| IrrigationControlError::Sqlite(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn store() -> IrrigationControlStore {
        let mut conn = Connection::open_in_memory().unwrap();
        crate::persistence::run_migrations(&mut conn).unwrap();
        IrrigationControlStore::new(Arc::new(Mutex::new(conn)))
    }

    #[tokio::test]
    async fn defaults_to_no_pause_auto_override() {
        let s = store().await;
        let st = s.get().await;
        assert_eq!(st.pause_until_epoch, 0);
        assert_eq!(st.override_tomorrow, "none");
    }

    #[tokio::test]
    async fn set_pause_persists_without_clobbering_override() {
        let s = store().await;
        s.set_override_tomorrow("skip".to_string()).await.unwrap();
        s.set_pause_until(1_900_000_000).await.unwrap();
        let st = s.get().await;
        assert_eq!(st.pause_until_epoch, 1_900_000_000);
        assert_eq!(
            st.override_tomorrow, "skip",
            "pause set must not reset override"
        );
    }

    #[tokio::test]
    async fn clear_pause_with_zero() {
        let s = store().await;
        s.set_pause_until(1_900_000_000).await.unwrap();
        s.set_pause_until(0).await.unwrap();
        assert_eq!(s.get().await.pause_until_epoch, 0);
    }

    #[tokio::test]
    async fn negative_pause_clamps_to_zero() {
        let s = store().await;
        s.set_pause_until(-5).await.unwrap();
        assert_eq!(s.get().await.pause_until_epoch, 0);
    }
}
