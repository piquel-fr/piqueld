-- A manual Deploy owns a snapshot separate from accepted application intent.
-- Fetched manifests and the commit they came from survive restart, are
-- accepted only after preparation, and are removed with their operation.
CREATE TABLE deployment_inputs (
    operation_id TEXT PRIMARY KEY REFERENCES operations(id) ON DELETE CASCADE,
    application_json TEXT NOT NULL CHECK (json_valid(application_json)),
    repository_commit TEXT,
    fetched INTEGER NOT NULL DEFAULT 0 CHECK (fetched IN (0, 1))
);
