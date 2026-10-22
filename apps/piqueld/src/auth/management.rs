//! Account, passkey, credential, grant, and invitation administration.
//!
//! Every account may manage itself. Changing another account requires
//! `accounts:manage` and every grant that account holds, and grants can only
//! be handed out by someone holding them (see `Grants::may_change_account`).
//! The store re-reads both the caller's and the target's grants inside each
//! change's transaction.
use super::{Auth, AuthError, CredentialKind, DAY, Identity, Result, now_secs};
use crate::store::{DirectoryView, NewInvitation};
use piqueld_core::access::GlobalPermission;
use piqueld_core::auth::{Directory, HostOperator, Manage, Managed, OperatorLink};
impl Auth {
    /// Lists the accounts `viewer` may see: every account, invitation, and
    /// host operator session with `accounts:manage`, otherwise only its own
    /// account. A credential limited to its own grants sees only itself
    /// there, and its account with those grants, not the account's passkeys,
    /// other credentials, or wider grants.
    pub(crate) async fn directory(&self, viewer: &Identity) -> Result<Directory> {
        let view = if viewer.grants.has_global(GlobalPermission::AccountsManage) {
            DirectoryView::All
        } else if viewer.scoped {
            DirectoryView::Credential(viewer.caller()?)
        } else {
            DirectoryView::Account(viewer.caller()?.user_id)
        };
        let mut directory = self.0.store.auth_directory(view).await?;
        if let DirectoryView::Credential(_) = view {
            for account in &mut directory.users {
                account.grants.clone_from(&viewer.grants);
            }
        }
        Ok(directory)
    }
    /// Applies one account change on behalf of `actor` and logs it. The store
    /// refuses changes that would leave no administrator able to sign in.
    ///
    /// Invitation and enrollment links expire after a day and return their
    /// dashboard URL; API tokens return their secret once. Both secrets are
    /// only stored hashed. The host operator may make every change except
    /// those creating links or tokens, which need an issuing account.
    pub(crate) async fn manage(&self, actor: &Identity, command: Manage) -> Result<Managed> {
        let store = &self.0.store;
        let by = actor.actor();
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
                    .update_user(by, &user_id, &username, &display_name)
                    .await?;
            }
            Manage::DeleteUser { user_id } => store.delete_user(by, &user_id).await?,
            Manage::RemovePasskey { id } => store.remove_passkey(by, &id).await?,
            Manage::RenamePasskey { id, name } => {
                Self::validate_passkey_name(&name)?;
                store.rename_passkey(by, &id, &name).await?;
            }
            Manage::RevokeCredential { id } => store.revoke_credential(by, &id).await?,
            Manage::RevokeAll { user_id } => store.revoke_credentials(by, &user_id).await?,
            Manage::SetGrants { user_id, grants } => {
                store.set_user_grants(by, &user_id, &grants).await?;
            }
            Manage::CreateInvitation { grants } => {
                let (secret, invitation) = Self::new_invitation()?;
                store
                    .create_invitation(actor.caller()?, &invitation, &grants)
                    .await?;
                result.invitation_url = Some(self.link("invite", &secret));
            }
            Manage::CreateEnrollment { user_id } => {
                let (secret, invitation) = Self::new_invitation()?;
                store
                    .create_enrollment(actor.caller()?, &invitation, &user_id)
                    .await?;
                result.invitation_url = Some(self.link("enroll", &secret));
            }
            Manage::RevokeInvitation { id } => store.revoke_invitation(by, &id).await?,
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
                        "tailnet bindings need the daemon's tailnet node (tailscale.enabled) or a dashboard hostname (ingress.private.dashboard_hostname)",
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
                    .create_token(actor.caller()?, &credential, tailnet.as_ref())
                    .await?;
                result.token = Some(token);
            }
        }
        tracing::info!(
            actor = ?actor.principal(),
            action,
            "account management action applied"
        );
        Ok(result)
    }
    /// Issues the one-time admin recovery link for `operator`. It is valid
    /// for a day, replaces any earlier link, and registers a new account with
    /// `admin` on every application.
    pub(crate) async fn recover_admin(&self, operator: HostOperator) -> Result<OperatorLink> {
        if !self.0.store.auth_initialized().await? {
            return Err(AuthError::SetupPending);
        }
        let secret = Self::secret()?;
        let expires_at = now_secs() + DAY;
        self.0
            .store
            .create_recovery(&Self::hash(&secret), expires_at, operator)
            .await?;
        tracing::warn!(uid = operator.uid, "issued an admin recovery link");
        Ok(OperatorLink {
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
