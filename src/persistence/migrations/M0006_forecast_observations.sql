-- M0006: forecast_observations
--
-- One row per local calendar day, recording the rain the forecast
-- predicted vs. what the station actually observed. Fed by the daily
-- ingest in the irrigation refresher (forecast.past_daily[0] for the
-- prediction we made yesterday, snapshot's rain_today_in for the
-- observation). Consumed by engine::forecast_bias::BiasModel to
-- compute a per-month-of-year multiplier applied before the skip
-- rules evaluate.
--
-- date is the primary key so the daily ingest is naturally idempotent:
-- INSERT OR REPLACE on the same date overwrites a partial-day reading
-- once the end-of-day total is known.

CREATE TABLE IF NOT EXISTS forecast_observations (
    date              TEXT    PRIMARY KEY NOT NULL,  -- YYYY-MM-DD local
    predicted_in      REAL    NOT NULL,
    observed_in       REAL    NOT NULL,
    month             INTEGER NOT NULL,              -- 1..12, denormalized for index
    inserted_at_epoch INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS idx_forecast_observations_month
    ON forecast_observations(month);
