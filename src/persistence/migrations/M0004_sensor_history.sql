-- M0004_sensor_history.sql
-- Generic time-series store for per-source per-field readings.
--
-- The engine's Penman-Monteith path needs daily ET0 integration; the
-- merge layer needs per-source last-seen values; the dashboard wants
-- spark history. One table services all of these.
--
-- Composite primary key (epoch, source_id, key) means an idempotent
-- INSERT OR IGNORE handles double-writes from chatty sources. WITHOUT
-- ROWID lets the table act as a single B-tree on the PK columns,
-- making (key, epoch DESC) range scans cheap.

CREATE TABLE IF NOT EXISTS sensor_history (
    epoch     INTEGER NOT NULL,
    source_id TEXT    NOT NULL,
    key       TEXT    NOT NULL,
    value     REAL    NOT NULL,
    PRIMARY KEY (epoch, source_id, key)
) WITHOUT ROWID;

CREATE INDEX IF NOT EXISTS idx_sh_key_epoch ON sensor_history(key, epoch DESC);
CREATE INDEX IF NOT EXISTS idx_sh_source_epoch ON sensor_history(source_id, epoch DESC);
