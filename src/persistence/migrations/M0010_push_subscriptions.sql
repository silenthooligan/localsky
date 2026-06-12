-- Web Push subscription store. The v1 codebase created this table inline
-- at subscribe time; the v2 pivot dropped that bootstrap without a
-- replacement migration, so fresh databases had no table and both
-- subscribe and dispatch failed ("no such table: push_subscriptions").
-- Shape matches the v1 table byte-for-byte. IF NOT EXISTS keeps legacy
-- databases, which already carry it, applying cleanly.
CREATE TABLE IF NOT EXISTS push_subscriptions (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    endpoint    TEXT    NOT NULL UNIQUE,
    p256dh      TEXT    NOT NULL,
    auth        TEXT    NOT NULL,
    created_at  INTEGER NOT NULL,
    last_seen   INTEGER NOT NULL
);
