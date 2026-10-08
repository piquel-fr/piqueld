-- Manually set secrets move from environments into one store per application,
-- where each secret lists the environments allowed to mount it. Generated
-- secrets, the ones the saved manifest declares, stay with their environment.
CREATE TABLE application_secrets (
 application_id TEXT NOT NULL REFERENCES applications(id) ON DELETE CASCADE,
 name TEXT NOT NULL,
 generation INTEGER NOT NULL CHECK(generation>0),
 updated_at_ms INTEGER NOT NULL,
 -- Cleanup reservations survive cancellation, partial Docker failures and restarts.
 deletion_id TEXT,
 -- Every environment, including later ones, or only those in application_secret_access.
 all_environments INTEGER NOT NULL CHECK (all_environments IN (0,1)),
 -- Stored for previews, which do not exist yet.
 previews INTEGER NOT NULL CHECK (previews IN (0,1)),
 PRIMARY KEY(application_id,name)
);
-- Environments are listed by ID: renaming one keeps its access, deleting one
-- removes it from every list.
CREATE TABLE application_secret_access (
 application_id TEXT NOT NULL,
 name TEXT NOT NULL,
 environment_id TEXT NOT NULL REFERENCES environments(id) ON DELETE CASCADE,
 PRIMARY KEY(application_id,name,environment_id),
 FOREIGN KEY(application_id,name) REFERENCES application_secrets(application_id,name) ON DELETE CASCADE
);
CREATE TABLE application_secret_versions (
 application_id TEXT NOT NULL,
 name TEXT NOT NULL,
 generation INTEGER NOT NULL CHECK(generation>0),
 nonce BLOB NOT NULL,
 ciphertext BLOB NOT NULL,
 available INTEGER NOT NULL DEFAULT 1 CHECK (available IN (0,1)),
 -- The environment a value moved from, whose encryption context it keeps
 -- until the daemon re-encrypts it on startup.
 moved_from TEXT,
 PRIMARY KEY(application_id,name,generation),
 FOREIGN KEY(application_id,name) REFERENCES application_secrets(application_id,name) ON DELETE CASCADE
);
-- Docker secrets belong to one environment, so each environment that mounts
-- a stored version gets its own Docker secret.
CREATE TABLE application_secret_copies (
 environment_id TEXT NOT NULL REFERENCES environments(id) ON DELETE CASCADE,
 application_id TEXT NOT NULL,
 name TEXT NOT NULL,
 generation INTEGER NOT NULL,
 swarm_name TEXT NOT NULL UNIQUE,
 PRIMARY KEY(environment_id,name,generation),
 FOREIGN KEY(application_id,name,generation) REFERENCES application_secret_versions(application_id,name,generation) ON DELETE CASCADE
);
CREATE TABLE deployment_stored_secret_pins (
 operation_id TEXT NOT NULL REFERENCES operations(id) ON DELETE CASCADE,
 environment_id TEXT NOT NULL,
 name TEXT NOT NULL,
 generation INTEGER NOT NULL,
 PRIMARY KEY(operation_id,name),
 FOREIGN KEY(environment_id,name,generation) REFERENCES application_secret_copies(environment_id,name,generation)
);

-- Environments may disagree about a name: one sets it manually while another's
-- manifest declares it. Since a name is either generated or stored per
-- application, the environment that uses it and comes first decides:
-- production, else the oldest. An environment uses a name when it holds a value
-- for it or the manifest it deploys declares it: the saved one, or for an
-- environment following a branch only the last one fetched from it.
-- The deciding environment's value is generated, and stays, when its manifest
-- declares the name or a retained deployment of it both declares and pins it
-- (deleting a secret drops its pins, so a name set manually after deletion counts
-- as manual). Otherwise it is manual and moves, with access limited to that
-- environment. Same-named secrets of other environments stay with them, where
-- their deployments' pins keep them, until deleted.
CREATE TEMP TABLE moved_secrets AS
WITH users(environment_id,name) AS (
 SELECT environment_id,name FROM environment_secrets
 UNION
 SELECT e.id,json_extract(j.value,'$.name')
 FROM environments e JOIN applications a ON a.id=e.application_id,
  json_each(CASE WHEN e.branch IS NULL THEN a.desired_json ELSE e.manifest_json END,'$.spec.secrets') j
),
deciding(environment_id,name) AS (
 SELECT environment_id,name FROM (
  SELECT u.environment_id,u.name,
   row_number() OVER (PARTITION BY e.application_id,u.name ORDER BY e.name='production' DESC,e.created_at_ms,e.id) AS rank
  FROM users u JOIN environments e ON e.id=u.environment_id
 ) WHERE rank=1
)
SELECT s.environment_id,e.application_id,s.name
FROM deciding
JOIN environment_secrets s USING(environment_id,name)
JOIN environments e ON e.id=s.environment_id
JOIN applications a ON a.id=e.application_id
WHERE NOT EXISTS (
 SELECT 1 FROM json_each(CASE WHEN e.branch IS NULL THEN a.desired_json ELSE e.manifest_json END,'$.spec.secrets') j
 WHERE json_extract(j.value,'$.name')=s.name
)
AND NOT EXISTS (
 SELECT 1 FROM deployments d
 JOIN deployment_secret_pins p ON p.operation_id=d.id AND p.environment_id=s.environment_id AND p.name=s.name
 JOIN json_each(d.template_json,'$.spec.secrets') j ON json_extract(j.value,'$.name')=s.name
 WHERE d.environment_id=s.environment_id
);
INSERT INTO application_secrets(application_id,name,generation,updated_at_ms,deletion_id,all_environments,previews)
SELECT m.application_id,s.name,s.generation,s.updated_at_ms,s.deletion_id,0,0
FROM moved_secrets m JOIN environment_secrets s USING(environment_id,name);
INSERT INTO application_secret_access(application_id,name,environment_id)
SELECT application_id,name,environment_id FROM moved_secrets;
INSERT INTO application_secret_versions(application_id,name,generation,nonce,ciphertext,available,moved_from)
SELECT m.application_id,v.name,v.generation,v.nonce,v.ciphertext,v.available,CASE WHEN v.available=1 THEN v.environment_id END
FROM moved_secrets m JOIN secret_versions v USING(environment_id,name);
-- Copies keep their Docker secret names, so running services and existing
-- deployment pins still use the same Docker secrets.
INSERT INTO application_secret_copies(environment_id,application_id,name,generation,swarm_name)
SELECT v.environment_id,m.application_id,v.name,v.generation,v.swarm_name
FROM moved_secrets m JOIN secret_versions v USING(environment_id,name);
INSERT INTO deployment_stored_secret_pins(operation_id,environment_id,name,generation)
SELECT p.operation_id,p.environment_id,p.name,p.generation
FROM moved_secrets m JOIN deployment_secret_pins p USING(environment_id,name);
DELETE FROM deployment_secret_pins WHERE (environment_id,name) IN (SELECT environment_id,name FROM moved_secrets);
DELETE FROM environment_secrets WHERE (environment_id,name) IN (SELECT environment_id,name FROM moved_secrets);
DROP TABLE moved_secrets;
