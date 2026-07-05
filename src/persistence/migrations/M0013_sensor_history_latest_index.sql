-- M0013_sensor_history_latest_index.sql
--
-- Covering index for the "latest reading per (source_id, key)" query shape.
-- soil_channels() and latest_for_source() resolve the newest row per channel;
-- without this index their per-group MAX(epoch) lookups had no covering path,
-- so on a mature keep-forever install (2M rows, 362k soil rows measured on
-- prod) soil_channels took ~15s while HOLDING the shared connection lock,
-- which serialized every sensors/devices endpoint behind it ("endless
-- loading" on the Sensors panes). With this index plus the group-max rewrite
-- of those queries, the same prod data answers in ~137ms.
-- Build cost: ~1s on the 2M-row table, one-time at boot.
CREATE INDEX IF NOT EXISTS idx_sh_source_key_epoch
    ON sensor_history(source_id, key, epoch DESC);
