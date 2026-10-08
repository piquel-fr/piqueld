//! Account, passkey, credential, grant, and invitation administration.
//!
//! Every account may manage itself. Changing another account requires
//! `accounts:manage` and every grant that account holds, and grants can only
//! be handed out by someone holding them (see `Grants::may_change_account`).
//! The store re-reads both the caller's and the target's grants inside each
//! change's transaction.
use super::{Auth, AuthError, CredentialKind, DAY, Identity, Result, now_secs};
use crate::store::NewInvitation;
use piqueld_core::access::GlobalPermission;
use piqueld_core::auth::{Directory, Manage, Managed, RecoveryLink};
impl Auth {
    /// Lists the accounts `viewer` may see: every account and invitation with
    /// `accounts:manage`, otherwise only its own account.
    pub(crate) async fn directory(&self, viewer: &Identity) -> Result<Directory> {
        let mut directory = self.0.store.auth_directory().await?;
        if !viewer.grants.has_global(GlobalPermission::AccountsManage) {
            let own = |user_id: &String| *user_id == viewer.user.id;
            directory.users.retain(|account| own(&account.user.id));
            directory.passkeys.retain(|passkey| own(&passkey.user_id));
            directory
                .credentials
                .retain(|credential| own(&credential.user_id));
            directory.invitations.clear();
        }
        Ok(directory)
    }
    /// Applies one account change on behalf of `actor` and logs it. The store
    /// refuses changes that would leave no administrator able to sign in.
    ///
    /// Invitation and enrollment links expire after a day and return their
    /// dashboard URL; API tokens return their secret once. Both secrets are
    /// only stored hashed.
    pub(crate) async fn manage(&self, actor: &Identity, command: Manage) -> Result<Managed> {
        let store = &self.0.store;
        let caller = actor.caller();
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
                    .update_user(caller, &user_id, &username, &display_name)
                    .await?;
            }
            Manage::DeleteUser { user_id } => store.delete_user(caller, &user_id).await?,
            Manage::RemovePasskey { id } => store.remove_passkey(caller, &id).await?,
            Manage::RenamePasskey { id, name } => {
                if name.is_empty() || name.len() > 200 {
                    return Err(AuthError::Invalid("passkey name must contain 1–200 bytes"));
                }
                store.rename_passkey(caller, &id, &name).await?;
            }
            Manage::RevokeCredential { id } => store.revoke_credential(caller, &id).await?,
            Manage::RevokeAll { user_id } => store.revoke_credentials(caller, &user_id).await?,
            Manage::SetGrants { user_id, grants } => {
                store.set_user_grants(caller, &user_id, &grants).await?;
            }
            Manage::CreateInvitation { grants } => {
                let (secret, invitation) = Self::new_invitation()?;
                store
                    .create_invitation(caller, &invitation, &grants)
                    .await?;
                result.invitation_url = Some(self.link("invite", &secret));
            }
            Manage::CreateEnrollment { user_id } => {
                let (secret, invitation) = Self::new_invitation()?;
                store
                    .create_enrollment(caller, &invitation, &user_id)
                    .await?;
                result.invitation_url = Some(self.link("enroll", &secret));
            }
            Manage::RevokeInvitation { id } => store.revoke_invitation(caller, &id).await?,
            Manage::CreateToken {
                grants,
                name,
                days,
                tailnet,
            } => {
                if name.is_empty() || name.len() > 200 || days == Some(0) {
                    return Err(AuthError::Invalid(
                        "token requires a name and a positive lifetime (or no expiry)",
                    ));
                }
                if tailnet.is_some() && !self.0.tokens.tailnet {
                    return Err(AuthError::Invalid(
                        "tailnet bindings need the daemon's tailnet node (tailscale.enabled)",
                    ));
                }
                if let Some(limit) = self.0.tokens.max_days
                    && days.is_none_or(|days| days > limit)
                {
                    return Err(AuthError::Invalid(
                        "token lifetime exceeds this installation's auth.max_token_days",
                    ));
                }
                let expires = days.map(|days| now_secs() + i64::from(days) * DAY);
                let (token, credential) =
                    Self::new_credential(CredentialKind::Token, &name, expires, Some(&grants))?;
                store
                    .create_token(caller, &credential, tailnet.as_ref())
                    .await?;
                result.token = Some(token);
            }
        }
        tracing::info!(
            actor = %actor.user.id,
            action,
            "account management action applied"
        );
        Ok(result)
    }
    /// Issues the one-time admin recovery link for `requester`, a local user
    /// the caller has verified as root or the daemon's own user. It is valid
    /// for a day, replaces any earlier link, and registers a new account with
    /// `admin` on every application.
    pub(crate) async fn recover_admin(&self, requester: &str) -> Result<RecoveryLink> {
        if !self.0.store.auth_initialized().await? {
            return Err(AuthError::SetupPending);
        }
        let secret = Self::secret()?;
        let expires_at = now_secs() + DAY;
        self.0
            .store
            .create_recovery(&Self::hash(&secret), expires_at, requester)
            .await?;
        tracing::warn!(requester, "issued an admin recovery link");
        Ok(RecoveryLink {
            url: self.link("invite", &secret),
            expires_at,
        })
    }
    /// Generates a day-long invitation, returning its secret.
    fn new_invitation() -> Result<(String, NewInvitation)> {
        let secret = Self::secret()?;
        Ok((
            secret.clone(),
            NewInvitation {
                id: Self::id(),
                secret_hash: Self::hash(&secret),
                expires_at: now_secs() + DAY,
            },
        ))
    }
    /// Builds a dashboard link carrying `secret` in the `kind` fragment, e.g.
    /// `https://piqueld.example.com/dashboard/auth#invite=<secret>`.
    pub(super) fn link(&self, kind: &str, secret: &str) -> String {
        format!("{}/dashboard/auth#{kind}={secret}", self.origin())
    }
}
