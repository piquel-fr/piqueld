-- Hostnames whose DNS records piqueld manages: a name is added before its
-- `_piqueld.<hostname>` ownership record is written, and removed once its
-- records are deleted, so a removed route's records are found after a restart.
CREATE TABLE dns_records (hostname TEXT PRIMARY KEY NOT NULL) STRICT;
