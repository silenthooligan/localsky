-- M0011: sticky global + per-zone irrigation override
--
-- Distinct from M0008's one-day `override_tomorrow` (which an operator/HA
-- automation reverts each midnight). These overrides are STICKY: they hold
-- until the operator sets them back to 'auto'. Mode is auto | skip | run:
--   auto = follow the engine (and the global, for a zone)
--   skip = force-skip
--   run  = force-run, overriding the skip conditions (rain/restriction/
--          soil-saturation/wind); the schedule still decides WHEN.
--
-- Decision precedence: zone override > global override > override_tomorrow
-- (legacy one-cycle) > engine verdict.

-- Global sticky override on the singleton control row.
ALTER TABLE irrigation_control ADD COLUMN global_override TEXT NOT NULL DEFAULT 'auto';

-- Per-zone sticky overrides, keyed by zone slug. A missing row = 'auto'.
CREATE TABLE IF NOT EXISTS zone_overrides (
    zone_slug        TEXT    PRIMARY KEY,
    override_mode    TEXT    NOT NULL DEFAULT 'auto',  -- auto | skip | run
    updated_at_epoch INTEGER NOT NULL DEFAULT 0
);
