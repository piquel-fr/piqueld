-- Tamper evidence: each audit record stores a SHA-256 link over its
-- predecessor's link and its own fields. Records written before this
-- migration carry none.
ALTER TABLE audit_events ADD COLUMN link TEXT;
-- Link of the newest pruned record, which the oldest retained one extends,
-- and the newest record from before the chain: only those may lack a link.
CREATE TABLE audit_chain (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    pruned_link TEXT,
    unlinked_through INTEGER
);
INSERT INTO audit_chain(singleton,unlinked_through) SELECT 1,MAX(id) FROM audit_events;
-- Recent refusals by caller, counted on each refusal to detect bursts.
CREATE INDEX audit_refusals ON audit_events(user_id, peer, created_at_ms) WHERE outcome = 'denied';
-- Network addresses each credential was used from, so use from a new one
-- can raise a security notification.
CREATE TABLE auth_credential_addresses (
    credential_id TEXT NOT NULL REFERENCES auth_credentials(id) ON DELETE CASCADE,
    address TEXT NOT NULL,
    first_seen_ms INTEGER NOT NULL,
    PRIMARY KEY (credential_id, address)
) WITHOUT ROWID;
-- The single outstanding admin recovery link, issued over the Unix socket to
-- root or the daemon's own user. Redeeming it creates an administrator.
CREATE TABLE auth_recovery (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    secret_hash TEXT NOT NULL UNIQUE,
    expires_at INTEGER NOT NULL
);
