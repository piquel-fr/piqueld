-- An account's access is a set of grants. Each row grants one permission on one
-- application, or on every application when application_id is NULL. A row
-- belongs to exactly one account, credential, or invitation; deleting any of
-- them, or the application, removes its grants.
CREATE TABLE auth_grants (
    user_id TEXT REFERENCES auth_users(id) ON DELETE CASCADE,
    credential_id TEXT REFERENCES auth_credentials(id) ON DELETE CASCADE,
    invitation_id TEXT REFERENCES auth_invitations(id) ON DELETE CASCADE,
    permission TEXT NOT NULL,
    application_id TEXT REFERENCES applications(id) ON DELETE CASCADE,
    CHECK ((user_id IS NOT NULL) + (credential_id IS NOT NULL) + (invitation_id IS NOT NULL) = 1)
);
-- Holders are selected with `IS`, which only full indexes serve.
CREATE INDEX auth_grants_user ON auth_grants(user_id);
CREATE INDEX auth_grants_credential ON auth_grants(credential_id);
CREATE INDEX auth_grants_invitation ON auth_grants(invitation_id);
CREATE INDEX auth_grants_application ON auth_grants(application_id);
-- Existing accounts keep the unrestricted access they had before authorization.
INSERT INTO auth_grants(user_id, permission) SELECT id, 'admin' FROM auth_users;
-- With user_id, an invitation adds a passkey to that existing account instead
-- of creating a new one.
ALTER TABLE auth_invitations ADD COLUMN user_id TEXT REFERENCES auth_users(id) ON DELETE CASCADE;
