PRAGMA foreign_keys = ON;

CREATE TABLE instance_metadata (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    instance_id TEXT NOT NULL CHECK (
        length(instance_id) BETWEEN 8 AND 64 AND
        instance_id NOT GLOB '*[^a-z0-9-]*' AND
        substr(instance_id, 1, 1) GLOB '[a-z0-9]' AND
        substr(instance_id, -1, 1) GLOB '[a-z0-9]'
    ),
    schema_version INTEGER NOT NULL CHECK (schema_version > 0),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms > 0)
);

CREATE TABLE applications (
    id TEXT PRIMARY KEY CHECK (
        length(id) BETWEEN 8 AND 64 AND
        id NOT GLOB '*[^a-z0-9-]*' AND
        substr(id, 1, 1) GLOB '[a-z0-9]' AND
        substr(id, -1, 1) GLOB '[a-z0-9]'
    ),
    name TEXT NOT NULL UNIQUE,
    generation INTEGER NOT NULL CHECK (generation > 0),
    desired_json TEXT NOT NULL CHECK (json_valid(desired_json)),
    resolved_json TEXT NOT NULL CHECK (json_valid(resolved_json)),
    spec_hash TEXT NOT NULL CHECK (length(spec_hash) = 71 AND spec_hash LIKE 'sha256:%'),
    delete_intent INTEGER NOT NULL DEFAULT 0 CHECK (delete_intent IN (0, 1)),
    deleted_at_ms INTEGER CHECK (deleted_at_ms IS NULL OR deleted_at_ms >= created_at_ms),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms > 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= created_at_ms)
);

CREATE TABLE application_status (
    application_id TEXT PRIMARY KEY REFERENCES applications(id) ON DELETE CASCADE,
    state TEXT NOT NULL CHECK (state IN ('pending','deploying','ready','degraded','deleting','failed')),
    observed_generation INTEGER CHECK (observed_generation IS NULL OR observed_generation > 0),
    message TEXT CHECK (message IS NULL OR length(message) BETWEEN 1 AND 2048),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms > 0),
    CHECK (observed_generation IS NULL OR state IN ('ready','degraded','failed'))
);

CREATE TABLE operations (
    id TEXT PRIMARY KEY,
    application_id TEXT NOT NULL REFERENCES applications(id) ON DELETE CASCADE,
    generation INTEGER NOT NULL CHECK (generation > 0),
    kind TEXT NOT NULL CHECK (kind IN ('create','replace','delete','reconcile')),
    state TEXT NOT NULL CHECK (state IN ('pending','running','recovery','succeeded','failed','cancelled')),
    error_code TEXT CHECK (error_code IS NULL OR length(error_code) BETWEEN 1 AND 64),
    error_message TEXT CHECK (error_message IS NULL OR length(error_message) BETWEEN 1 AND 2048),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms > 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= created_at_ms),
    started_at_ms INTEGER,
    finished_at_ms INTEGER,
    UNIQUE(id, application_id),
    CHECK ((state IN ('succeeded','failed','cancelled') AND finished_at_ms IS NOT NULL) OR
           (state NOT IN ('succeeded','failed','cancelled') AND finished_at_ms IS NULL)),
    CHECK ((state = 'running' AND started_at_ms IS NOT NULL) OR state != 'running'),
    CHECK (started_at_ms IS NULL OR started_at_ms >= created_at_ms),
    CHECK (finished_at_ms IS NULL OR finished_at_ms >= created_at_ms),
    CHECK ((error_code IS NULL) = (error_message IS NULL)),
    CHECK (error_code IS NULL OR state = 'failed')
);

CREATE TABLE operation_steps (
    id TEXT PRIMARY KEY,
    operation_id TEXT NOT NULL REFERENCES operations(id) ON DELETE CASCADE,
    position INTEGER NOT NULL CHECK (position >= 0),
    action TEXT NOT NULL CHECK (length(action) BETWEEN 1 AND 64),
    state TEXT NOT NULL CHECK (state IN ('pending','running','recovery','succeeded','failed','cancelled','skipped')),
    attempt INTEGER NOT NULL DEFAULT 0 CHECK (attempt >= 0),
    error_code TEXT CHECK (error_code IS NULL OR length(error_code) BETWEEN 1 AND 64),
    error_message TEXT CHECK (error_message IS NULL OR length(error_message) BETWEEN 1 AND 2048),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms > 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= created_at_ms),
    started_at_ms INTEGER,
    finished_at_ms INTEGER,
    UNIQUE(operation_id, position),
    CHECK ((state IN ('succeeded','failed','cancelled','skipped') AND finished_at_ms IS NOT NULL) OR
           (state NOT IN ('succeeded','failed','cancelled','skipped') AND finished_at_ms IS NULL)),
    CHECK ((state = 'running' AND started_at_ms IS NOT NULL) OR state != 'running'),
    CHECK (started_at_ms IS NULL OR started_at_ms >= created_at_ms),
    CHECK (finished_at_ms IS NULL OR finished_at_ms >= created_at_ms),
    CHECK ((error_code IS NULL) = (error_message IS NULL)),
    CHECK (error_code IS NULL OR state = 'failed')
);

CREATE TABLE mutation_idempotency (
    key_hash TEXT PRIMARY KEY CHECK (length(key_hash) = 71 AND key_hash LIKE 'sha256:%'),
    request_hash TEXT NOT NULL CHECK (length(request_hash) = 71 AND request_hash LIKE 'sha256:%'),
    application_id TEXT NOT NULL REFERENCES applications(id),
    operation_id TEXT NOT NULL REFERENCES operations(id) ON DELETE CASCADE,
    generation INTEGER NOT NULL CHECK (generation > 0),
    kind TEXT NOT NULL CHECK (kind IN ('create','replace','delete','reconcile')),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms > 0)
);

CREATE INDEX applications_delete_intent_idx ON applications(delete_intent, updated_at_ms);
CREATE INDEX applications_live_idx ON applications(deleted_at_ms, name);
CREATE INDEX operations_dispatch_idx ON operations(state, created_at_ms);
CREATE INDEX operations_application_idx ON operations(application_id, created_at_ms DESC);
CREATE UNIQUE INDEX operations_one_running_per_app_idx ON operations(application_id)
    WHERE state = 'running';
CREATE INDEX operations_finished_retention_idx ON operations(finished_at_ms)
    WHERE finished_at_ms IS NOT NULL;
CREATE INDEX operation_steps_dispatch_idx ON operation_steps(operation_id, state, position);
CREATE INDEX mutation_idempotency_application_idx
    ON mutation_idempotency(application_id);
