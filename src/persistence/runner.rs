// Hand-rolled SQLite migration runner. Migrations are baked into the
// binary via include_str! so the deploy image needs no extra files.
// Each migration runs in a transaction; partial application is impossible
// because rusqlite rolls back any error before applied_at is recorded.
//
// Why not refinery: refinery pulls a sizable proc-macro tree and forces
// migrations into a specific module layout. This file is 80 lines and
// reviewable by anyone reading SQL.

use rusqlite::{params, Connection, Transaction};
use thiserror::Error;

#[derive(Debug, Clone, Copy)]
pub struct Migration {
    pub version: &'static str,
    pub name: &'static str,
    pub sql: &'static str,
}

/// All known migrations, in the order they must apply. Append-only.
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: "M0001",
        name: "init",
        sql: include_str!("migrations/M0001_init.sql"),
    },
    Migration {
        version: "M0002",
        name: "config_snapshots",
        sql: include_str!("migrations/M0002_config_snapshots.sql"),
    },
    Migration {
        version: "M0003",
        name: "runs_v2",
        sql: include_str!("migrations/M0003_runs.sql"),
    },
    Migration {
        version: "M0004",
        name: "sensor_history",
        sql: include_str!("migrations/M0004_sensor_history.sql"),
    },
    Migration {
        version: "M0005",
        name: "verdict_history",
        sql: include_str!("migrations/M0005_verdict_history.sql"),
    },
    Migration {
        version: "M0006",
        name: "forecast_observations",
        sql: include_str!("migrations/M0006_forecast_observations.sql"),
    },
    Migration {
        version: "M0007",
        name: "decision_trace",
        sql: include_str!("migrations/M0007_decision_trace.sql"),
    },
    Migration {
        version: "M0008",
        name: "irrigation_control",
        sql: include_str!("migrations/M0008_irrigation_control.sql"),
    },
    Migration {
        version: "M0009",
        name: "auth",
        sql: include_str!("migrations/M0009_auth.sql"),
    },
    Migration {
        version: "M0010",
        name: "push_subscriptions",
        sql: include_str!("migrations/M0010_push_subscriptions.sql"),
    },
    Migration {
        version: "M0011",
        name: "overrides",
        sql: include_str!("migrations/M0011_overrides.sql"),
    },
];

#[derive(Debug, Error)]
pub enum MigrationError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("migration {version} failed: {detail}")]
    Failed { version: String, detail: String },
}

/// Compile-time-ish guarantee that MIGRATIONS is monotonically ordered
/// by version. Called once at the top of `run()` so a misordered
/// migration entry trips immediately with a clear panic rather than
/// silently applying out of sequence.
fn assert_monotonic() {
    for w in MIGRATIONS.windows(2) {
        assert!(
            w[0].version < w[1].version,
            "MIGRATIONS slice must be monotonically ordered by version; \
             got {} >= {} (insertion order error in src/persistence/runner.rs)",
            w[0].version,
            w[1].version,
        );
    }
}

/// Run all pending migrations. Returns the list of versions newly applied
/// in this call (already-applied ones are skipped). Idempotent.
pub fn run(conn: &mut Connection) -> Result<Vec<String>, MigrationError> {
    assert_monotonic();
    // Bootstrap: M0001 creates schema_migrations. We must check whether
    // the table exists before SELECTing from it.
    let has_table: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='schema_migrations'",
        [],
        |r| r.get(0),
    )?;

    let mut newly_applied = Vec::new();

    if has_table == 0 {
        // Always apply M0001 first when bootstrapping.
        let tx = conn.transaction()?;
        apply(&tx, &MIGRATIONS[0])?;
        tx.commit()?;
        newly_applied.push(MIGRATIONS[0].version.to_string());
        // For legacy v0.1 DBs that pre-date the runner, mark the implicit
        // schema state so we don't try to re-create runs/push_subscriptions.
        backfill_legacy(conn)?;
    }

    let applied_versions: Vec<String> = conn
        .prepare("SELECT version FROM schema_migrations")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<Result<_, _>>()?;

    for m in MIGRATIONS {
        if applied_versions.iter().any(|v| v == m.version) {
            continue;
        }
        let tx = conn.transaction()?;
        apply(&tx, m)?;
        tx.commit()?;
        newly_applied.push(m.version.to_string());
    }
    Ok(newly_applied)
}

fn apply(tx: &Transaction, m: &Migration) -> Result<(), MigrationError> {
    tx.execute_batch(m.sql)
        .map_err(|e| MigrationError::Failed {
            version: m.version.to_string(),
            detail: e.to_string(),
        })?;
    tx.execute(
        "INSERT OR REPLACE INTO schema_migrations(version, name, applied_at) VALUES (?, ?, ?)",
        params![m.version, m.name, now_epoch()],
    )?;
    Ok(())
}

