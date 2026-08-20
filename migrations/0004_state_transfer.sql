PRAGMA foreign_keys = ON;

CREATE TABLE state_transfer_operations (
    id TEXT PRIMARY KEY CHECK (length(id) BETWEEN 8 AND 128),
    direction TEXT NOT NULL CHECK (direction IN ('export','import')),
    mode TEXT NOT NULL CHECK (mode IN ('portable','encrypted','unknown')),
    state TEXT NOT NULL CHECK (state IN ('running','succeeded','failed')),
    archive_digest TEXT CHECK (
        archive_digest IS NULL OR
        (length(archive_digest) = 71 AND archive_digest LIKE 'sha256:%')
    ),
    source_instance_id TEXT,
    diagnostic_code TEXT CHECK (diagnostic_code IS NULL OR length(diagnostic_code) BETWEEN 1 AND 64),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms > 0),
    finished_at_ms INTEGER,
    CHECK ((state = 'running' AND finished_at_ms IS NULL) OR
           (state != 'running' AND finished_at_ms IS NOT NULL))
);

CREATE INDEX state_transfer_operations_created_idx
    ON state_transfer_operations(created_at_ms DESC);

UPDATE instance_metadata SET schema_version = 4 WHERE singleton = 1;
