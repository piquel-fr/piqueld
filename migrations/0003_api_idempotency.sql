CREATE TABLE application_create_idempotency (
    key_hash TEXT PRIMARY KEY CHECK (length(key_hash) = 71 AND key_hash LIKE 'sha256:%'),
    request_hash TEXT NOT NULL CHECK (length(request_hash) = 71 AND request_hash LIKE 'sha256:%'),
    application_id TEXT NOT NULL REFERENCES applications(id) CHECK (length(application_id) BETWEEN 8 AND 64),
    operation_id TEXT NOT NULL REFERENCES operations(id) CHECK (length(operation_id) BETWEEN 8 AND 128),
    generation INTEGER NOT NULL CHECK (generation = 1),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms > 0)
);

CREATE INDEX application_create_idempotency_application_idx
    ON application_create_idempotency(application_id);

UPDATE instance_metadata SET schema_version = 3 WHERE singleton = 1;
