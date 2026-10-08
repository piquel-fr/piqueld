-- Scoped credentials act with their own grants, limited by their account's
-- current access; other credentials act with the account's access. Scoped
-- credentials without grants can do nothing.
ALTER TABLE auth_credentials ADD COLUMN scoped INTEGER NOT NULL DEFAULT 0 CHECK (scoped IN (0, 1));
-- Existing API tokens keep acting with their account's full access.
UPDATE auth_credentials SET scoped = 1 WHERE kind = 'token';
INSERT INTO auth_grants(credential_id, permission) SELECT id, 'admin' FROM auth_credentials WHERE kind = 'token';
