-- Preserve each issuance: a later fetch must not rewrite the forecast an
-- earlier watering decision could have seen. Lead time remains queryable.
CREATE TABLE forecast_hourly_archive (
    track TEXT NOT NULL,
    provider TEXT NOT NULL,
    model TEXT,
    target_epoch INTEGER NOT NULL,
    lead_h INTEGER NOT NULL CHECK (lead_h BETWEEN 0 AND 47),
    pop_pct INTEGER CHECK (pop_pct BETWEEN 0 AND 100),
    precip_in REAL CHECK (precip_in >= 0),
    fetched_at INTEGER NOT NULL,
    PRIMARY KEY (track, target_epoch, fetched_at)
) WITHOUT ROWID;
CREATE INDEX forecast_archive_expiry ON forecast_hourly_archive(target_epoch);