/// Pre-runner v0.1 databases have `runs` and `push_subscriptions` tables
/// created by ad-hoc CREATE-IF-NOT-EXISTS statements in
/// history/ingest.rs and push/store.rs. Mark those as if the eventual
/// migrations (M0003 runs, M0007 push_subscriptions) had already run,
/// so the runner doesn't try to re-create them with different schemas.
/// Phase 4B+ will add explicit migrations for new columns.
fn backfill_legacy(conn: &Connection) -> Result<(), MigrationError> {
    let legacy_runs: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='runs'",
        [],
        |r| r.get(0),
    )?;
    let legacy_push: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='push_subscriptions'",
        [],
        |r| r.get(0),
    )?;
    let now = now_epoch();
    // Note: legacy `runs` is NOT marked here -- M0003 evolves the legacy
    // schema to v2 via a table-rebuild that handles both fresh and
    // legacy databases. Only mark the tables whose migration is not yet
    // written (push_subscriptions schema lives in Phase 4C; until that
    // migration ships the legacy table stands.)
    let _ = legacy_runs;
    if legacy_push > 0 {
        conn.execute(
            "INSERT OR IGNORE INTO schema_migrations(version, name, applied_at) VALUES ('M0007_legacy', 'push_subscriptions (legacy store)', ?)",
            params![now],
        )?;
    }
    Ok(())
}

fn now_epoch() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fresh_conn() -> Connection {
        Connection::open_in_memory().unwrap()
    }

    #[test]
    fn migrations_slice_is_monotonic() {
        // Tripping this means a future contributor inserted an
        // out-of-order MIGRATIONS entry; assert_monotonic() in run()
        // would panic at boot. This test catches it at PR time instead.
        for w in MIGRATIONS.windows(2) {
            assert!(
                w[0].version < w[1].version,
                "MIGRATIONS out of order: {} >= {}",
                w[0].version,
                w[1].version
            );
        }
    }

    #[test]
    fn fresh_db_applies_all_migrations() {
        let mut c = fresh_conn();
        let applied = run(&mut c).unwrap();
        assert_eq!(
            applied,
            vec![
                "M0001".to_string(),
                "M0002".to_string(),
                "M0003".to_string(),
                "M0004".to_string(),
                "M0005".to_string(),
                "M0006".to_string(),
                "M0007".to_string(),
                "M0008".to_string(),
                "M0009".to_string(),
                "M0010".to_string(),
                "M0011".to_string(),
            ]
        );
    }

    #[test]
    fn second_run_applies_nothing() {
        let mut c = fresh_conn();
        run(&mut c).unwrap();
        let again = run(&mut c).unwrap();
        assert!(
            again.is_empty(),
            "second run should be no-op, got {:?}",
            again
        );
    }

    #[test]
    fn schema_migrations_records_versions() {
        let mut c = fresh_conn();
        run(&mut c).unwrap();
        let versions: Vec<String> = c
            .prepare("SELECT version FROM schema_migrations ORDER BY version")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(versions.contains(&"M0001".to_string()));
        assert!(versions.contains(&"M0002".to_string()));
    }

    #[test]
    fn legacy_push_subscriptions_table_backfilled() {
        let mut c = fresh_conn();
        c.execute_batch("CREATE TABLE push_subscriptions (endpoint TEXT, auth TEXT, p256dh TEXT);")
            .unwrap();
        run(&mut c).unwrap();
        let versions: Vec<String> = c
            .prepare("SELECT version FROM schema_migrations")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(versions.contains(&"M0007_legacy".to_string()));
    }

    #[test]
    fn legacy_runs_evolves_to_v2_schema() {
        let mut c = fresh_conn();
        // Simulate a v0.1 DB: runs table exists with legacy schema and
        // a row. Migration should preserve the row + evolve the schema.
        c.execute_batch(
            "CREATE TABLE runs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                zone TEXT NOT NULL,
                start_epoch INTEGER NOT NULL,
                duration_s INTEGER NOT NULL,
                skip_reason TEXT,
                UNIQUE(zone, start_epoch)
            );
            INSERT INTO runs(zone, start_epoch, duration_s, skip_reason)
                VALUES ('back_yard', 1700000000, 600, NULL);",
        )
        .unwrap();

        run(&mut c).unwrap();

        // v2 columns are present.
        let cols: Vec<String> = c
            .prepare("SELECT name FROM pragma_table_info('runs')")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        for col in [
            "zone_slug",
            "end_epoch",
            "source",
            "controller_id",
            "status",
            "et0_mm",
            "etc_mm",
            "applied_mm",
            "cycle_index",
            "cycle_count",
        ] {
            assert!(cols.contains(&col.to_string()), "missing v2 column: {col}");
        }

        // Legacy row carried forward into zone_slug.
        let (slug, dur, status): (String, i64, String) = c
            .query_row(
                "SELECT zone_slug, duration_s, status FROM runs WHERE start_epoch = 1700000000",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(slug, "back_yard");
        assert_eq!(dur, 600);
        assert_eq!(status, "completed");
    }

    #[test]
    fn config_snapshots_retention_trigger_caps_at_20() {
        let mut c = fresh_conn();
        run(&mut c).unwrap();
        // Insert 25 snapshots.
        for i in 0..25 {
            c.execute(
                "INSERT INTO config_snapshots(applied_at, schema_version, note, blob) VALUES (?, 1, ?, ?)",
                params![1_700_000_000 + i, format!("note {i}"), "{}"],
            )
            .unwrap();
        }
        let count: i64 = c
            .query_row("SELECT COUNT(*) FROM config_snapshots", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 20, "trigger should cap retention at 20");
    }
}
