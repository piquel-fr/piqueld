-- Who did what through the API, kept independently of application history:
-- application deletion keeps these rows, and they have their own retention.
-- Accounts and credentials are copied, not referenced, so records outlive them.
CREATE TABLE audit_events (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    created_at_ms INTEGER NOT NULL,
    action TEXT NOT NULL,
    outcome TEXT NOT NULL CHECK (outcome IN ('allowed', 'denied', 'failed')),
    status INTEGER NOT NULL,
    user_id TEXT,
    username TEXT,
    credential_id TEXT,
    credential_kind TEXT,
    scoped INTEGER CHECK (scoped IS NULL OR scoped IN (0, 1)),
    peer TEXT,
    request_id TEXT,
    application_id TEXT,
    environment_id TEXT,
    permission TEXT
);
CREATE INDEX audit_created ON audit_events(created_at_ms);
CREATE INDEX audit_user ON audit_events(user_id, id);
CREATE INDEX audit_credential ON audit_events(credential_id, id);
-- Operations and history record the account and credential that caused them.
ALTER TABLE operations ADD COLUMN actor_user_id TEXT;
ALTER TABLE operations ADD COLUMN actor_credential_id TEXT;
ALTER TABLE events ADD COLUMN actor_user_id TEXT;
ALTER TABLE events ADD COLUMN actor_credential_id TEXT;
-- Runtime requests record their actor while they run: their operation's
-- actor when they started, or the caller of a request outside operations,
-- like removing a deleted secret's versions.
ALTER TABLE active_actions ADD COLUMN actor_user_id TEXT;
ALTER TABLE active_actions ADD COLUMN actor_credential_id TEXT;
-- Other events about an operation carry the actor that requested it, so work
-- done long after the request stays attributed.
CREATE TRIGGER events_operation_actor AFTER INSERT ON events
WHEN NEW.operation_id IS NOT NULL AND NEW.action_id IS NULL AND NEW.actor_user_id IS NULL
BEGIN
    UPDATE events SET
        actor_user_id = (SELECT actor_user_id FROM operations WHERE id = NEW.operation_id),
        actor_credential_id = (SELECT actor_credential_id FROM operations WHERE id = NEW.operation_id)
    WHERE id = NEW.id;
END;
