CREATE TABLE application_secrets (
 application_id TEXT NOT NULL REFERENCES applications(id) ON DELETE CASCADE,
 name TEXT NOT NULL,
 generation INTEGER NOT NULL CHECK(generation>0),
 updated_at_ms INTEGER NOT NULL,
 -- Cleanup reservations survive cancellation, partial Docker failures and restarts.
 deletion_id TEXT,
 PRIMARY KEY(application_id,name)
);
CREATE TABLE secret_versions (
 application_id TEXT NOT NULL,
 name TEXT NOT NULL,
 generation INTEGER NOT NULL CHECK(generation>0),
 swarm_name TEXT NOT NULL UNIQUE,
 nonce BLOB NOT NULL,
 ciphertext BLOB NOT NULL,
 -- Preserve immutable identities when lost-key recovery discards their values.
 available INTEGER NOT NULL DEFAULT 1 CHECK (available IN (0,1)),
 PRIMARY KEY(application_id,name,generation),
 FOREIGN KEY(application_id,name) REFERENCES application_secrets(application_id,name) ON DELETE CASCADE
);
CREATE TABLE deployment_secret_pins (
 operation_id TEXT NOT NULL REFERENCES operations(id) ON DELETE CASCADE,
 application_id TEXT NOT NULL,
 name TEXT NOT NULL,
 generation INTEGER NOT NULL,
 PRIMARY KEY(operation_id,name),
 FOREIGN KEY(application_id,name,generation) REFERENCES secret_versions(application_id,name,generation)
);
-- An empty pin set must also remain immutable across retries.
CREATE TABLE deployment_secrets_prepared (
 operation_id TEXT PRIMARY KEY REFERENCES operations(id) ON DELETE CASCADE
);
-- Keep the key binding even after the final secret is deleted.
CREATE TABLE secret_key_verification (
 singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
 nonce BLOB NOT NULL,
 ciphertext BLOB NOT NULL
);
