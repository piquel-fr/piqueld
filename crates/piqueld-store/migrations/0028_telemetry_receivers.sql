-- Environments granted the daemon's telemetry. Their receiving service, if
-- they deploy one, gets every application's telemetry over a socket in
-- `<data_dir>/telemetry/<environment-id>/`. Previews are never granted it.
ALTER TABLE environments ADD COLUMN telemetry INTEGER NOT NULL DEFAULT 0
    CHECK (telemetry IN (0, 1) AND (telemetry = 0 OR kind = 'environment'));
