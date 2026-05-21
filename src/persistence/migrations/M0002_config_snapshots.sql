-- M0002_config_snapshots.sql
-- Versioned config history. Every PUT /api/config snapshots the
-- previous Config blob here before writing the new one. Powers
-- /api/config/rollback?to=<version>. The runner caps retention at 20
-- versions via a trigger (oldest dropped on insert past the cap).

CREATE TABLE IF NOT EXISTS config_snapshots (
    version        INTEGER PRIMARY KEY AUTOINCREMENT,
    applied_at     INTEGER NOT NULL,
    schema_version INTEGER NOT NULL,
    note           TEXT,
    blob           TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_config_snapshots_applied_at
    ON config_snapshots(applied_at DESC);

-- Retention: keep newest 20. Cheap trigger because the table is tiny
-- (at most a few snapshots per day on the busiest setups).
CREATE TRIGGER IF NOT EXISTS trg_config_snapshots_retention
AFTER INSERT ON config_snapshots
BEGIN
    DELETE FROM config_snapshots
    WHERE version IN (
        SELECT version FROM config_snapshots
        ORDER BY version DESC
        LIMIT -1 OFFSET 20
    );
END;
