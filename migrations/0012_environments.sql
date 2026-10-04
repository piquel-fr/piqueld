-- Applications split into the product and its deployable environments.
-- Every existing application becomes an application with one environment
-- named `production`. Both keep the existing ID, so Docker names, ownership
-- labels, secret encryption context and history still derive from it.
DROP INDEX application_live_name;
-- Renaming the table also repoints every foreign key that referenced it.
ALTER TABLE applications RENAME TO environments;

-- The shared manifest and its revision belong to the application.
CREATE TABLE applications (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    desired_json TEXT NOT NULL CHECK (json_valid(desired_json)),
    generation INTEGER NOT NULL DEFAULT 1 CHECK (generation > 0),
    delete_intent INTEGER NOT NULL DEFAULT 0 CHECK (delete_intent IN (0, 1)),
    created_at_ms INTEGER NOT NULL,
    updated_at_ms INTEGER NOT NULL
) STRICT;
INSERT INTO applications(id,name,desired_json,generation,delete_intent,created_at_ms,updated_at_ms)
SELECT id,name,desired_json,generation,delete_intent,created_at_ms,updated_at_ms FROM environments;

-- SQLite cannot add a NOT NULL foreign key to existing rows; the store always
-- sets it. Environments are removed before their application, never cascaded.
ALTER TABLE environments ADD COLUMN application_id TEXT REFERENCES applications(id);
UPDATE environments SET application_id=id,name='production';
ALTER TABLE environments DROP COLUMN desired_json;
ALTER TABLE environments DROP COLUMN generation;
-- Tombstones were removed by 0002; deletion is physical.
ALTER TABLE environments DROP COLUMN deleted_at_ms;
CREATE UNIQUE INDEX environment_name ON environments(application_id,name);

-- Runtime records belong to environments. Column and table renames also
-- update the indexes and foreign keys that name them.
ALTER TABLE application_status RENAME TO environment_status;
ALTER TABLE environment_status RENAME COLUMN application_id TO environment_id;
ALTER TABLE operations RENAME COLUMN application_id TO environment_id;
ALTER TABLE deployments RENAME COLUMN application_id TO environment_id;
ALTER TABLE builds RENAME COLUMN application_id TO environment_id;
ALTER TABLE events RENAME COLUMN application_id TO environment_id;
ALTER TABLE active_actions RENAME COLUMN application_id TO environment_id;
ALTER TABLE notification_conditions RENAME COLUMN application_id TO environment_id;
ALTER TABLE application_secrets RENAME TO environment_secrets;
ALTER TABLE environment_secrets RENAME COLUMN application_id TO environment_id;
ALTER TABLE secret_versions RENAME COLUMN application_id TO environment_id;
ALTER TABLE deployment_secret_pins RENAME COLUMN application_id TO environment_id;
ALTER TABLE application_routes RENAME TO environment_routes;
ALTER TABLE environment_routes RENAME COLUMN application_id TO environment_id;
ALTER TABLE hostname_reservations RENAME COLUMN application_id TO environment_id;
