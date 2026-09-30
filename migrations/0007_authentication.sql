CREATE TABLE auth_setup (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    initialized INTEGER NOT NULL DEFAULT 0,
    secret_hash TEXT
);
INSERT INTO auth_setup(singleton) VALUES(1);
CREATE TABLE auth_users (
    id TEXT PRIMARY KEY,
    username TEXT NOT NULL UNIQUE COLLATE NOCASE,
    display_name TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE TABLE auth_passkeys (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES auth_users(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    credential TEXT NOT NULL,
    created_at INTEGER NOT NULL
);
CREATE TABLE auth_credentials (
    id TEXT PRIMARY KEY,
    user_id TEXT NOT NULL REFERENCES auth_users(id) ON DELETE CASCADE,
    secret_hash TEXT NOT NULL UNIQUE,
    kind TEXT NOT NULL CHECK (kind IN ('browser','cli','token')),
    name TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    last_used_at INTEGER NOT NULL,
    expires_at INTEGER
);
CREATE TABLE auth_invitations (
    id TEXT PRIMARY KEY,
    issuer_id TEXT NOT NULL REFERENCES auth_users(id) ON DELETE CASCADE,
    secret_hash TEXT NOT NULL UNIQUE,
    expires_at INTEGER NOT NULL
);
CREATE INDEX auth_passkeys_user ON auth_passkeys(user_id);
CREATE INDEX auth_credentials_user ON auth_credentials(user_id);
