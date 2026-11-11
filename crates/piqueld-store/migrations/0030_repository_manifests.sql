-- Repository-backed applications become editable. Each keeps the manifest it
-- last read from its repository: while the manifest there is unchanged, its
-- deployments keep deploying the saved one, edits included. Saved manifests
-- could not be edited until now, so each repository-backed application's is
-- the one it last read, and none starts with edits. Nothing is redeployed.
ALTER TABLE applications ADD COLUMN repository_json TEXT CHECK (repository_json IS NULL OR json_valid(repository_json));
UPDATE applications SET repository_json=desired_json WHERE json_extract(desired_json,'$.spec.manifest') IS NOT NULL;

-- A deployment captures the application's repository manifest when requested,
-- and records the manifest it fetched; when they differ, a push changed it.
ALTER TABLE deployment_inputs ADD COLUMN repository_json TEXT CHECK (repository_json IS NULL OR json_valid(repository_json));
ALTER TABLE deployment_inputs ADD COLUMN fetched_json TEXT CHECK (fetched_json IS NULL OR json_valid(fetched_json));
-- Deployments requested before this migration started from the saved manifest,
-- and those already fetched deployed what they fetched.
UPDATE deployment_inputs SET repository_json=(SELECT a.repository_json FROM operations o JOIN environments e ON e.id=o.environment_id JOIN applications a ON a.id=e.application_id WHERE o.id=deployment_inputs.operation_id);
UPDATE deployment_inputs SET fetched_json=application_json WHERE fetched=1 AND repository_commit IS NOT NULL;
