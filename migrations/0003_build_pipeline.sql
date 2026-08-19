PRAGMA foreign_keys = ON;

CREATE TABLE builds (
    id TEXT PRIMARY KEY,
    operation_id TEXT NOT NULL REFERENCES operations(id) ON DELETE CASCADE,
    application_id TEXT NOT NULL REFERENCES applications(id) ON DELETE CASCADE,
    service_name TEXT NOT NULL CHECK (
        length(service_name) BETWEEN 1 AND 63 AND
        service_name NOT GLOB '*[^a-z0-9-]*' AND
        substr(service_name, 1, 1) GLOB '[a-z]' AND
        substr(service_name, -1, 1) GLOB '[a-z0-9]'
    ),
    state TEXT NOT NULL CHECK (state IN ('pending','running','recovery','succeeded','failed','cancelled')),
    source_commit TEXT CHECK (source_commit IS NULL OR (length(source_commit) = 40 AND source_commit NOT GLOB '*[^0-9a-fA-F]*')),
    image_reference TEXT CHECK (image_reference IS NULL OR length(image_reference) BETWEEN 1 AND 512),
    image_digest TEXT CHECK (image_digest IS NULL OR length(image_digest) BETWEEN 1 AND 512),
    error_code TEXT CHECK (error_code IS NULL OR length(error_code) BETWEEN 1 AND 64),
    error_message TEXT CHECK (error_message IS NULL OR length(error_message) BETWEEN 1 AND 2048),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms > 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= created_at_ms),
    started_at_ms INTEGER,
    finished_at_ms INTEGER,
    CHECK ((error_code IS NULL) = (error_message IS NULL)),
    CHECK (error_code IS NULL OR state = 'failed'),
    CHECK ((state IN ('succeeded','failed','cancelled') AND finished_at_ms IS NOT NULL) OR
           (state NOT IN ('succeeded','failed','cancelled') AND finished_at_ms IS NULL)),
    UNIQUE(operation_id, service_name)
);

CREATE INDEX builds_application_idx ON builds(application_id, created_at_ms DESC);

CREATE TABLE build_artifacts (
    build_id TEXT PRIMARY KEY REFERENCES builds(id) ON DELETE CASCADE,
    build_key TEXT NOT NULL CHECK (length(build_key) = 71 AND build_key LIKE 'sha256:%'),
    context_hash TEXT NOT NULL CHECK (length(context_hash) = 71 AND context_hash LIKE 'sha256:%'),
    verified INTEGER NOT NULL DEFAULT 0 CHECK (verified IN (0, 1)),
    verified_at_ms INTEGER,
    CHECK ((verified = 1 AND verified_at_ms IS NOT NULL) OR
           (verified = 0 AND verified_at_ms IS NULL))
);

CREATE INDEX build_artifacts_verified_cache_idx
    ON build_artifacts(build_key) WHERE verified = 1;

CREATE TABLE build_logs (
    build_id TEXT NOT NULL REFERENCES builds(id) ON DELETE CASCADE,
    sequence INTEGER NOT NULL CHECK (sequence > 0),
    timestamp_ms INTEGER NOT NULL CHECK (timestamp_ms > 0),
    message TEXT NOT NULL CHECK (length(message) BETWEEN 1 AND 16384),
    PRIMARY KEY(build_id, sequence)
);

CREATE INDEX build_logs_stream_idx ON build_logs(build_id, sequence);

UPDATE instance_metadata SET schema_version = 3 WHERE singleton = 1;
