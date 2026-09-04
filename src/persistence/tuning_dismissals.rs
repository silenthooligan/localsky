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

/// Reserved `zone_slug` for install-scoped notices (not about any one
/// zone). Zone slugs come from slugified zone names and the tuning
/// report only ever writes real slugs here, but the collision guard is
/// the FIELD, not this name: `silences` matches on (zone, field) both,
/// and no tuning recommendation carries a field named like a notice, so
/// an install-scoped row can never strip a real recommendation even if
/// a yard somehow names a zone "_install".
pub const INSTALL_SCOPE: &str = "_install";

/// The soil-model opt-in offer's field under `INSTALL_SCOPE`. One row
/// governs the whole install: the offer is about the engine default,
/// not any zone.
pub const SOIL_INVITE_FIELD: &str = "soil_invite";

/// Where the soil-model opt-in offer stands for this install. Server
/// side on purpose: the offer promises "dismiss and it will not
/// return", which a browser-local record cannot keep across devices,
/// browsers, or a cleared profile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InviteState {
    /// No live record: the offer may show (eligibility permitting).
    Open,
    /// Snoozed until the carried epoch; reads as `Open` from then on.
    Snoozed { until_epoch: i64 },
    /// Dismissed for good. Never expires.
    Dismissed,
}

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

    /// Where the soil-model opt-in offer stands at `now`. Reads the one
    /// (`INSTALL_SCOPE`, `SOIL_INVITE_FIELD`) row this store's replace-
    /// on-redismiss write discipline guarantees: `permanent` is
    /// dismissed forever, a live `snooze` is snoozed, an expired or
    /// missing one is open. Writes ride the existing `dismiss()` with
    /// the same key, so a re-snooze extends and a dismissal over a
    /// snooze replaces it, exactly the M0016 semantics.
    pub async fn soil_invite_state(
        &self,
        now_epoch: i64,
    ) -> Result<InviteState, TuningDismissalsError> {
        let c = self.conn.clone();
        tokio::task::spawn_blocking(move || -> rusqlite::Result<InviteState> {
            let conn = c.blocking_lock();
            let mut stmt = conn.prepare(
                "SELECT kind, until_epoch FROM tuning_dismissals
                 WHERE zone_slug = ?1 AND field = ?2",
            )?;
            let rows = stmt
                .query_map(params![INSTALL_SCOPE, SOIL_INVITE_FIELD], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, Option<i64>>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for (kind, until) in rows {
                match kind.as_str() {
                    "permanent" => return Ok(InviteState::Dismissed),
                    "snooze" => {
                        if let Some(u) = until {
                            if u > now_epoch {
                                return Ok(InviteState::Snoozed { until_epoch: u });
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(InviteState::Open)
        })
        .await
        .map_err(|e| TuningDismissalsError::Sqlite(format!("join: {e}")))?
        .map_err(|e| TuningDismissalsError::Sqlite(e.to_string()))
    }

    /// Record the answer to the soil-model offer and report where the
    /// offer stands afterward, read back from the row rather than
    /// assumed. Permanent is final: a snooze arriving after one (a
    /// second tab still showing the offer, a post retried from a queue)
    /// leaves the dismissal alone instead of bringing the offer back in
    /// 30 days. Every other direction writes, so a re-snooze extends and
    /// a dismissal over a snooze replaces it.
    pub async fn record_soil_invite_choice(
        &self,
        permanent: bool,
        now_epoch: i64,
    ) -> Result<InviteState, TuningDismissalsError> {
        if !permanent && self.soil_invite_state(now_epoch).await? == InviteState::Dismissed {
            return Ok(InviteState::Dismissed);
        }
        let (rec_id, kind) = if permanent {
            (None, "permanent")
        } else {
            (Some(SOIL_INVITE_FIELD), "snooze")
        };
        self.dismiss(INSTALL_SCOPE, SOIL_INVITE_FIELD, rec_id, kind, now_epoch)
            .await?;
        self.soil_invite_state(now_epoch).await
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

    /// The snooze boundary the offer promises: quiet for 30 days, back
    /// on the day they end. One second before expiry it still reads
    /// snoozed; at the expiry epoch itself it reads open (`silences`
    /// requires `until > now`, and this reader matches it).
    #[tokio::test]
    async fn soil_invite_snooze_reoffers_at_the_30_day_boundary() {
        let s = fresh_store().await;
        let now = 1_700_000_000;
        assert_eq!(s.soil_invite_state(now).await.unwrap(), InviteState::Open);
        s.dismiss(
            INSTALL_SCOPE,
            SOIL_INVITE_FIELD,
            Some(SOIL_INVITE_FIELD),
            "snooze",
            now,
        )
        .await
        .unwrap();
        let until = now + SNOOZE_DAYS * 86_400;
        assert_eq!(
            s.soil_invite_state(until - 1).await.unwrap(),
            InviteState::Snoozed { until_epoch: until }
        );
        assert_eq!(s.soil_invite_state(until).await.unwrap(), InviteState::Open);
        // Dismissing after a snooze replaces it: the offer never returns.
        s.dismiss(INSTALL_SCOPE, SOIL_INVITE_FIELD, None, "permanent", now)
            .await
            .unwrap();
        assert_eq!(
            s.soil_invite_state(until + 86_400).await.unwrap(),
            InviteState::Dismissed
        );
    }

    /// "This offer will not return" has to survive a stale tab. A second
    /// browser still showing the offer posts Snooze after the dismissal
    /// landed; the write is skipped, so the offer stays gone instead of
    /// coming back 30 days later. The opposite direction still lands.
    #[tokio::test]
    async fn a_snooze_after_a_dismissal_leaves_the_offer_dismissed() {
        let s = fresh_store().await;
        let now = 1_700_000_000;
        assert_eq!(
            s.record_soil_invite_choice(true, now).await.unwrap(),
            InviteState::Dismissed
        );
        // The stale tab's snooze: accepted by the API, written nowhere.
        assert_eq!(
            s.record_soil_invite_choice(false, now).await.unwrap(),
            InviteState::Dismissed
        );
        assert_eq!(
            s.soil_invite_state(now + SNOOZE_DAYS * 86_400 + 86_400)
                .await
                .unwrap(),
            InviteState::Dismissed
        );
    }

    /// The directions that DO write: a first snooze records one, a
    /// re-snooze extends it, and a dismissal over a live snooze replaces
    /// it for good.
    #[tokio::test]
    async fn a_snooze_extends_and_a_dismissal_replaces_it() {
        let s = fresh_store().await;
        let now = 1_700_000_000;
        assert_eq!(
            s.record_soil_invite_choice(false, now).await.unwrap(),
            InviteState::Snoozed {
                until_epoch: now + SNOOZE_DAYS * 86_400
            }
        );
        let later = now + 10 * 86_400;
        assert_eq!(
            s.record_soil_invite_choice(false, later).await.unwrap(),
            InviteState::Snoozed {
                until_epoch: later + SNOOZE_DAYS * 86_400
            }
        );
        assert_eq!(
            s.record_soil_invite_choice(true, later).await.unwrap(),
            InviteState::Dismissed
        );
    }

    /// The reason this record is server side at all: a dismissal must
    /// hold across a restart. File-backed database, store dropped,
    /// fresh connection, still dismissed.
    #[tokio::test]
    async fn soil_invite_dismissal_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!(
            "localsky-soil-invite-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("history.db");
        let now = 1_700_000_000;
        {
            let mut c = Connection::open(&db).unwrap();
            runner::run(&mut c).unwrap();
            let s = TuningDismissalsStore::new(Arc::new(Mutex::new(c)));
            s.dismiss(INSTALL_SCOPE, SOIL_INVITE_FIELD, None, "permanent", now)
                .await
                .unwrap();
        }
        // The restart: nothing survives but the file.
        let mut c = Connection::open(&db).unwrap();
        runner::run(&mut c).unwrap();
        let s = TuningDismissalsStore::new(Arc::new(Mutex::new(c)));
        assert_eq!(
            s.soil_invite_state(now + 400 * 86_400).await.unwrap(),
            InviteState::Dismissed
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The install-scoped row must never strip a real tuning
    /// recommendation: `silences` keys (zone, field) both, and no
    /// recommendation field is named like the offer.
    #[tokio::test]
    async fn soil_invite_row_silences_no_zone_recommendation() {
        let s = fresh_store().await;
        let now = 1_700_000_000;
        s.dismiss(INSTALL_SCOPE, SOIL_INVITE_FIELD, None, "permanent", now)
            .await
            .unwrap();
        let rows = s.active(now).await.unwrap();
        assert_eq!(rows.len(), 1);
        for field in ["weekly_budget_in", "sessions_per_week", "max_run_minutes"] {
            assert!(!rows[0].silences("_install", field, "any-id", now));
            assert!(!rows[0].silences("back_yard", field, "any-id", now));
        }
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
