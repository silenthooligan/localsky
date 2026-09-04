-- M0018: forecast_observations.et0_mm + et0_source
--
-- Per-day reference ET0 (mm) beside the day's rain pair, making the
-- ledger the durable per-day evapotranspiration record the soil
-- scheduling replay reads. Nullable on purpose: rows written before
-- this column, and days where no writer resolved an ET0, carry NULL
-- (no evidence). A day with no evidence is charged from the zone's
-- weekly-target-derived daily mean at replay time, never from a
-- fabricated constant.
--
-- et0_source tags the writer that supplied the day's max value, the
-- same provenance-follows-the-value pattern as observed_source. The
-- refresher self-emits its resolved daily figure under
-- 'localsky_engine' once that wiring lands; station or provider
-- writers use their own tags.

ALTER TABLE forecast_observations ADD COLUMN et0_mm REAL;
ALTER TABLE forecast_observations ADD COLUMN et0_source TEXT;
