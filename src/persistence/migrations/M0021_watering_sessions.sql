-- Session identity is provenance, never an estimate from time proximity.
-- Old rows stay NULL: their original job identity cannot be recovered.
ALTER TABLE runs ADD COLUMN session_id TEXT;
CREATE TABLE watering_commands (
    id INTEGER PRIMARY KEY,
    session_id TEXT NOT NULL,
    zone_slug TEXT NOT NULL,
    controller_id TEXT NOT NULL,
    start_epoch INTEGER NOT NULL,
    end_epoch INTEGER NOT NULL,
    cycle_index INTEGER,
    cycle_count INTEGER,
    state TEXT NOT NULL CHECK (state IN ('requested', 'confirmed', 'failed'))
);
CREATE INDEX watering_commands_observation
    ON watering_commands(zone_slug, controller_id, start_epoch, end_epoch);
