// Standalone control-surface persistence. Holds the vacation pause +
// one-day override that, in HA mode, live in HA helpers. The native
// (no-HA) snapshot builder reads this each refresh so a standalone deploy
// can be paused; the POST /action handler writes it.
//
// Single row (id = 1, enforced by the M0008 CHECK). Reads default to "no
// pause / auto override" when the row or DB is unavailable, so a read
// failure can never accidentally *create* a pause or override.

use std::collections::HashMap;
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
    /// Sticky global override (holds until set back to auto):
    /// "auto" | "skip" | "run". Beats the engine verdict; a per-zone
    /// override beats this. Distinct from the one-day override_tomorrow.
    pub global_override: String,
    /// Sticky per-zone overrides: zone slug -> "skip" | "run". A zone absent
    /// from the map is "auto". Loaded alongside the singleton row so the
    /// snapshot builder + engine get the whole control surface in one read.
    pub zone_overrides: HashMap<String, String>,
    /// Indefinite vacation pause (M0017). The native home of what used to be
    /// `input_boolean.irrigation_pause`: an on/off hard skip on every zone,
    /// distinct from `pause_until_epoch`, which expires by itself.
    pub is_paused: bool,
    /// Dry-run mode (M0017). The native home of what used to be
    /// `input_boolean.irrigation_dry_run`: the engine decides normally and
    /// then returns a skip with reason "Dry-run mode", so nothing dispatches.
    /// Not the dry_run CONTROLLER kind, which produces a run verdict and
    /// synthesizes run rows; this one waters nothing and records nothing.
    pub is_dry_run: bool,
}

impl Default for IrrigationControlState {
    fn default() -> Self {
        Self {
            pause_until_epoch: 0,
            override_tomorrow: "none".to_string(),
            global_override: "auto".to_string(),
            zone_overrides: HashMap::new(),
            is_paused: false,
            is_dry_run: false,
        }
    }
}

