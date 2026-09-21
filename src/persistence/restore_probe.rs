//! Validate a self-contained LocalSky database before restore staging or boot.
//! The migration proof uses a disposable copy and never upgrades the supplied file.

use rusqlite::Connection;

#[derive(Debug, thiserror::Error)]
pub(crate) enum RestoreProbeError {
    #[error("LS_RESTORE_SCHEMA: {0}")]
    Schema(String),
    #[error("{0}")]
    Cause(#[source] Box<crate::failure::Failure>),
}
impl From<String> for RestoreProbeError {
    fn from(value: String) -> Self {
        Self::Schema(value)
    }
}
impl From<&str> for RestoreProbeError {
    fn from(value: &str) -> Self {
        Self::Schema(value.into())
    }
}
impl RestoreProbeError {
    pub fn diagnostic(&self) -> crate::failure::Failure {
        match self {
            Self::Schema(_) => crate::failure::Failure::new(
                crate::failure::FailureCode::RestoreSchema,
                "restore database schema proof",
            ),
            Self::Cause(failure) => (**failure).clone(),
        }
    }
}
fn cause(error: &(dyn std::error::Error + 'static), operation: &'static str) -> RestoreProbeError {
    RestoreProbeError::Cause(Box::new(crate::diagnostics::from_error(error, operation)))
}

static PROBE_TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Scratch files are unique per proof and never candidates for a later restore.
struct ProbeFileGuard(String);

impl Drop for ProbeFileGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// Required schema materialized by the migrations, compared structurally so
/// whitespace in historic CREATE statements does not reject a usable backup.
fn require_migration_schema(
    conn: &Connection,
    expected: &Connection,
) -> Result<(), RestoreProbeError> {
    type ColumnShape = (String, String, i64, Option<String>, i64, i64);
    type IndexShape = (String, i64, String, i64);
    type IndexField = (Option<String>, i64, Option<String>, i64);
    let tables: Vec<String> = expected.prepare(
        "SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
    ).and_then(|mut stmt| stmt.query_map([], |row| row.get(0))?.collect())
        .map_err(|e| cause(&e, "reference schema"))?;
    for table in tables {
        let columns = |db: &Connection| -> rusqlite::Result<Vec<ColumnShape>> {
            db.prepare("SELECT name, upper(type), \"notnull\", dflt_value, pk, hidden FROM pragma_table_xinfo(?1) ORDER BY name")?
                .query_map([&table], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)))?
                .collect()
        };
        let actual = columns(conn).map_err(|e| cause(&e, "db table"))?;
        for column in columns(expected).map_err(|e| cause(&e, "reference table"))? {
            if !actual.contains(&column) {
                return Err(format!("db schema does not match its migration history: {table}.{} is missing or incompatible", column.0).into());
            }
        }
        let indexes = |db: &Connection| -> rusqlite::Result<Vec<IndexShape>> {
            db.prepare(
                "SELECT name, \"unique\", origin, partial FROM pragma_index_list(?1) ORDER BY name",
            )?
            .query_map([&table], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect()
        };
        let actual = indexes(conn).map_err(|e| cause(&e, "db indexes for"))?;
        for index in indexes(expected).map_err(|e| cause(&e, "reference indexes for"))? {
            if !actual.contains(&index) {
                return Err(format!("db schema does not match its migration history: index {} is missing or incompatible", index.0).into());
            }
            let fields = |db: &Connection| -> rusqlite::Result<Vec<IndexField>> {
                db.prepare(
                    "SELECT name, \"desc\", coll, key FROM pragma_index_xinfo(?1) ORDER BY seqno",
                )?
                .query_map([&index.0], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
                })?
                .collect()
            };
            if fields(conn).map_err(|e| cause(&e, "db index"))?
                != fields(expected).map_err(|e| cause(&e, "reference index"))?
            {
                return Err(format!("db schema does not match its migration history: index {} has incompatible columns", index.0).into());
            }
        }
    }
    Ok(())
}

