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

CREATE TABLE logical_secrets (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE CHECK (
        length(name) BETWEEN 1 AND 63 AND
        name NOT GLOB '*[^a-z0-9-]*' AND
        substr(name, 1, 1) GLOB '[a-z]' AND
        substr(name, -1, 1) GLOB '[a-z0-9]'
    ),
    generation INTEGER NOT NULL CHECK (generation > 0),
    encryption_algorithm TEXT,
    encryption_key_id TEXT,
    nonce BLOB,
    ciphertext BLOB,
    swarm_secret_name TEXT,
    value_is_set INTEGER NOT NULL DEFAULT 0 CHECK (value_is_set IN (0, 1)),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms > 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= created_at_ms),
    CHECK ((value_is_set = 0 AND ciphertext IS NULL AND nonce IS NULL) OR
           (value_is_set = 1 AND ciphertext IS NOT NULL AND nonce IS NOT NULL AND
            encryption_algorithm IS NOT NULL AND encryption_key_id IS NOT NULL)),
    CHECK (value_is_set = 1 OR
           (encryption_algorithm IS NULL AND encryption_key_id IS NULL AND
            swarm_secret_name IS NULL))
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
    resolved_json TEXT CHECK (resolved_json IS NULL OR json_valid(resolved_json)),
    spec_hash TEXT NOT NULL CHECK (length(spec_hash) = 71 AND spec_hash LIKE 'sha256:%'),
    delete_intent INTEGER NOT NULL DEFAULT 0 CHECK (delete_intent IN (0, 1)),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms > 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= created_at_ms)
);

CREATE TABLE application_status (
    application_id TEXT PRIMARY KEY REFERENCES applications(id) ON DELETE CASCADE,
    state TEXT NOT NULL CHECK (state IN ('pending','resolving','building','deploying','ready','degraded','deleting','failed')),
    observed_generation INTEGER CHECK (observed_generation IS NULL OR observed_generation > 0),
    message TEXT CHECK (message IS NULL OR length(message) BETWEEN 1 AND 2048),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms > 0),
    CHECK (observed_generation IS NULL OR state IN ('ready','degraded','failed'))
);

CREATE TABLE operations (
    id TEXT PRIMARY KEY,
    application_id TEXT NOT NULL REFERENCES applications(id) ON DELETE CASCADE,
    generation INTEGER NOT NULL CHECK (generation > 0),
    kind TEXT NOT NULL CHECK (kind IN ('create','replace','delete','reconcile','build','deploy')),
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

CREATE TABLE builds (
    id TEXT PRIMARY KEY,
    operation_id TEXT NOT NULL,
    application_id TEXT NOT NULL REFERENCES applications(id) ON DELETE CASCADE,
    service_name TEXT NOT NULL CHECK (
        length(service_name) BETWEEN 1 AND 63 AND
        service_name NOT GLOB '*[^a-z0-9-]*' AND
        substr(service_name, 1, 1) GLOB '[a-z]' AND
        substr(service_name, -1, 1) GLOB '[a-z0-9]'
    ),
    state TEXT NOT NULL CHECK (state IN ('pending','running','recovery','succeeded','failed','cancelled')),
    source_commit TEXT,
    image_reference TEXT,
    image_digest TEXT,
    error_code TEXT CHECK (error_code IS NULL OR length(error_code) BETWEEN 1 AND 64),
    error_message TEXT CHECK (error_message IS NULL OR length(error_message) BETWEEN 1 AND 2048),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms > 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= created_at_ms),
    started_at_ms INTEGER,
    finished_at_ms INTEGER,
    UNIQUE(operation_id, service_name),
    FOREIGN KEY(operation_id, application_id)
        REFERENCES operations(id, application_id) ON DELETE CASCADE,
    CHECK ((state IN ('succeeded','failed','cancelled') AND finished_at_ms IS NOT NULL) OR
           (state NOT IN ('succeeded','failed','cancelled') AND finished_at_ms IS NULL)),
    CHECK ((state = 'running' AND started_at_ms IS NOT NULL) OR state != 'running'),
    CHECK (started_at_ms IS NULL OR started_at_ms >= created_at_ms),
    CHECK (finished_at_ms IS NULL OR finished_at_ms >= created_at_ms),
    CHECK ((error_code IS NULL) = (error_message IS NULL)),
    CHECK (error_code IS NULL OR state = 'failed'),
    CHECK (state != 'succeeded' OR
           (source_commit IS NOT NULL AND image_reference IS NOT NULL AND image_digest IS NOT NULL))
);

CREATE INDEX applications_delete_intent_idx ON applications(delete_intent, updated_at_ms);
CREATE INDEX operations_dispatch_idx ON operations(state, created_at_ms);
CREATE INDEX operations_application_idx ON operations(application_id, created_at_ms DESC);
CREATE UNIQUE INDEX operations_one_running_per_app_idx ON operations(application_id)
    WHERE state = 'running';
CREATE INDEX operation_steps_dispatch_idx ON operation_steps(operation_id, state, position);
CREATE INDEX builds_dispatch_idx ON builds(state, created_at_ms);
CREATE INDEX builds_application_idx ON builds(application_id, created_at_ms DESC);
