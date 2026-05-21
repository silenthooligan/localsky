-- M0003_runs.sql
-- Evolve `runs` to the v2 schema.
--
-- v0.1 columns: id, zone, start_epoch, duration_s, skip_reason
-- v2 columns:   id, zone_slug, start_epoch, end_epoch, duration_s, source,
--               controller_id, status, skip_reason, et0_mm, etc_mm,
--               applied_mm, cycle_index, cycle_count
--
-- Strategy: SQLite has limited ALTER TABLE, so we use the canonical
-- table-rebuild pattern. For fresh DBs the CREATE IF NOT EXISTS no-ops
-- the legacy schema then we rebuild; for legacy DBs the v0.1 rows copy
-- forward (zone -> zone_slug). Source + controller_id default to
-- 'unknown' for migrated rows; the v2 scheduler tags new inserts.

-- Step 1: ensure legacy schema exists for fresh installs (no-op on legacy).
CREATE TABLE IF NOT EXISTS runs (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    zone          TEXT    NOT NULL,
    start_epoch   INTEGER NOT NULL,
    duration_s    INTEGER NOT NULL,
    skip_reason   TEXT,
    UNIQUE(zone, start_epoch)
);

-- Step 2: build the v2 table.
CREATE TABLE runs_v2 (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    zone_slug     TEXT    NOT NULL,
    start_epoch   INTEGER NOT NULL,
    end_epoch     INTEGER,
    duration_s    INTEGER,
    source        TEXT    NOT NULL DEFAULT 'unknown',
    controller_id TEXT    NOT NULL DEFAULT 'unknown',
    status        TEXT    NOT NULL DEFAULT 'completed',
    skip_reason   TEXT,
    et0_mm        REAL,
    etc_mm        REAL,
    applied_mm    REAL,
    cycle_index   INTEGER,
    cycle_count   INTEGER
);

-- Step 3: copy data forward. zone -> zone_slug; legacy duration is
-- already final so status='completed' is correct.
INSERT INTO runs_v2 (zone_slug, start_epoch, duration_s, skip_reason)
SELECT zone, start_epoch, duration_s, skip_reason FROM runs;

-- Step 4: replace.
DROP TABLE runs;
ALTER TABLE runs_v2 RENAME TO runs;

-- Step 5: indexes for the v2 query patterns.
CREATE INDEX IF NOT EXISTS idx_runs_zone_start ON runs(zone_slug, start_epoch DESC);
CREATE INDEX IF NOT EXISTS idx_runs_start ON runs(start_epoch DESC);
-- Uniqueness includes controller_id so two controllers can't double-log
-- the same start instant for the same zone (rare but theoretically
-- possible with HaServiceCall + OpenSprinklerDirect both pointed at OS).
CREATE UNIQUE INDEX IF NOT EXISTS uq_runs_zone_start_ctrl
    ON runs(zone_slug, start_epoch, controller_id);
-- Partial index for the scheduler's "what's still in flight?" query.
CREATE INDEX IF NOT EXISTS idx_runs_in_flight
    ON runs(status, zone_slug) WHERE status IN ('running', 'intended');
