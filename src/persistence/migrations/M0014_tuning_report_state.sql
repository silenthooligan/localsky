-- M0014: tuning_report_state
--
-- Weekly tuning-report notification dedupe. The scheduler emits the
-- "your tuning report is ready" push at most once per 7 local days; the
-- last-notified stamp must survive container restarts (GitOps redeploys
-- restart the stack on every push, and every in-memory push dedupe is
-- re-armed by a restart; see the M0008 header for the same reasoning).
-- Singleton row (id = 1, enforced by CHECK), UPSERT setters, safe-default
-- reads: the irrigation_control pattern.

CREATE TABLE IF NOT EXISTS tuning_report_state (
    id                  INTEGER PRIMARY KEY CHECK (id = 1),
    last_notified_epoch INTEGER NOT NULL DEFAULT 0, -- UTC epoch; 0 = never
    updated_at_epoch    INTEGER NOT NULL DEFAULT 0
);

INSERT OR IGNORE INTO tuning_report_state
    (id, last_notified_epoch, updated_at_epoch)
    VALUES (1, 0, 0);
