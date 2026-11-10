-- Deployments reuse the image of an earlier build with the same commit and
-- build settings instead of building it again.
--
-- `builds.reused_build_id` names the build whose image a build reused. It
-- has no foreign key: build records are informational history, and the
-- reused one may be gone. Existing builds all built their image, so theirs
-- stays NULL.
ALTER TABLE builds ADD COLUMN reused_build_id INTEGER;
-- Reuse looks earlier builds up by the commit they checked out.
CREATE INDEX builds_commit ON builds(commit_hash) WHERE commit_hash IS NOT NULL;
-- A deployment requested with `rebuild` builds every Git source, whatever
-- could be reused. Existing deployments never asked for one.
ALTER TABLE deployments ADD COLUMN rebuild INTEGER NOT NULL DEFAULT 0
    CHECK (rebuild IN (0, 1));
