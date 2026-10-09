-- Immutable releases: what a successful preparation in a tracking environment
-- deployed, with each service's source provenance and prepared image. They
-- belong to the application, so they survive the environment that recorded them.
CREATE TABLE releases (
 id TEXT PRIMARY KEY,
 application_id TEXT NOT NULL REFERENCES applications(id) ON DELETE CASCADE,
 -- Preparations with the same content share one release.
 content_hash TEXT NOT NULL,
 release_json TEXT NOT NULL CHECK (json_valid(release_json)),
 created_at_ms INTEGER NOT NULL,
 UNIQUE(application_id,content_hash)
);
CREATE INDEX release_application ON releases(application_id,id DESC);

-- The release a deployment's prepared target runs; NULL until prepared, and
-- for deployments prepared before releases existed.
ALTER TABLE deployments ADD COLUMN release_id TEXT REFERENCES releases(id);
