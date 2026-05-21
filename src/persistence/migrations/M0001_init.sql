-- M0001_init.sql
-- Bootstrap migration. Creates the schema_migrations registry that
-- subsequent migrations record themselves in.
--
-- For existing v0.1 databases (which were never under the migration
-- runner), the runner's first pass detects the legacy `runs` and
-- `push_subscriptions` tables and back-fills schema_migrations with
-- the migration versions that "would have" produced them. See
-- src/persistence/runner.rs::backfill_legacy.

CREATE TABLE IF NOT EXISTS schema_migrations (
    version    TEXT PRIMARY KEY NOT NULL,
    name       TEXT NOT NULL,
    applied_at INTEGER NOT NULL
);
