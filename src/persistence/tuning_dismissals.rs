// Tuning-recommendation dismissals (M0016). Two kinds with distinct
// keys:
//   'snooze':    keyed by the exact recommendation id; expires at
//                until_epoch (30 days). The same recommendation may
//                return after expiry; a re-derived one with a different
//                id returns immediately.
//   'permanent': keyed by (zone_slug, field); never expires, so a
//                recommendation whose suggested value drifts stays
//                dismissed.
//
// The report generator loads the active set once per generation and
// strips matching recommendations server-side, which silences every
// consumer at once (cards, counts, auto-select, the weekly push).

use std::sync::Arc;

use rusqlite::{params, Connection};
use thiserror::Error;
use tokio::sync::Mutex;

/// Snooze duration: the recommendation stays quiet this long, then may
/// return if it still derives.
pub const SNOOZE_DAYS: i64 = 30;

#[derive(Debug, Error)]
pub enum TuningDismissalsError {
    #[error("sqlite: {0}")]
    Sqlite(String),
}

/// One stored dismissal row.
#[derive(Debug, Clone, PartialEq)]
pub struct DismissalRow {
    pub zone_slug: String,
    pub field: String,
    pub rec_id: Option<String>,
    pub kind: String,
    pub until_epoch: Option<i64>,
}

