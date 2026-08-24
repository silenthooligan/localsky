-- M0015: forecast_observations.observed_source
--
-- Per-day provenance for the observed rain value: which kind of source
-- supplied the day's total. The writer stamps the day-max writer's
-- nature; a day with NO rain-capable source writes observed 0.0 with
-- source 'none' so fabricated dry days are distinguishable from
-- measured-dry days (and are excluded from the bias-model fit and the
-- dryness counters).
--
-- Values: 'gauge' | 'radar' | 'none' | 'legacy' (plus 'model', reserved:
-- the writer records the 'none' placeholder for model-nature owners,
-- since a model's rain-today is a whole-day forecast, not an
-- observation, and the day-max semantics would make it permanent).
-- 'legacy' marks rows written before this column existed: their source
-- label was lost at write time. Consumers treat legacy rows as
-- gauge-quality only when the install has a station source, else as
-- model-quality.

ALTER TABLE forecast_observations
    ADD COLUMN observed_source TEXT NOT NULL DEFAULT 'legacy';
