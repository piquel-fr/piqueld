-- Tamper evidence: audit records get consecutive IDs, and each stores a
-- SHA-256 link over its predecessor's link and its own ID and fields.
-- Releases apply this together with 0017_audit.sql, so the trail is empty;
-- records from pre-release builds cannot be linked in SQL and are dropped.
DELETE FROM audit_events;
ALTER TABLE audit_events ADD COLUMN link TEXT;
-- The newest pruned record, which the oldest retained one extends.
CREATE TABLE audit_chain (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1),
    pruned_through INTEGER,
    pruned_link TEXT,
    CHECK ((pruned_through IS NULL) = (pruned_link IS NULL))
);
INSERT INTO audit_chain(singleton) VALUES (1);
-- Recent refusals by caller, counted on each refusal to detect bursts.
CREATE INDEX audit_refusals ON audit_events(user_id, peer, created_at_ms) WHERE outcome = 'denied';
-- Recent burst notifications by address and account, checked on each refusal.
CREATE INDEX security_denial_bursts ON events(resource, actor_user_id, created_at_ms) WHERE kind = 'access_denial_burst';
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