/// Resolve the stored one-day override against today's local date.
///
/// The one-day override paints tomorrow's verdict, so it is only meaningful
/// on the day it was set. In Home Assistant mode a midnight automation reset
/// the input_select each night; nothing native did. Retiring that read
/// without this would freeze an adopted "skip" on tomorrow's cell forever.
///
/// A stamp that does not match today reads as "none". An unstamped row reads
/// as "none" too: that is the M0017 default, carried by any override that
/// predates the migration, and a pre-M0017 override had no day of its own and
/// could not expire, so there is no day it can honestly claim.
fn effective_override_tomorrow(mode: &str, day: &str, today: &str) -> String {
    if mode == "none" || day != today {
        return "none".to_string();
    }
    mode.to_string()
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
    ///
    /// The one-day override is resolved through `effective_override_tomorrow`
    /// against the CONFIGURED deployment timezone's current date, so it
    /// expires at local midnight the way the Home Assistant midnight
    /// automation used to expire the input_select.
    ///
    /// This lenient read is right for the snapshot path, which rebuilds every
    /// ten seconds and self-corrects. It is WRONG for anything that concludes
    /// something irreversible from an absence: see `try_get`.
    pub async fn get(&self) -> IrrigationControlState {
        let today = crate::timeutil::now_local().date_naive().to_string();
        self.get_on(&today).await
    }

    /// `get`, with the local date supplied. Split out so the one-day
    /// override's expiry is testable without moving the clock.
    pub async fn get_on(&self, today: &str) -> IrrigationControlState {
        self.try_get_on(today).await.unwrap_or_else(|e| {
            tracing::warn!(error = %e, "irrigation_control read failed; using safe default");
            IrrigationControlState::default()
        })
    }

    /// The same read, reporting failure instead of defaulting.
    ///
    /// The one-time helper adoption pass plans irreversible decisions from
    /// this state: a control column holding a non-default value is the
    /// operator's own answer and outranks the Home Assistant helper, and an
    /// all-default state means the helper's value is adopted and its read is
    /// retired for good. A failed SELECT resolving to the default is
    /// indistinguishable from "nobody ever set anything", so a transient
    /// SQLite error would overwrite a live native pause with a legacy helper
    /// and retire the read on the way out. The pass defers on `Err` and
    /// re-earns its evidence instead.
    ///
    /// A genuinely MISSING singleton row is not an error: that is a database
    /// answering confidently that nothing was ever set, which is what a fresh
    /// install looks like.
    pub async fn try_get(&self) -> Result<IrrigationControlState, IrrigationControlError> {
        let today = crate::timeutil::now_local().date_naive().to_string();
        self.try_get_on(&today).await
    }

    /// `try_get`, with the local date supplied.
    pub async fn try_get_on(
        &self,
        today: &str,
    ) -> Result<IrrigationControlState, IrrigationControlError> {
        let c = self.conn.clone();
        type Row = (
            i64,
            String,
            String,
            String,
            bool,
            bool,
            HashMap<String, String>,
        );
        let res = tokio::task::spawn_blocking(move || -> rusqlite::Result<Row> {
            use rusqlite::OptionalExtension;
            let conn = c.blocking_lock();
            let row = conn
                .query_row(
                    "SELECT pause_until_epoch, override_tomorrow, override_tomorrow_day,
                            global_override, is_paused, is_dry_run
                     FROM irrigation_control WHERE id = 1",
                    [],
                    |r| {
                        Ok((
                            r.get::<_, i64>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, String>(2)?,
                            r.get::<_, String>(3)?,
                            r.get::<_, i64>(4)? != 0,
                            r.get::<_, i64>(5)? != 0,
                        ))
                    },
                )
                .optional()?;
            // No singleton row is a confident "nothing was ever set", not a
            // failure: it is what a database carries before the first write.
            let d = IrrigationControlState::default();
            let (pause, override_tomorrow, override_day, global_override, is_paused, is_dry_run) =
                row.unwrap_or((
                    d.pause_until_epoch,
                    d.override_tomorrow.clone(),
                    String::new(),
                    d.global_override.clone(),
                    d.is_paused,
                    d.is_dry_run,
                ));
            let mut zone_overrides = HashMap::new();
            let mut stmt = conn.prepare("SELECT zone_slug, override_mode FROM zone_overrides")?;
            let rows =
                stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
            for row in rows {
                let (slug, mode) = row?;
                zone_overrides.insert(slug, mode);
            }
            Ok((
                pause,
                override_tomorrow,
                override_day,
                global_override,
                is_paused,
                is_dry_run,
                zone_overrides,
            ))
        })
        .await;
        match res {
            Ok(Ok((
                pause_until_epoch,
                override_tomorrow,
                override_day,
                global_override,
                is_paused,
                is_dry_run,
                zone_overrides,
            ))) => Ok(IrrigationControlState {
                pause_until_epoch,
                override_tomorrow: effective_override_tomorrow(
                    &override_tomorrow,
                    &override_day,
                    today,
                ),
                global_override,
                zone_overrides,
                is_paused,
                is_dry_run,
            }),
            Ok(Err(e)) => Err(IrrigationControlError::Sqlite(e.to_string())),
            Err(e) => Err(IrrigationControlError::Sqlite(format!("join: {e}"))),
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
    /// one of none/skip/run before calling. Stamped with the current local
    /// date so it expires at midnight (see `effective_override_tomorrow`).
    pub async fn set_override_tomorrow(&self, mode: String) -> Result<(), IrrigationControlError> {
        let today = crate::timeutil::now_local().date_naive().to_string();
        self.set_override_tomorrow_on(mode, today).await
    }

    /// `set_override_tomorrow`, with the day stamp supplied. Split out for the
    /// adoption pass (which stamps the day it read the helper on) and for
    /// tests that need a fixed date.
    pub async fn set_override_tomorrow_on(
        &self,
        mode: String,
        day: String,
    ) -> Result<(), IrrigationControlError> {
        let c = self.conn.clone();
        tokio::task::spawn_blocking(move || -> rusqlite::Result<()> {
            let conn = c.blocking_lock();
            conn.execute(
                "INSERT INTO irrigation_control
                    (id, override_tomorrow, override_tomorrow_day, updated_at_epoch)
                 VALUES (1, ?1, ?2, strftime('%s','now'))
                 ON CONFLICT(id) DO UPDATE SET
                    override_tomorrow     = excluded.override_tomorrow,
                    override_tomorrow_day = excluded.override_tomorrow_day,
                    updated_at_epoch      = excluded.updated_at_epoch",
                params![mode, day],
            )?;
            Ok(())
        })
        .await
        .map_err(|e| IrrigationControlError::Sqlite(format!("join: {e}")))?
        .map_err(|e| IrrigationControlError::Sqlite(e.to_string()))
    }

    /// Set the indefinite vacation pause (M0017). The native home of
    /// `input_boolean.irrigation_pause`.
    pub async fn set_paused(&self, on: bool) -> Result<(), IrrigationControlError> {
        self.set_flag("is_paused", on).await
    }

    /// Set dry-run mode (M0017). The native home of
    /// `input_boolean.irrigation_dry_run`.
    pub async fn set_dry_run(&self, on: bool) -> Result<(), IrrigationControlError> {
        self.set_flag("is_dry_run", on).await
    }

    /// Shared UPSERT for the two boolean toggles. `column` is a literal from
    /// the two callers above and never reaches here from a request, so the
    /// format! cannot carry user input into SQL.
    async fn set_flag(&self, column: &'static str, on: bool) -> Result<(), IrrigationControlError> {
        let c = self.conn.clone();
        tokio::task::spawn_blocking(move || -> rusqlite::Result<()> {
            let conn = c.blocking_lock();
            let sql = format!(
                "INSERT INTO irrigation_control (id, {column}, updated_at_epoch)
                 VALUES (1, ?1, strftime('%s','now'))
                 ON CONFLICT(id) DO UPDATE SET
                    {column}         = excluded.{column},
                    updated_at_epoch = excluded.updated_at_epoch"
            );
            conn.execute(&sql, params![i64::from(on)])?;
            Ok(())
        })
        .await
        .map_err(|e| IrrigationControlError::Sqlite(format!("join: {e}")))?
        .map_err(|e| IrrigationControlError::Sqlite(e.to_string()))
    }

    /// Force everything written above onto stable storage.
    ///
    /// The connection runs `journal_mode=WAL` with `synchronous=NORMAL`,
    /// which does NOT fsync on commit: an UPSERT returns Ok while its pages
    /// sit in the OS page cache. The adoption pass's config marker, by
    /// contrast, goes through `write_atomic_durable` and is fsynced
    /// immediately, so without this the documented "SQLite first, config
    /// marker last" ordering is the exact reverse of what survives a power
    /// cut: the markers land, the pause does not, and the read is retired
    /// onto a column that never got the value. Called once, by the adoption
    /// pass, between the control writes and the config marker.
    pub async fn flush_durable(&self) -> Result<(), IrrigationControlError> {
        let c = self.conn.clone();
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            let conn = c.blocking_lock();
            let mode: String = conn
                .query_row("PRAGMA journal_mode", [], |r| r.get(0))
                .map_err(|e| e.to_string())?;
            if !mode.eq_ignore_ascii_case("wal") {
                // A rollback-journal or in-memory database has no WAL to
                // checkpoint, and its commits are already as durable as they
                // are going to get.
                return Ok(());
            }
            // FULL rather than TRUNCATE: TRUNCATE fails SQLITE_BUSY while any
            // other handle holds a read lock on the WAL, and the second
            // history handle is exactly that. FULL still syncs the WAL and the
            // database file, and waits out a reader through the connection's
            // busy timeout.
            conn.pragma_update(None, "synchronous", "FULL").ok();
            let busy = conn.query_row("PRAGMA wal_checkpoint(FULL)", [], |r| r.get::<_, i64>(0));
            conn.pragma_update(None, "synchronous", "NORMAL").ok();
            match busy {
                // Column 0 is the busy flag: non-zero means the checkpoint
                // could not finish, so the pages are not known to be on disk
                // and the caller must NOT write its marker yet.
                Ok(0) => Ok(()),
                Ok(_) => Err("wal checkpoint did not complete".to_string()),
                Err(rusqlite::Error::QueryReturnedNoRows) => Ok(()),
                Err(e) => Err(e.to_string()),
            }
        })
        .await
        .map_err(|e| IrrigationControlError::Sqlite(format!("join: {e}")))?
        .map_err(IrrigationControlError::Sqlite)
    }

    /// Set the sticky global override. Caller validates mode is auto/skip/run.
    pub async fn set_global_override(&self, mode: String) -> Result<(), IrrigationControlError> {
        let c = self.conn.clone();
        tokio::task::spawn_blocking(move || -> rusqlite::Result<()> {
            let conn = c.blocking_lock();
            conn.execute(
                "INSERT INTO irrigation_control (id, global_override, updated_at_epoch)
                 VALUES (1, ?1, strftime('%s','now'))
                 ON CONFLICT(id) DO UPDATE SET
                    global_override  = excluded.global_override,
                    updated_at_epoch = excluded.updated_at_epoch",
                params![mode],
            )?;
            Ok(())
        })
        .await
        .map_err(|e| IrrigationControlError::Sqlite(format!("join: {e}")))?
        .map_err(|e| IrrigationControlError::Sqlite(e.to_string()))
    }

    /// All per-zone sticky overrides as slug -> mode. Zones absent here are
    /// "auto". Returns empty on any read failure (safe: no override applied).
    pub async fn zone_overrides(&self) -> HashMap<String, String> {
        let c = self.conn.clone();
        let res =
            tokio::task::spawn_blocking(move || -> rusqlite::Result<HashMap<String, String>> {
                let conn = c.blocking_lock();
                let mut stmt =
                    conn.prepare("SELECT zone_slug, override_mode FROM zone_overrides")?;
                let rows =
                    stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
                let mut map = HashMap::new();
                for row in rows {
                    let (slug, mode) = row?;
                    map.insert(slug, mode);
                }
                Ok(map)
            })
            .await;
        match res {
            Ok(Ok(map)) => map,
            Ok(Err(e)) => {
                tracing::warn!(error = %e, "zone_overrides read failed; treating as none");
                HashMap::new()
            }
            Err(e) => {
                tracing::warn!(error = %e, "zone_overrides read join failed; treating as none");
                HashMap::new()
            }
        }
    }

    /// Set (or, for "auto", clear) a per-zone sticky override. Caller validates
    /// mode is auto/skip/run. "auto" deletes the row so it falls back cleanly.
    pub async fn set_zone_override(
        &self,
        slug: String,
        mode: String,
    ) -> Result<(), IrrigationControlError> {
        let c = self.conn.clone();
        tokio::task::spawn_blocking(move || -> rusqlite::Result<()> {
            let conn = c.blocking_lock();
            if mode == "auto" {
                conn.execute(
                    "DELETE FROM zone_overrides WHERE zone_slug = ?1",
                    params![slug],
                )?;
            } else {
                conn.execute(
                    "INSERT INTO zone_overrides (zone_slug, override_mode, updated_at_epoch)
                     VALUES (?1, ?2, strftime('%s','now'))
                     ON CONFLICT(zone_slug) DO UPDATE SET
                        override_mode    = excluded.override_mode,
                        updated_at_epoch = excluded.updated_at_epoch",
                    params![slug, mode],
                )?;
            }
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

    // The adoption pass writes these columns and then fsyncs a config marker
    // that retires the matching read. WAL + synchronous=NORMAL does not fsync
    // on commit, so without this flush the durable order is the reverse of the
    // documented one: on a power cut the markers survive and the pause does
    // not, which is the outcome the ordering exists to prevent. Proven on a
    // real file-backed WAL database, read back through a second connection.
    #[tokio::test]
    async fn flush_durable_puts_the_control_row_where_another_connection_sees_it() {
        let mut dir = std::env::temp_dir();
        dir.push(format!("localsky-flush-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("localsky.db");

        let mut conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "synchronous", "NORMAL").ok();
        crate::persistence::run_migrations(&mut conn).unwrap();
        let store = IrrigationControlStore::new(Arc::new(Mutex::new(conn)));

        store.set_pause_until(1_900_000_000).await.unwrap();
        store.set_paused(true).await.unwrap();
        store.flush_durable().await.unwrap();

        // Read back the MAIN DATABASE FILE ALONE: copied without its -wal and
        // -shm, which is exactly what a power cut leaves behind. A committed
        // WAL transaction is visible to any second handle whether or not a
        // checkpoint ran, so reading through one would pass with the whole
        // body of flush_durable replaced by Ok(()). The row is in this copy
        // only because the checkpoint put it there.
        let bare = dir.join("main-only.db");
        std::fs::copy(&path, &bare).unwrap();
        let other = Connection::open(&bare).unwrap();
        let (epoch, paused): (i64, bool) = other
            .query_row(
                "SELECT pause_until_epoch, is_paused FROM irrigation_control WHERE id = 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("the checkpoint did not land the row in the database file");
        assert_eq!(epoch, 1_900_000_000);
        assert!(paused);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // The branch the whole design exists for, and the one nothing covered.
    // `PRAGMA wal_checkpoint(FULL)` reports "could not finish" as a ROW with a
    // non-zero busy flag rather than as an error, so reading it as "any row
    // means done" would let the pass fsync its config marker over control
    // values still sitting only in the WAL. A reader pinned on an older
    // snapshot is exactly what the second history handle is in production.
    #[tokio::test]
    async fn a_checkpoint_blocked_by_a_reader_is_a_failed_flush() {
        let mut dir = std::env::temp_dir();
        dir.push(format!("localsky-flush-busy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("localsky.db");

        let mut conn = Connection::open(&path).unwrap();
        conn.pragma_update(None, "journal_mode", "WAL").ok();
        conn.pragma_update(None, "synchronous", "NORMAL").ok();
        conn.busy_timeout(std::time::Duration::ZERO).ok();
        crate::persistence::run_migrations(&mut conn).unwrap();
        let store = IrrigationControlStore::new(Arc::new(Mutex::new(conn)));
        store.set_paused(false).await.unwrap();

        // A second handle holding a read transaction open on the snapshot as
        // it stands now.
        let reader = Connection::open(&path).unwrap();
        reader.busy_timeout(std::time::Duration::ZERO).ok();
        reader.execute_batch("BEGIN").unwrap();
        let _: i64 = reader
            .query_row("SELECT count(*) FROM irrigation_control", [], |r| r.get(0))
            .unwrap();

        // Frames written past that snapshot: the checkpoint cannot complete.
        store.set_pause_until(1_900_000_000).await.unwrap();
        let err = store
            .flush_durable()
            .await
            .expect_err("a checkpoint that could not finish must not report success");
        assert!(
            format!("{err}").contains("wal checkpoint did not complete"),
            "a busy checkpoint has to surface as an error, got {err}"
        );

        reader.execute_batch("COMMIT").unwrap();
        assert!(
            store.flush_durable().await.is_ok(),
            "and succeed once the reader lets go"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    // In-memory and rollback-journal databases have no WAL, and the flush has
    // to succeed rather than fail the commit on them: the adoption pass treats
    // a failed flush as a reason to mark nothing.
    #[tokio::test]
    async fn flush_durable_is_a_no_op_without_a_wal() {
        let s = store().await;
        s.set_dry_run(true).await.unwrap();
        s.flush_durable().await.unwrap();
        assert!(s.get().await.is_dry_run);
    }

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

    #[test]
    fn one_day_override_expires_when_the_local_date_moves_on() {
        // Set on the 4th, read on the 4th: live.
        assert_eq!(
            effective_override_tomorrow("skip", "2026-09-04", "2026-09-04"),
            "skip"
        );
        // Read on the 5th: tomorrow is a different day now, so it is spent.
        assert_eq!(
            effective_override_tomorrow("skip", "2026-09-04", "2026-09-05"),
            "none"
        );
        // A "run" override expires the same way; nothing here is one-sided.
        assert_eq!(
            effective_override_tomorrow("run", "2026-09-04", "2026-09-05"),
            "none"
        );
        // An unstamped row reads as none rather than as a permanent override.
        assert_eq!(
            effective_override_tomorrow("skip", "", "2026-09-05"),
            "none"
        );
    }

    #[tokio::test]
    async fn override_tomorrow_resets_at_the_next_local_midnight() {
        let s = store().await;
        s.set_override_tomorrow_on("skip".to_string(), "2026-09-04".to_string())
            .await
            .unwrap();
        assert_eq!(s.get_on("2026-09-04").await.override_tomorrow, "skip");
        assert_eq!(
            s.get_on("2026-09-05").await.override_tomorrow,
            "none",
            "the one-day override must not survive the day it was set on"
        );
    }

    #[tokio::test]
    async fn set_paused_persists_without_clobbering_pause_until() {
        let s = store().await;
        s.set_pause_until(1_900_000_000).await.unwrap();
        s.set_paused(true).await.unwrap();
        let st = s.get().await;
        assert!(st.is_paused);
        assert_eq!(st.pause_until_epoch, 1_900_000_000);
        assert!(!st.is_dry_run, "the two toggles are independent");
        s.set_paused(false).await.unwrap();
        assert!(!s.get().await.is_paused);
    }

    #[tokio::test]
    async fn set_dry_run_persists_without_clobbering_the_pause_toggle() {
        let s = store().await;
        s.set_paused(true).await.unwrap();
        s.set_dry_run(true).await.unwrap();
        let st = s.get().await;
        assert!(st.is_dry_run);
        assert!(st.is_paused, "dry-run set must not clear the pause toggle");
    }

    #[tokio::test]
    async fn m0017_defaults_are_false_on_a_populated_row() {
        // The upgrade path the ALTER TABLEs actually take: a database that
        // already carries a control row gets the three new columns, and the
        // two toggles must read false rather than NULL or true.
        let mut conn = Connection::open_in_memory().unwrap();
        crate::persistence::run_migrations(&mut conn).unwrap();
        conn.execute(
            "UPDATE irrigation_control SET pause_until_epoch = 123 WHERE id = 1",
            [],
        )
        .unwrap();
        let s = IrrigationControlStore::new(Arc::new(Mutex::new(conn)));
        let st = s.get().await;
        assert_eq!(st.pause_until_epoch, 123);
        assert!(!st.is_paused);
        assert!(!st.is_dry_run);
    }
}
