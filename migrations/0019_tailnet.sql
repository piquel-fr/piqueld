-- Tailnet user or tag an API token is bound to, e.g. `tag:ci`.
ALTER TABLE auth_credentials ADD COLUMN tailnet TEXT;
-- Who an audited request came from on the tailnet. A record's chain link
-- covers it only when present, so links written before stay valid.
ALTER TABLE audit_events ADD COLUMN tailnet TEXT;
