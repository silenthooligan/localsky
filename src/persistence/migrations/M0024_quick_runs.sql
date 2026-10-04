-- Manual queue metadata is command intent, never proof of applied water.
-- Requests survive a response loss; queues never resume after process restart.
CREATE TABLE quick_runs (
    id TEXT PRIMARY KEY,
    request_id TEXT NOT NULL UNIQUE,
    started_epoch INTEGER NOT NULL,
    status_json TEXT NOT NULL
);
