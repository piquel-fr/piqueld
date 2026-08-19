ALTER TABLE applications
    ADD COLUMN deleted_at_ms INTEGER
    CHECK (deleted_at_ms IS NULL OR deleted_at_ms >= created_at_ms);

CREATE INDEX applications_live_idx
    ON applications(deleted_at_ms, name);

UPDATE instance_metadata SET schema_version = 4 WHERE singleton = 1;
