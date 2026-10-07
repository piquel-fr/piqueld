-- Each environment of a repository-backed application follows its own branch
-- of the application's manifest repository, optionally pinned to a commit.
-- `branch` is NULL for environments that deploy the saved manifest.
ALTER TABLE environments ADD COLUMN branch TEXT;
ALTER TABLE environments ADD COLUMN pinned_commit TEXT CHECK (pinned_commit IS NULL OR branch IS NOT NULL);

-- The manifest last fetched from the environment's branch, with references
-- unresolved; NULL before the first fetch and for saved-manifest environments.
ALTER TABLE environments ADD COLUMN manifest_json TEXT CHECK (manifest_json IS NULL OR json_valid(manifest_json));

-- Environments of repository-backed applications take the branch and pinned
-- commit `spec.manifest` names. Until then, a fetched manifest was saved as the
-- application's configuration, so it stays each environment's last fetched one
-- and nothing deploys or reserves differently after upgrading.
UPDATE environments SET
    branch=(SELECT json_extract(a.desired_json,'$.spec.manifest.repository.branch') FROM applications a WHERE a.id=environments.application_id),
    pinned_commit=(SELECT json_extract(a.desired_json,'$.spec.manifest.repository.commit') FROM applications a WHERE a.id=environments.application_id),
    manifest_json=(SELECT a.desired_json FROM applications a WHERE a.id=environments.application_id)
WHERE (SELECT json_extract(a.desired_json,'$.spec.manifest') FROM applications a WHERE a.id=environments.application_id) IS NOT NULL;

-- Problems found while fetching a deployment's manifest that did not stop it,
-- such as a `spec.manifest` that differs from the application's connection.
ALTER TABLE deployments ADD COLUMN warnings_json TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(warnings_json));
