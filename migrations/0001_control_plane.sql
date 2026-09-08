PRAGMA foreign_keys = ON;

CREATE TABLE instance_metadata (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    instance_id TEXT NOT NULL CHECK (
        length(instance_id) BETWEEN 1 AND 64 AND
        instance_id NOT GLOB '*[^a-z0-9-]*' AND
        substr(instance_id, 1, 1) GLOB '[a-z0-9]' AND
        substr(instance_id, -1, 1) GLOB '[a-z0-9]'
    ),
    schema_version INTEGER NOT NULL CHECK (schema_version > 0),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms > 0)
);

CREATE TABLE applications (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    desired_json TEXT NOT NULL CHECK (json_valid(desired_json)),
    resolved_json TEXT NOT NULL CHECK (json_valid(resolved_json)),
    delete_intent INTEGER NOT NULL DEFAULT 0 CHECK (delete_intent IN (0, 1)),
    deleted_at_ms INTEGER,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);
CREATE UNIQUE INDEX application_live_name ON applications(name) WHERE deleted_at_ms IS NULL;

CREATE TABLE application_status (
    application_id TEXT PRIMARY KEY REFERENCES applications(id) ON DELETE CASCADE,
    state TEXT NOT NULL CHECK (state IN ('pending','deploying','ready','degraded','deleting','failed')),
    message TEXT,
    updated_at_ms INTEGER NOT NULL
);

CREATE TABLE operations (
    id TEXT PRIMARY KEY,
    application_id TEXT NOT NULL REFERENCES applications(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('apply','delete')),
    state TEXT NOT NULL CHECK (state IN ('requested','running','succeeded','failed','cancelled')),
    error_code TEXT,
    error_message TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    started_at_ms INTEGER,
    finished_at_ms INTEGER,
    CHECK ((error_code IS NULL) = (error_message IS NULL)),
    CHECK ((state IN ('succeeded','failed','cancelled')) = (finished_at_ms IS NOT NULL))
);

CREATE INDEX operation_application ON operations(application_id,created_at_ms DESC,id DESC);
CREATE UNIQUE INDEX operation_running ON operations(application_id) WHERE state='running';
CREATE INDEX operation_finished ON operations(finished_at_ms) WHERE finished_at_ms IS NOT NULL;
