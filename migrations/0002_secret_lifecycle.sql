PRAGMA foreign_keys = ON;

ALTER TABLE applications ADD COLUMN deployed_json TEXT
    CHECK (deployed_json IS NULL OR json_valid(deployed_json));

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
    content_hash TEXT CHECK (
        content_hash IS NULL OR
        (length(content_hash) = 71 AND content_hash LIKE 'sha256:%')
    ),
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms > 0),
    updated_at_ms INTEGER NOT NULL CHECK (updated_at_ms >= created_at_ms),
    CHECK ((value_is_set = 0 AND ciphertext IS NULL AND nonce IS NULL AND
            content_hash IS NULL) OR
           (value_is_set = 1 AND ciphertext IS NOT NULL AND nonce IS NOT NULL AND
            encryption_algorithm IS NOT NULL AND encryption_key_id IS NOT NULL AND
            swarm_secret_name IS NOT NULL AND content_hash IS NOT NULL)),
    CHECK (value_is_set = 1 OR
           (encryption_algorithm IS NULL AND encryption_key_id IS NULL AND
            swarm_secret_name IS NULL))
);

CREATE TABLE secret_generations (
    secret_id TEXT NOT NULL REFERENCES logical_secrets(id) ON DELETE CASCADE,
    generation INTEGER NOT NULL CHECK (generation > 0),
    encryption_algorithm TEXT NOT NULL,
    encryption_key_id TEXT NOT NULL,
    nonce BLOB NOT NULL CHECK (length(nonce) = 24),
    ciphertext BLOB NOT NULL,
    content_hash TEXT NOT NULL CHECK (
        length(content_hash) = 71 AND content_hash LIKE 'sha256:%'
    ),
    swarm_secret_name TEXT NOT NULL UNIQUE,
    created_at_ms INTEGER NOT NULL CHECK (created_at_ms > 0),
    retired_at_ms INTEGER,
    PRIMARY KEY(secret_id, generation),
    CHECK (retired_at_ms IS NULL OR retired_at_ms >= created_at_ms)
);

CREATE INDEX secret_generations_cleanup_idx
    ON secret_generations(retired_at_ms, created_at_ms);

UPDATE instance_metadata SET schema_version = 2 WHERE singleton = 1;