impl DismissalRow {
    /// Whether this row silences `rec_id` for (zone, field) at `now`.
    /// Permanent rows match on (zone, field) regardless of the value;
    /// snoozes match the exact recommendation id until they expire.
    pub fn silences(&self, zone_slug: &str, field: &str, rec_id: &str, now_epoch: i64) -> bool {
        if self.zone_slug != zone_slug || self.field != field {
            return false;
        }
        match self.kind.as_str() {
            "permanent" => true,
            "snooze" => {
                self.rec_id.as_deref() == Some(rec_id)
                    && self.until_epoch.map(|u| u > now_epoch).unwrap_or(false)
            }
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TuningDismissalsStore {
    conn: Arc<Mutex<Connection>>,
}

impl TuningDismissalsStore {
    pub fn new(conn: Arc<Mutex<Connection>>) -> Self {
        Self { conn }
    }

    /// Record a dismissal. A permanent dismissal replaces any prior rows
    /// for (zone, field); a snooze replaces prior snoozes for the same
    /// (zone, field) so re-snoozing extends rather than stacks.
    pub async fn dismiss(
        &self,
        zone_slug: &str,
        field: &str,
        rec_id: Option<&str>,
        kind: &str,
        now_epoch: i64,
    ) -> Result<(), TuningDismissalsError> {
        let c = self.conn.clone();
        let zone = zone_slug.to_string();
        let field = field.to_string();
        let rec = rec_id.map(|s| s.to_string());
        let kind = kind.to_string();
        let until = if kind == "snooze" {
            Some(now_epoch + SNOOZE_DAYS * 86_400)
        } else {
            None
        };
        tokio::task::spawn_blocking(move || -> rusqlite::Result<()> {
            let conn = c.blocking_lock();
            conn.execute(
                "DELETE FROM tuning_dismissals WHERE zone_slug = ?1 AND field = ?2",
                params![zone, field],
            )?;
            conn.execute(
                "INSERT INTO tuning_dismissals
                    (zone_slug, field, rec_id, kind, until_epoch, created_epoch)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![zone, field, rec, kind, until, now_epoch],
            )?;
            Ok(())
        })
        .await
        .map_err(|e| TuningDismissalsError::Sqlite(format!("join: {e}")))?
        .map_err(|e| TuningDismissalsError::Sqlite(e.to_string()))
    }

    /// Remove every dismissal for (zone, field). Returns how many rows
    /// were removed (0 = nothing was silenced).
    pub async fn undismiss(
        &self,
        zone_slug: &str,
        field: &str,
    ) -> Result<usize, TuningDismissalsError> {
        let c = self.conn.clone();
        let zone = zone_slug.to_string();
        let field = field.to_string();
        tokio::task::spawn_blocking(move || -> rusqlite::Result<usize> {
            let conn = c.blocking_lock();
            conn.execute(
                "DELETE FROM tuning_dismissals WHERE zone_slug = ?1 AND field = ?2",
                params![zone, field],
            )
        })
        .await
        .map_err(|e| TuningDismissalsError::Sqlite(format!("join: {e}")))?
        .map_err(|e| TuningDismissalsError::Sqlite(e.to_string()))
    }

    /// Every ACTIVE dismissal (expired snoozes are pruned in passing).
    pub async fn active(&self, now_epoch: i64) -> Result<Vec<DismissalRow>, TuningDismissalsError> {
        let c = self.conn.clone();
        tokio::task::spawn_blocking(move || -> rusqlite::Result<Vec<DismissalRow>> {
            let conn = c.blocking_lock();
            // Opportunistic prune keeps the table from accreting expired
            // snoozes; harmless if it races another reader.
            conn.execute(
                "DELETE FROM tuning_dismissals
                 WHERE kind = 'snooze' AND until_epoch IS NOT NULL AND until_epoch <= ?1",
                params![now_epoch],
            )?;
            let mut stmt = conn.prepare(
                "SELECT zone_slug, field, rec_id, kind, until_epoch
                 FROM tuning_dismissals
                 ORDER BY zone_slug, field",
            )?;
            let rows = stmt
                .query_map([], |r| {
                    Ok(DismissalRow {
                        zone_slug: r.get(0)?,
                        field: r.get(1)?,
                        rec_id: r.get(2)?,
                        kind: r.get(3)?,
                        until_epoch: r.get(4)?,
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows)
        })
        .await
        .map_err(|e| TuningDismissalsError::Sqlite(format!("join: {e}")))?
        .map_err(|e| TuningDismissalsError::Sqlite(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::runner;

    async fn fresh_store() -> TuningDismissalsStore {
        let mut c = Connection::open_in_memory().unwrap();
        runner::run(&mut c).unwrap();
        TuningDismissalsStore::new(Arc::new(Mutex::new(c)))
    }

    /// A snooze silences the exact recommendation id and expires after
    /// 30 days; a different id (the value drifted) is NOT silenced.
    #[tokio::test]
    async fn snooze_keys_the_exact_id_and_expires() {
        let s = fresh_store().await;
        let now = 1_700_000_000;
        s.dismiss(
            "back_yard",
            "max_run_minutes",
            Some("abc123"),
            "snooze",
            now,
        )
        .await
        .unwrap();
        let rows = s.active(now).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].silences("back_yard", "max_run_minutes", "abc123", now + 60));
        assert!(
            !rows[0].silences("back_yard", "max_run_minutes", "other-id", now + 60),
            "a drifted recommendation id may return"
        );
        // Past expiry the snooze no longer silences, and active() prunes it.
        let after = now + (SNOOZE_DAYS + 1) * 86_400;
        assert!(!rows[0].silences("back_yard", "max_run_minutes", "abc123", after));
        let rows = s.active(after).await.unwrap();
        assert!(rows.is_empty(), "expired snoozes are pruned");
    }

    /// A permanent dismissal keys (zone, field) and survives value
    /// drift: any recommendation id for that field stays silenced.
    #[tokio::test]
    async fn permanent_survives_value_drift() {
        let s = fresh_store().await;
        let now = 1_700_000_000;
        s.dismiss(
            "back_yard",
            "max_run_minutes",
            Some("abc123"),
            "permanent",
            now,
        )
        .await
        .unwrap();
        let far_future = now + 400 * 86_400;
        let rows = s.active(far_future).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert!(rows[0].silences("back_yard", "max_run_minutes", "abc123", far_future));
        assert!(
            rows[0].silences("back_yard", "max_run_minutes", "drifted-id", far_future),
            "field-keyed: the drifted value stays dismissed"
        );
        assert!(!rows[0].silences("front_yard", "max_run_minutes", "abc123", far_future));
        assert!(!rows[0].silences("back_yard", "sessions_per_week", "abc123", far_future));
    }

    #[tokio::test]
    async fn undismiss_clears_and_reports_count() {
        let s = fresh_store().await;
        let now = 1_700_000_000;
        s.dismiss("back_yard", "max_run_minutes", None, "permanent", now)
            .await
            .unwrap();
        assert_eq!(
            s.undismiss("back_yard", "max_run_minutes").await.unwrap(),
            1
        );
        assert_eq!(
            s.undismiss("back_yard", "max_run_minutes").await.unwrap(),
            0
        );
        assert!(s.active(now).await.unwrap().is_empty());
    }

    /// Re-dismissing the same (zone, field) replaces rather than stacks.
    #[tokio::test]
    async fn redismiss_replaces_prior_rows() {
        let s = fresh_store().await;
        let now = 1_700_000_000;
        s.dismiss("z", "f", Some("a"), "snooze", now).await.unwrap();
        s.dismiss("z", "f", Some("b"), "snooze", now + 10)
            .await
            .unwrap();
        let rows = s.active(now + 20).await.unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].rec_id.as_deref(), Some("b"));
    }
}
