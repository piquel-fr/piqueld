-- Editable applications remain authoritative. Operations execute captured snapshots.
CREATE TABLE deployments (
    id TEXT PRIMARY KEY REFERENCES operations(id) ON DELETE CASCADE,
    application_id TEXT NOT NULL REFERENCES applications(id) ON DELETE CASCADE,
    manifest_json TEXT NOT NULL CHECK(json_valid(manifest_json)),
    generation INTEGER NOT NULL CHECK(generation > 0),
    created_at_ms INTEGER NOT NULL,
    succeeded_at_ms INTEGER
);
CREATE INDEX deployment_application ON deployments(application_id,created_at_ms DESC,id DESC);
-- Only the latest legacy intent has a recoverable source manifest. Do not invent history.
INSERT INTO deployments(id,application_id,manifest_json,generation,created_at_ms,succeeded_at_ms)
SELECT o.id,a.id,a.desired_json,o.generation,o.created_at_ms,
       CASE WHEN o.state='succeeded' THEN o.finished_at_ms END
FROM applications a JOIN operations o ON o.id=(SELECT id FROM operations WHERE application_id=a.id ORDER BY created_at_ms DESC,id DESC LIMIT 1)
WHERE o.kind!='delete' AND a.deleted_at_ms IS NULL;

CREATE TABLE deployment_attempts (
    deployment_id TEXT NOT NULL REFERENCES deployments(id) ON DELETE CASCADE,
    attempt INTEGER NOT NULL,
    outcome_json TEXT NOT NULL CHECK(json_valid(outcome_json)),
    PRIMARY KEY(deployment_id,attempt)
);

CREATE TABLE application_status_new (
    application_id TEXT PRIMARY KEY REFERENCES applications(id) ON DELETE CASCADE,
    state TEXT NOT NULL CHECK(state IN ('not_deployed','pending','deploying','ready','degraded','deleting','failed')),
    message TEXT,
    runtime_health TEXT,
    updated_at_ms INTEGER NOT NULL
);
INSERT INTO application_status_new SELECT * FROM application_status;
DROP TABLE application_status;
ALTER TABLE application_status_new RENAME TO application_status;
