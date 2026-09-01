-- M0017: the pause and dry-run toggles, and a day stamp on the one-day
-- override.
--
-- 0.7.22 retires the Home Assistant helper reads. Two of the four operator
-- controls had no native column at all: input_boolean.irrigation_pause and
-- input_boolean.irrigation_dry_run were read straight from the entity map on
-- both deployment paths, which meant they were permanently false on a
-- standalone install (the map is empty there by construction). Both gates are
-- PROTECTED in the skip-rule ladder, so a protected control with nowhere to
-- store its state is a control that does not exist. These two columns are
-- where they live now.
--
-- override_tomorrow_day is the reset the one-day override never had natively.
-- In Home Assistant mode a midnight automation set the input_select back to
-- "none" each night; this migration makes that automation irrelevant, so
-- without a stamp an adopted "skip" would freeze tomorrow's cell forever. The
-- store returns override_tomorrow only while the stamp matches the current
-- local date, so the value expires at midnight the way it always did.

ALTER TABLE irrigation_control ADD COLUMN is_paused  INTEGER NOT NULL DEFAULT 0;
ALTER TABLE irrigation_control ADD COLUMN is_dry_run INTEGER NOT NULL DEFAULT 0;
ALTER TABLE irrigation_control ADD COLUMN override_tomorrow_day TEXT NOT NULL DEFAULT '';

-- No backfill. The stamp is written only by `set_override_tomorrow`, which
-- uses the CONFIGURED deployment timezone; SQLite's 'localtime' resolves the
-- container's TZ, which is UTC in the shipped image, and would disagree with
-- the reader in both directions: expiring an override the moment the column
-- appears on a US install upgraded in the evening, or keeping it a day too
-- long on the far side of the date line. An override that predates this
-- migration therefore reads 'none' until it is set again, which is the
-- direction that cannot water a yard the owner asked to skip.
