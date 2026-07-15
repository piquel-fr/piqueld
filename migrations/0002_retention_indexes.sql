-- Forward-only migration kept separate to exercise ordered migration upgrades.
CREATE INDEX operations_finished_retention_idx ON operations(finished_at_ms)
    WHERE finished_at_ms IS NOT NULL;
CREATE INDEX builds_finished_retention_idx ON builds(finished_at_ms)
    WHERE finished_at_ms IS NOT NULL;
UPDATE instance_metadata SET schema_version = 2 WHERE singleton = 1;
