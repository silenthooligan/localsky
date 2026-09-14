-- A morning with no valve command is still an irrigation decision.
CREATE TABLE daily_irrigation (
    date_local TEXT PRIMARY KEY NOT NULL,
    epoch INTEGER NOT NULL,
    decision_json TEXT NOT NULL
);
CREATE INDEX daily_irrigation_epoch ON daily_irrigation(epoch);
