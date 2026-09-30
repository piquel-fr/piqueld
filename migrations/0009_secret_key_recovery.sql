-- Preserve immutable identities when lost-key recovery discards their values.
ALTER TABLE secret_versions ADD COLUMN available INTEGER NOT NULL DEFAULT 1 CHECK (available IN (0,1));
