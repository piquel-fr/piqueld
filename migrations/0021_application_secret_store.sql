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

-- A secret is generated, and stays with its environment, when the saved
-- manifest or any environment's last fetched one declares its name (so no
-- declaration conflicts with the store), or when a retained deployment both
-- declares and pins it (deleting a secret drops its pins, so a name set manually
-- after deletion counts as manual). Of the other, manually set secrets, one per
-- application and name moves, with access limited to the environment it came
-- from: production's, else the oldest environment's. Same-named secrets of other
-- environments stay with them, where their deployments' pins keep them, until deleted.
CREATE TEMP TABLE moved_secrets AS
SELECT environment_id,application_id,name FROM (
 SELECT s.environment_id,e.application_id,s.name,
  row_number() OVER (PARTITION BY e.application_id,s.name ORDER BY e.name='production' DESC,e.created_at_ms,e.id) AS rank
 FROM environment_secrets s
 JOIN environments e ON e.id=s.environment_id
 JOIN applications a ON a.id=e.application_id
 WHERE NOT EXISTS (
  SELECT 1 FROM json_each(a.desired_json,'$.spec.secrets') j WHERE json_extract(j.value,'$.name')=s.name
  UNION ALL
  SELECT 1 FROM environments o, json_each(o.manifest_json,'$.spec.secrets') j
  WHERE o.application_id=e.application_id AND json_extract(j.value,'$.name')=s.name
 )
 AND NOT EXISTS (
  SELECT 1 FROM deployments d
  JOIN deployment_secret_pins p ON p.operation_id=d.id AND p.environment_id=s.environment_id AND p.name=s.name
  JOIN json_each(d.template_json,'$.spec.secrets') j ON json_extract(j.value,'$.name')=s.name
  WHERE d.environment_id=s.environment_id
 )
) WHERE rank=1;
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
