-- M0012: active-run safety ledger for the deadline reaper (P0-1b).
--
-- A commanded-valve ledger, deliberately SEPARATE from the `runs` history table
-- (which records what happened, via the run-edge observer). One row per zone
-- currently commanded ON, carrying the wall-clock epoch by which the valve MUST
-- be closed. A reaper enforces these deadlines independent of any controller's
-- own shutoff: the MQTT adapter's shutoff is an in-process timer that dies with
-- the process, so without a persisted deadline a crash strands a valve open. With
-- this ledger no valve is left open past its deadline regardless of process state.
--
-- Lifecycle: armed on a successful run_zone (off_deadline = start + duration);
-- disarmed on an explicit stop, on a successful reap, and cleared wholesale at
-- boot AFTER reconcile_stop_all has physically closed every valve.
--
-- zone_slug is the primary key: a zone cannot run twice at once (the dispatch
-- path also serializes Run per zone), so a new run REPLACEs any stale ledger row.
CREATE TABLE IF NOT EXISTS active_runs (
    zone_slug          TEXT    PRIMARY KEY,
    controller_id      TEXT    NOT NULL,
    started_epoch      INTEGER NOT NULL,
    off_deadline_epoch INTEGER NOT NULL
);

CREATE INDEX IF NOT EXISTS active_runs_deadline ON active_runs(off_deadline_epoch);
