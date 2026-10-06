-- One durable daily-outlook claim, retained across restarts and config saves.
CREATE TABLE notification_delivery (
    kind TEXT PRIMARY KEY,
    date_local TEXT NOT NULL,
    claimed_at_epoch INTEGER NOT NULL
);
