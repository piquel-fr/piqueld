-- Keep application identities and history while replacing the prototype journal.
DROP TABLE mutation_idempotency;
DROP TABLE operation_steps;
ALTER TABLE operations RENAME TO old_operations;
ALTER TABLE application_status RENAME TO old_application_status;
ALTER TABLE applications RENAME TO old_applications;

CREATE TABLE applications (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    desired_json TEXT NOT NULL CHECK (json_valid(desired_json)),
    resolved_json TEXT NOT NULL CHECK (json_valid(resolved_json)),
    delete_intent INTEGER NOT NULL DEFAULT 0 CHECK (delete_intent IN (0, 1)),
    deleted_at_ms INTEGER,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
);
CREATE UNIQUE INDEX application_live_name ON applications(name) WHERE deleted_at_ms IS NULL;

CREATE TABLE application_status (
    application_id TEXT PRIMARY KEY REFERENCES applications(id) ON DELETE CASCADE,
    state TEXT NOT NULL CHECK (state IN ('pending','deploying','ready','degraded','deleting','failed')),
    message TEXT,
    updated_at_ms INTEGER NOT NULL
);

CREATE TABLE operations (
    id TEXT PRIMARY KEY,
    application_id TEXT NOT NULL REFERENCES applications(id) ON DELETE CASCADE,
    kind TEXT NOT NULL CHECK (kind IN ('apply','delete')),
    state TEXT NOT NULL CHECK (state IN ('requested','running','succeeded','failed','cancelled')),
    error_code TEXT,
    error_message TEXT,
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL,
    started_at_ms INTEGER,
    finished_at_ms INTEGER,
    CHECK ((error_code IS NULL) = (error_message IS NULL)),
    CHECK ((state IN ('succeeded','failed','cancelled')) = (finished_at_ms IS NOT NULL))
);

INSERT INTO applications SELECT id,name,desired_json,resolved_json,delete_intent,deleted_at_ms,created_at_ms,updated_at_ms FROM old_applications;
INSERT INTO application_status SELECT application_id,state,message,updated_at_ms FROM old_application_status;
INSERT INTO operations
SELECT id,application_id,CASE WHEN kind='delete' THEN 'delete' ELSE 'apply' END,
       CASE WHEN state IN ('pending','recovery') THEN 'requested' ELSE state END,
       error_code,error_message,created_at_ms,updated_at_ms,started_at_ms,finished_at_ms
FROM old_operations;
-- Only the latest request can remain active after migration.
UPDATE operations SET state='cancelled',finished_at_ms=updated_at_ms
WHERE state IN ('requested','running') AND id != (
    SELECT latest.id FROM operations latest WHERE latest.application_id=operations.application_id
    ORDER BY latest.created_at_ms DESC,latest.id DESC LIMIT 1
);

DROP TABLE old_operations;
DROP TABLE old_application_status;
DROP TABLE old_applications;
CREATE INDEX operation_application ON operations(application_id,created_at_ms DESC,id DESC);
CREATE UNIQUE INDEX operation_running ON operations(application_id) WHERE state='running';
CREATE INDEX operation_finished ON operations(finished_at_ms) WHERE finished_at_ms IS NOT NULL;
