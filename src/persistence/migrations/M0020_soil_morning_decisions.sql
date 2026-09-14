-- Actual per-zone decisions at the scheduled morning. Ordinary dry days and
-- operator/restriction holds are not forecast deferrals. The first decision
-- for a zone/day survives refreshes, catch-up attempts and process restarts.
CREATE TABLE soil_morning_decisions (
    date_local TEXT NOT NULL,
    zone_slug TEXT NOT NULL,
    epoch INTEGER NOT NULL,
    outcome TEXT NOT NULL CHECK (outcome IN ('forecast_rain', 'other_hold', 'not_due')),
    reason_code TEXT NOT NULL,
    PRIMARY KEY (date_local, zone_slug)
);
