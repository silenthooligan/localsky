-- M0019: runs.note + runs.volume_gal
--
-- note: why a row ended the way it did when that is not a skip. An
-- aborted run carries "ended by restart" or "stopped by the reaper at
-- its deadline" here rather than in skip_reason, so the water it
-- applied still counts as watering and the audit trail still says what
-- happened. NULL on rows written before this column and on ordinary
-- completed runs.
--
-- volume_gal: the run's metered volume when the controller has a flow
-- meter, integrated from the sampled flow rate across the run. NULL
-- without a meter; a run with no volume is not a run with zero water.
ALTER TABLE runs ADD COLUMN note TEXT;
ALTER TABLE runs ADD COLUMN volume_gal REAL;
