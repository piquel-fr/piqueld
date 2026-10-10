-- Deploying on push. How an application syncs is part of its repository
-- connection, `spec.manifest.sync` in its saved manifest; this stores what
-- sync observed and the webhook secret.

-- An environment opts into its application's sync with `sync = 1`;
-- previews follow their application's setting and ignore it.
-- `synced_commit` is the branch head as of its last deployment, sync's or one
-- of its own branch: sync deploys only once the branch moves past it, and
-- nothing follows before its first deployment.
ALTER TABLE environments ADD COLUMN sync INTEGER NOT NULL DEFAULT 0
    CHECK (sync IN (0, 1));
ALTER TABLE environments ADD COLUMN synced_commit TEXT;
ALTER TABLE environments ADD COLUMN synced_at_ms INTEGER
    CHECK ((synced_commit IS NULL) = (synced_at_ms IS NULL));

-- The branch the environment or preview tracked, unpinned, when the
-- deployment was requested: the only one whose head its fetch may record.
ALTER TABLE deployment_inputs ADD COLUMN tracked_branch TEXT;

-- The last listing of the application's branches, and why it failed.
ALTER TABLE applications ADD COLUMN sync_checked_at_ms INTEGER;
ALTER TABLE applications ADD COLUMN sync_error TEXT;

-- The secret GitHub signs an application's push webhooks with, encrypted
-- with the secret master key. Generating another replaces it.
CREATE TABLE webhook_secrets (
    application_id TEXT PRIMARY KEY REFERENCES applications(id) ON DELETE CASCADE,
    nonce BLOB NOT NULL,
    ciphertext BLOB NOT NULL,
    created_at_ms INTEGER NOT NULL
) STRICT;

-- An automated actor, e.g. `sync:poll`, when one caused the record instead
-- of an account or the host operator.
ALTER TABLE operations ADD COLUMN actor_system TEXT;
ALTER TABLE events ADD COLUMN actor_system TEXT;
ALTER TABLE active_actions ADD COLUMN actor_system TEXT;
-- Events about an operation inherit its actor, now including system actors.
DROP TRIGGER events_operation_actor;
CREATE TRIGGER events_operation_actor AFTER INSERT ON events
WHEN NEW.operation_id IS NOT NULL AND NEW.action_id IS NULL AND NEW.actor_user_id IS NULL
    AND NEW.actor_operator_uid IS NULL AND NEW.actor_system IS NULL
BEGIN
    UPDATE events SET
        actor_user_id = (SELECT actor_user_id FROM operations WHERE id = NEW.operation_id),
        actor_credential_id = (SELECT actor_credential_id FROM operations WHERE id = NEW.operation_id),
        actor_operator_uid = (SELECT actor_operator_uid FROM operations WHERE id = NEW.operation_id),
        actor_system = (SELECT actor_system FROM operations WHERE id = NEW.operation_id)
    WHERE id = NEW.id;
END;
