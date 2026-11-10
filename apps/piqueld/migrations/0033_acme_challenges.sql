-- DNS-01 challenge TXT records piqueld creates, one per record name. A row is
-- saved before its record is created, gets the provider's `record_id` once
-- created, and is removed once the record is deleted, so records left by a
-- failed deletion or a crash are deleted after a restart. `zone` is the zone's
-- name, resolved to its provider again when deleting; `value` finds a record
-- whose creation never returned its ID.
CREATE TABLE acme_challenges (
    name TEXT PRIMARY KEY NOT NULL,
    zone TEXT NOT NULL,
    value TEXT NOT NULL,
    record_id TEXT
) STRICT;
