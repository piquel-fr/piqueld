//! Account, passkey, credential, and invitation administration.
use super::{Auth, AuthError, CredentialKind, DAY, Result, now_secs};
use piqueld_core::auth::{Directory, Manage, Managed};
impl Auth {
    /// Lists every user with their passkeys and credentials, plus open invitations.
    pub(crate) async fn directory(&self) -> Result<Directory> {
        Ok(self.0.store.auth_directory().await?)
    }
    /// Applies one account change on behalf of `actor` (a user ID) and logs it. The
    /// store refuses changes that would leave nobody able to sign in.
    ///
    /// Invitations expire after a day and return a dashboard invite URL; API tokens
    /// return their secret once. Both secrets are only stored hashed.
    pub(crate) async fn manage(&self, actor: &str, command: Manage) -> Result<Managed> {
        let store = &self.0.store;
        let mut result = Managed::default();
        let action = format!("{command:?}");
        match command {
            Manage::UpdateUser {
                user_id,
                username,
                display_name,
            } => {
                Self::validate_profile(&username, &display_name)?;
                store
                    .update_user(&user_id, &username, &display_name)
                    .await?;
            }
            Manage::DeleteUser { user_id } => store.delete_user(&user_id).await?,
            Manage::RemovePasskey { id } => store.remove_passkey(&id).await?,
            Manage::RenamePasskey { id, name } => {
                if name.is_empty() || name.len() > 200 {
                    return Err(AuthError::Invalid("passkey name must contain 1–200 bytes"));
                }
                store.rename_passkey(&id, &name).await?;
            }
            Manage::RevokeCredential { id } => store.revoke_credential(&id).await?,
            Manage::RevokeAll { user_id } => store.revoke_credentials(&user_id).await?,
            Manage::RevokeInvitation { id } => store.revoke_invitation(&id).await?,
            Manage::CreateInvitation => {
                let secret = Self::secret()?;
                store
                    .create_invitation(&Self::id(), actor, &Self::hash(&secret), now_secs() + DAY)
                    .await?;
                result.invitation_url =
                    Some(format!("{}/dashboard/auth#invite={secret}", self.origin()));
            }
            Manage::CreateToken {
                user_id,
                name,
                days,
            } => {
                if name.is_empty() || name.len() > 200 || days == Some(0) {
                    return Err(AuthError::Invalid(
                        "token requires a name and a positive lifetime (or no expiry)",
                    ));
                }
                let expires = days.map(|days| now_secs() + i64::from(days) * DAY);
                let (token, credential) =
                    Self::new_credential(CredentialKind::Token, &name, expires)?;
                store.insert_credential(&user_id, &credential).await?;
                result.token = Some(token);
            }
        }
        tracing::info!(actor, action, "account management action applied");
        Ok(result)
    }
}
