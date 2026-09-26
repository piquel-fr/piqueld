-- Preserve immutable identities when recovery deliberately discards their values.
ALTER TABLE secret_versions ADD COLUMN available INTEGER NOT NULL DEFAULT 1 CHECK (available IN (0,1));

-- A committed replacement is recoverable until its durable staged key is installed.
ALTER TABLE secret_key_verification ADD COLUMN pending_key TEXT;