/// Validate the ledger as a contiguous prefix of migrations this binary knows.
/// The one legacy marker is produced by runner::backfill_legacy and is not a
/// numbered schema migration. Unknown future versions are never silently skipped.
fn supported_migration_prefix(conn: &Connection) -> Result<usize, RestoreProbeError> {
    let reference =
        Connection::open_in_memory().map_err(|e| cause(&e, "restore reference database"))?;
    reference
        .execute_batch(crate::persistence::MIGRATIONS[0].sql)
        .map_err(|e| cause(&e, "restore reference database"))?;
    require_migration_schema(conn, &reference)?;
    let rows: Vec<(String, String, i64)> = conn
        .prepare("SELECT version, name, applied_at FROM schema_migrations ORDER BY version")
        .and_then(|mut stmt| {
            stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                .collect()
        })
        .map_err(|e| cause(&e, "db migration ledger is malformed"))?;
    let mut prefix = 0;
    for (version, name, applied_at) in rows {
        if applied_at < 0 {
            return Err("db migration ledger has an invalid timestamp".into());
        }
        if version == "M0007_legacy" {
            if name != "push_subscriptions (legacy store)" {
                return Err("db migration ledger has an invalid legacy marker".into());
            }
            let push = crate::persistence::MIGRATIONS
                .iter()
                .find(|m| m.version == "M0010")
                .ok_or_else(|| "reference legacy push migration is missing".to_string())?;
            reference
                .execute_batch(push.sql)
                .map_err(|e| cause(&e, "reference legacy push schema"))?;
            require_migration_schema(conn, &reference)?;
            continue;
        }
        let Some(migration) = crate::persistence::MIGRATIONS.get(prefix) else {
            return Err(format!("db migration {version} is unsupported by this LocalSky").into());
        };
        if version != migration.version || name != migration.name {
            return Err(format!("db migration history is unsupported or inconsistent at {version}; expected {} ({})", migration.version, migration.name).into());
        }
        prefix += 1;
    }
    if prefix == 0 {
        return Err("db migration history is empty".into());
    }
    Ok(prefix)
}

/// Prove a self-contained upload can migrate to the current schema. The
/// original bytes stay read-only; pending migrations run on a disposable copy.
/// Claimed migration rows alone are insufficient: the corresponding tables,
/// columns and indexes must already exist before the normal runner is invoked.
pub(crate) fn probe_localsky_db(path: &str) -> Result<(), RestoreProbeError> {
    let conn = Connection::open_with_flags(
        path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| cause(&e, "db is not readable as a SQLite database"))?;
    let has_ledger: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='schema_migrations'",
            [],
            |row| row.get(0),
        )
        .map_err(|e| cause(&e, "db is not readable as a SQLite database"))?;
    if has_ledger == 0 {
        return Err("db is a SQLite file but not a LocalSky database (it has no schema_migrations table); upload the irrigation.db from a LocalSky backup bundle".into());
    }
    let integrity: String = conn
        .query_row("PRAGMA quick_check", [], |row| row.get(0))
        .map_err(|e| cause(&e, "db integrity check failed"))?;
    if integrity != "ok" {
        return Err(format!("db integrity check failed: {integrity}").into());
    }
    let prefix = supported_migration_prefix(&conn)?;
    let mut expected =
        Connection::open_in_memory().map_err(|e| cause(&e, "restore reference database"))?;
    for migration in crate::persistence::MIGRATIONS.iter().take(prefix) {
        expected
            .execute_batch(migration.sql)
            .map_err(|e| cause(&e, "reference migration"))?;
    }
    require_migration_schema(&conn, &expected)?;
    drop(conn);

    let proof_path = format!(
        "{path}.migration-proof-{}-{}",
        std::process::id(),
        PROBE_TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let _proof = ProbeFileGuard(proof_path.clone());
    let _wal = ProbeFileGuard(format!("{proof_path}-wal"));
    let _shm = ProbeFileGuard(format!("{proof_path}-shm"));
    let _journal = ProbeFileGuard(format!("{proof_path}-journal"));
    std::fs::copy(path, &proof_path).map_err(|e| cause(&e, "db migration probe copy"))?;
    let mut proof =
        Connection::open(&proof_path).map_err(|e| cause(&e, "db migration probe open"))?;
    crate::persistence::run_migrations(&mut proof)
        .map_err(|e| cause(&e, "db cannot migrate to this LocalSky"))?;
    // The reference has no ledger rows, so build the final canonical schema in
    // a new connection using the very same runner boot uses.
    expected = Connection::open_in_memory().map_err(|e| cause(&e, "restore reference database"))?;
    crate::persistence::run_migrations(&mut expected)
        .map_err(|e| cause(&e, "reference migrations"))?;
    require_migration_schema(&proof, &expected)?;
    Ok(())
}
