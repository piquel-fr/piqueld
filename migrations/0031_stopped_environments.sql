-- Stopped environments: their services run no replicas and the gateway
-- serves none of their routes, until they are started or deployed again.
ALTER TABLE environments ADD COLUMN stopped INTEGER NOT NULL DEFAULT 0
    CHECK (stopped IN (0, 1));
