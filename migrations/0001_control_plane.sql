PRAGMA foreign_keys = ON;

CREATE TABLE instance_metadata (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    instance_id TEXT NOT NULL CHECK (length(instance_id) BETWEEN 8 AND 64),
    schema_version INTEGER NOT NULL CHECK (schema_version > 0),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms > 0)
);

CREATE TABLE logical_secrets (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    generation INTEGER NOT NULL CHECK (generation > 0),
    encryption_algorithm TEXT,
    encryption_key_id TEXT,
    nonce BLOB,
    ciphertext BLOB,
    swarm_secret_name TEXT,
    value_is_set INTEGER NOT NULL DEFAULT 0 CHECK (value_is_set IN (0, 1)),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    CHECK ((value_is_set = 0 AND ciphertext IS NULL AND nonce IS NULL) OR
           (value_is_set = 1 AND ciphertext IS NOT NULL AND nonce IS NOT NULL AND
            encryption_algorithm IS NOT NULL AND encryption_key_id IS NOT NULL))
);

CREATE TABLE applications (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    generation INTEGER NOT NULL CHECK (generation > 0),
    desired_json TEXT NOT NULL,
    resolved_json TEXT,
    spec_hash TEXT NOT NULL CHECK (spec_hash LIKE 'sha256:%'),
    delete_intent INTEGER NOT NULL DEFAULT 0 CHECK (delete_intent IN (0, 1)),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);

CREATE TABLE application_status (
    application_id TEXT PRIMARY KEY REFERENCES applications(id) ON DELETE CASCADE,
    state TEXT NOT NULL CHECK (state IN ('pending','resolving','building','deploying','ready','degraded','deleting','failed')),
    observed_generation INTEGER CHECK (observed_generation IS NULL OR observed_generation > 0),
    message TEXT,
    updated_at_ms INTEGER NOT NULL
);

CREATE TABLE operations (
    id TEXT PRIMARY KEY,
    application_id TEXT NOT NULL REFERENCES applications(id) ON DELETE CASCADE,
    generation INTEGER NOT NULL CHECK (generation > 0),
    kind TEXT NOT NULL CHECK (kind IN ('create','replace','delete','reconcile','build','deploy')),
    state TEXT NOT NULL CHECK (state IN ('pending','running','recovery','succeeded','failed','cancelled')),
    error_code TEXT,
    error_message TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    started_at_ms INTEGER,
    finished_at_ms INTEGER,
    CHECK ((state IN ('succeeded','failed','cancelled') AND finished_at_ms IS NOT NULL) OR
           (state NOT IN ('succeeded','failed','cancelled') AND finished_at_ms IS NULL))
);

CREATE TABLE operation_steps (
    id TEXT PRIMARY KEY,
    operation_id TEXT NOT NULL REFERENCES operations(id) ON DELETE CASCADE,
    position INTEGER NOT NULL CHECK (position >= 0),
    kind TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('pending','running','recovery','succeeded','failed','cancelled','skipped')),
    attempt INTEGER NOT NULL DEFAULT 0 CHECK (attempt >= 0),
    error_code TEXT,
    error_message TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    started_at_ms INTEGER,
    finished_at_ms INTEGER,
    UNIQUE(operation_id, position),
    CHECK ((state IN ('succeeded','failed','cancelled','skipped') AND finished_at_ms IS NOT NULL) OR
           (state NOT IN ('succeeded','failed','cancelled','skipped') AND finished_at_ms IS NULL))
);

CREATE TABLE builds (
    id TEXT PRIMARY KEY,
    operation_id TEXT NOT NULL REFERENCES operations(id) ON DELETE CASCADE,
    application_id TEXT NOT NULL REFERENCES applications(id) ON DELETE CASCADE,
    service_name TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('pending','running','recovery','succeeded','failed','cancelled')),
    source_commit TEXT,
    image_reference TEXT,
    image_digest TEXT,
    error_code TEXT,
    error_message TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    started_at_ms INTEGER,
    finished_at_ms INTEGER,
    UNIQUE(operation_id, service_name)
);

CREATE INDEX applications_delete_intent_idx ON applications(delete_intent, updated_at_ms);
CREATE INDEX operations_dispatch_idx ON operations(state, created_at_ms);
CREATE INDEX operations_application_idx ON operations(application_id, created_at_ms DESC);
CREATE INDEX operation_steps_dispatch_idx ON operation_steps(operation_id, state, position);
CREATE INDEX builds_dispatch_idx ON builds(state, created_at_ms);
CREATE INDEX builds_application_idx ON builds(application_id, created_at_ms DESC);
