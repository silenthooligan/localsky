-- M0016: tuning_dismissals
--
-- Operator-silenced tuning recommendations. Two kinds:
--   'snooze':    keys the exact recommendation id; expires at
--                until_epoch (30 days from creation). The same
--                recommendation may return after expiry.
--   'permanent': keys (zone_slug, field) and never expires, so a
--                recommendation whose suggested value drifts stays
--                dismissed.
--
-- Silencing is total: a dismissed/snoozed recommendation is stripped
-- from the report server-side, so every consumer (zone cards, KPI
-- counts, the strip, auto-select, and the weekly push trigger) goes
-- quiet together.

CREATE TABLE IF NOT EXISTS tuning_dismissals (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    zone_slug     TEXT    NOT NULL,
    field         TEXT    NOT NULL,
    rec_id        TEXT,                 -- snooze only: the recommendation id
    kind          TEXT    NOT NULL,     -- 'snooze' | 'permanent'
    until_epoch   INTEGER,              -- snooze only: expiry, UTC epoch
    created_epoch INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_tuning_dismissals_zone_field
    ON tuning_dismissals(zone_slug, field);
