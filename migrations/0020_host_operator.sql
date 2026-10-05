-- Sign-in links and browser sessions of the host operator: root or the
-- daemon's own Unix user, identified by the kernel over the Unix socket. They
-- belong to no account. Redeeming a link turns it into a session, replacing
-- its secret hash with the session's.
CREATE TABLE auth_operator_sessions (
    id TEXT PRIMARY KEY,
    secret_hash TEXT NOT NULL UNIQUE,
    uid INTEGER NOT NULL,
    redeemed INTEGER NOT NULL DEFAULT 0 CHECK (redeemed IN (0, 1)),
    expires_at INTEGER NOT NULL
);
-- The host operator's Unix user, for requests it made. A record's chain link
-- covers it only when present, so links written before stay valid.
ALTER TABLE audit_events ADD COLUMN operator_uid INTEGER;
ALTER TABLE operations ADD COLUMN actor_operator_uid INTEGER;
ALTER TABLE events ADD COLUMN actor_operator_uid INTEGER;
ALTER TABLE active_actions ADD COLUMN actor_operator_uid INTEGER;
-- Events about an operation inherit its actor, now including the operator.
DROP TRIGGER events_operation_actor;
CREATE TRIGGER events_operation_actor AFTER INSERT ON events
WHEN NEW.operation_id IS NOT NULL AND NEW.action_id IS NULL AND NEW.actor_user_id IS NULL
    AND NEW.actor_operator_uid IS NULL
BEGIN
    UPDATE events SET
        actor_user_id = (SELECT actor_user_id FROM operations WHERE id = NEW.operation_id),
        actor_credential_id = (SELECT actor_credential_id FROM operations WHERE id = NEW.operation_id),
        actor_operator_uid = (SELECT actor_operator_uid FROM operations WHERE id = NEW.operation_id)
    WHERE id = NEW.id;
END;
