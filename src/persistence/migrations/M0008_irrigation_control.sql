-- M0008: irrigation_control
--
-- Standalone (no-Home-Assistant) control surface: the vacation pause
-- (pause_until_epoch) + one-day override (override_tomorrow). In HA mode
-- these live in HA helpers (input_datetime.irrigation_pause_until +
-- input_select.irrigation_override_tomorrow) and the snapshot builder
-- reads them from /api/states. With no HA there is nowhere to put them,
-- so the native snapshot builder reads them here instead and a standalone
-- deploy can actually be paused.
--
-- Persisted (not in-memory) on purpose: Komodo redeploys the stack on
-- every push, so an in-memory pause would be silently dropped on restart
-- and the lawn would water on a day the operator paused it. A single row
-- (id = 1, enforced by CHECK) holds the whole control surface; setters
-- UPSERT it.

CREATE TABLE IF NOT EXISTS irrigation_control (
    id                INTEGER PRIMARY KEY CHECK (id = 1),
    pause_until_epoch INTEGER NOT NULL DEFAULT 0,      -- UTC epoch; 0 = no pause
    override_tomorrow TEXT    NOT NULL DEFAULT 'none', -- none | skip | run
    updated_at_epoch  INTEGER NOT NULL DEFAULT 0
);

-- Seed the singleton row so setters can rely on it existing. INSERT OR
-- IGNORE keeps this idempotent if the migration is ever re-derived.
INSERT OR IGNORE INTO irrigation_control
    (id, pause_until_epoch, override_tomorrow, updated_at_epoch)
    VALUES (1, 0, 'none', 0);
