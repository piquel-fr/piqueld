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

-- Stored environment responses replay retried requests. Their source was the
-- string `saved` or `repository`; give them the environment's migrated one.
UPDATE request_receipts SET response_json=json_set(response_json,'$.Environment.source',json(COALESCE(
    (SELECT CASE WHEN e.branch IS NULL THEN json_object('type','saved')
        ELSE json_patch(json_object('type','branch','branch',e.branch),json_object('commit',e.pinned_commit)) END
     FROM environments e WHERE e.id=json_extract(request_receipts.response_json,'$.Environment.id')),
    json_object('type','saved'))))
WHERE json_type(response_json,'$.Environment.source')='text';

-- Problems found while fetching a deployment's manifest that did not stop it,
-- such as a `spec.manifest` that differs from the application's connection.
ALTER TABLE deployments ADD COLUMN warnings_json TEXT NOT NULL DEFAULT '[]' CHECK (json_valid(warnings_json));
