ALTER TABLE push_subscriptions ADD COLUMN preferences TEXT NOT NULL DEFAULT '{}';
ALTER TABLE push_subscriptions ADD COLUMN last_outlook_day TEXT;

CREATE TABLE notification_conditions (
    kind TEXT PRIMARY KEY,
    active INTEGER NOT NULL,
    last_fired_epoch INTEGER NOT NULL,
    observed_epoch INTEGER NOT NULL
);
