-- Promoted environments never build or fetch: they only receive releases
-- promoted from another environment of the same application, stored by ID so
-- renames keep it. Deleting a source with live dependents is refused; when
-- both go (deleting the application), the dependent no longer names it.
ALTER TABLE environments ADD COLUMN promoted_from TEXT REFERENCES environments(id) ON DELETE SET NULL
    CHECK (promoted_from IS NULL OR (kind = 'environment' AND branch IS NULL AND promoted_from != id));
CREATE INDEX environment_promotion_source ON environments(promoted_from) WHERE promoted_from IS NOT NULL;

-- Where each deployment came from: `{"type":"build"}` for deployments built
-- from their environment's source, which every existing one was, or a
-- promoted or redeployed release.
ALTER TABLE deployments ADD COLUMN origin_json TEXT NOT NULL DEFAULT '{"type":"build"}' CHECK (json_valid(origin_json));
