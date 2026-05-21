-- M0005_verdict_history.sql
-- Rename legacy `decisions` to `verdict_history` and add inputs_json +
-- date_local so the engine can replay any historical decision through
-- the new skip_rules engine and verify behavior is unchanged across
-- v0.1 -> v2 upgrades.

-- Step 1: ensure legacy schema exists for fresh installs.
CREATE TABLE IF NOT EXISTS decisions (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    epoch       INTEGER NOT NULL,
    verdict     TEXT    NOT NULL,
    reason      TEXT    NOT NULL,
    UNIQUE(epoch)
);

-- Step 2: build the v2 table.
CREATE TABLE verdict_history (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    epoch       INTEGER NOT NULL,
    date_local  TEXT    NOT NULL DEFAULT '',
    verdict     TEXT    NOT NULL,
    reason      TEXT    NOT NULL,
    inputs_json TEXT    NOT NULL DEFAULT '{}',
    UNIQUE(epoch)
);

-- Step 3: copy data forward. date_local is derived from epoch in UTC;
-- the wall-clock-local conversion happens in the application layer.
INSERT INTO verdict_history (epoch, date_local, verdict, reason, inputs_json)
SELECT epoch, strftime('%Y-%m-%d', epoch, 'unixepoch'), verdict, reason, '{}'
FROM decisions;

-- Step 4: drop legacy.
DROP TABLE decisions;

-- Step 5: indexes.
CREATE INDEX IF NOT EXISTS idx_verdict_history_epoch
    ON verdict_history(epoch DESC);
CREATE INDEX IF NOT EXISTS idx_verdict_history_date
    ON verdict_history(date_local);
