-- Hostnames whose DNS records piqueld manages: a name is added before its
-- `_piqueld.<hostname>` ownership record is written, and removed once its
-- records are deleted, so a removed route's records are found after a restart.
-- `unpublished` is set before changes a provider stages (OVH) and cleared once
-- the zone is published, so a failed publish is retried even without changes.
CREATE TABLE dns_records (
    hostname TEXT PRIMARY KEY NOT NULL,
    unpublished INTEGER NOT NULL DEFAULT 0 CHECK(unpublished IN (0,1))
) STRICT;
