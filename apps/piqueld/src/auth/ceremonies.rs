//! Fixed `WebAuthn` policy: discoverable credentials, required user verification,
//! exact origin/RP binding, and server-side single-use ceremony state.
use super::{
    Auth, AuthError, CEREMONY_LIFETIME, CredentialKind, DAY, Identity, MAX_PENDING, Result,
    now_secs,
};
use crate::store::{Invitation, NewPasskey, PasskeyOwner};
use base64::Engine;
use piqueld_core::auth::{Ceremony, CeremonyFinish, RegistrationStart, User};
use webauthn_rs_core::proto::{
    AuthenticationState, Credential, PublicKeyCredential, RegistrationState, UserVerificationPolicy,
};

/// Server-side state of one started ceremony, redeemable once before `expires`.
pub(super) struct Pending {
    pub(super) expires: i64,
    /// Hash of the per-ceremony browser cookie secret, so only the browser that
    /// started the ceremony can finish it.
    binding: String,
    kind: Kind,
}
/// The ceremony-specific state needed to verify the browser's response.
enum Kind {
    /// Adds a passkey to the signed-in user, or redeems the setup secret or
    /// invitation whose hash `invitation` holds: creating `user`, or adding to
    /// it for enrollment links.
    Register {
        state: RegistrationState,
        user: User,
        invitation: Option<String>,
        /// Display name for the new passkey.
        name: String,
    },
    Login(AuthenticationState),
}

impl Auth {
    /// Stores a pending ceremony under a fresh random ID and returns the options
    /// the browser passes to `WebAuthn`. Expired entries are pruned first; fails
    /// with [`AuthError::Busy`] when capacity is still exhausted.
    async fn remember(
        &self,
        binding: &str,
        kind: Kind,
        options: serde_json::Value,
    ) -> Result<Ceremony> {
        let mut pending = self.0.ceremonies.lock().await;
        pending.retain(|_, item| item.expires > now_secs());
        if pending.len() >= MAX_PENDING {
            return Err(AuthError::Busy);
        }
        let id = Self::secret()?;
        pending.insert(
            id.clone(),
            Pending {
                expires: now_secs() + CEREMONY_LIFETIME,
                binding: Self::hash(binding),
                kind,
            },
        );
        Ok(Ceremony { id, options })
    }
    /// Removes and returns a pending ceremony if it is unexpired and bound to the
    /// caller's cookie. Mismatched or expired attempts leave the entry in place.
    async fn consume(&self, id: &str, binding: &str) -> Result<Kind> {
        let mut pending = self.0.ceremonies.lock().await;
        let item = pending.get(id).ok_or(AuthError::Unauthorized)?;
        if item.expires <= now_secs() || item.binding != Self::hash(binding) {
            return Err(AuthError::Unauthorized);
        }
        Ok(pending.remove(id).ok_or(AuthError::Unauthorized)?.kind)
    }
    /// Starts passkey registration for one of:
    ///
    /// - the signed-in `caller` adding a passkey to its own account (`user_id`);
    /// - a new account redeeming the setup secret or an invitation;
    /// - an existing account redeeming an enrollment link.
    ///
    /// Existing passkeys of the user are excluded so an authenticator cannot be
    /// registered twice. Resident keys and user verification are required.
    pub(crate) async fn registration_start(
        &self,
        input: RegistrationStart,
        binding: &str,
        caller: Option<&Identity>,
    ) -> Result<Ceremony> {
        if input.passkey_name.is_empty() || input.passkey_name.len() > 200 {
            return Err(AuthError::Invalid("passkey name must contain 1–200 bytes"));
        }
        let (user, invitation) = if let Some(id) = input.user_id {
            let caller = caller.ok_or(AuthError::Unauthorized)?;
            if caller.user.id != id {
                return Err(AuthError::Invalid(
                    "passkeys can only be added to your own account; create an enrollment link for others",
                ));
            }
            (caller.user.clone(), None)
        } else {
            let secret = input.invitation.ok_or(AuthError::Unauthorized)?;
            let user = match self.invitation(&secret).await? {
                None => return Err(AuthError::Unauthorized),
                Some(Invitation::Enrollment(user)) => user,
                Some(Invitation::Account) => {
                    Self::validate_profile(&input.username, &input.display_name)?;
                    User {
                        id: Self::id(),
                        username: input.username,
                        display_name: input.display_name,
                    }
                }
            };
            (user, Some(Self::hash(&secret)))
        };
        let excluded = self
            .0
            .store
            .passkey_credentials(&user.id)
            .await?
            .iter()
            .map(|value| serde_json::from_str::<Credential>(value).map(|c| c.cred_id))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let display = if user.display_name.is_empty() {
            &user.username
        } else {
            &user.display_name
        };
        let builder = self
            .0
            .webauthn
            .new_challenge_register_builder(user.id.as_bytes(), &user.username, display)?
            .require_resident_key(true)
            .user_verification_policy(UserVerificationPolicy::Required)
            .exclude_credentials(Some(excluded));
        let (options, state) = self.0.webauthn.generate_challenge_register(builder)?;
        self.remember(
            binding,
            Kind::Register {
                state,
                user,
                invitation,
                name: input.passkey_name,
            },
            serde_json::to_value(options)?,
        )
        .await
    }
    /// Verifies the registration response and stores the passkey.
    ///
    /// Redeeming a secret consumes it atomically with storing the passkey (and
    /// creating the account), and returns a browser session token. A signed-in
    /// account adding a passkey to itself gets no token. A store refusal (e.g.
    /// an invitation used concurrently) is reported as unauthorized.
    pub(crate) async fn registration_finish(
        &self,
        input: CeremonyFinish,
        binding: &str,
        caller: Option<&Identity>,
    ) -> Result<(User, Option<String>)> {
        let Kind::Register {
            state,
            user,
            invitation,
            name,
        } = self.consume(&input.id, binding).await?
        else {
            return Err(AuthError::Unauthorized);
        };
        if invitation.is_none() && caller.is_none_or(|caller| caller.user.id != user.id) {
            return Err(AuthError::Unauthorized);
        }
        let response = serde_json::from_value(input.credential)
            .map_err(|_| AuthError::Invalid("invalid passkey response"))?;
        let credential = self
            .0
            .webauthn
            .register_credential(&response, &state, None)?;
        let passkey = NewPasskey {
            id: &super::URL_SAFE_NO_PAD.encode(&credential.cred_id),
            name: &name,
            credential: &serde_json::to_string(&credential)?,
        };
        let redeemed = invitation.is_some();
        let (owner, token) = if let Some(invitation_hash) = &invitation {
            let (token, session) = Self::browser_session()?;
            let owner = PasskeyOwner::Redeem {
                user: &user,
                invitation_hash,
                session,
            };
            (owner, Some(token))
        } else {
            let caller = caller.ok_or(AuthError::Unauthorized)?.caller();
            (PasskeyOwner::Existing(caller, &user.id), None)
        };
        if !self.0.store.add_passkey(owner, passkey).await? {
            return Err(AuthError::Unauthorized);
        }
        tracing::info!(
            user_id = %user.id,
            username = %user.username,
            redeemed,
            "registered passkey"
        );
        Ok((user, token))
    }
    /// A week-long browser session, which also ends after a day without use.
    pub(super) fn browser_session() -> Result<(String, crate::store::NewCredential<'static>)> {
        Self::new_credential(
            CredentialKind::Browser,
            "Browser",
            Some(now_secs() + 7 * DAY),
        )
    }
    /// Starts a usernameless login: no allowed credentials are listed, so the
    /// authenticator offers any discoverable passkey for this relying party.
    pub(crate) async fn login_start(&self, binding: &str) -> Result<Ceremony> {
        let builder = self.0.webauthn.new_challenge_authenticate_builder(
            Vec::new(),
            Some(UserVerificationPolicy::Required),
        )?;
        let (options, state) = self.0.webauthn.generate_challenge_authenticate(builder)?;
        self.remember(binding, Kind::Login(state), serde_json::to_value(options)?)
            .await
    }
    /// Verifies a login assertion against the stored passkey identified by the
    /// response's user handle and credential ID, then opens a browser session.
    pub(crate) async fn login_finish(
        &self,
        input: CeremonyFinish,
        binding: &str,
    ) -> Result<(User, String)> {
        let Kind::Login(mut state) = self.consume(&input.id, binding).await? else {
            return Err(AuthError::Unauthorized);
        };
        let response: PublicKeyCredential = serde_json::from_value(input.credential)
            .map_err(|_| AuthError::Invalid("invalid passkey response"))?;
        let handle = response
            .get_user_unique_id()
            .ok_or(AuthError::Unauthorized)?;
        let user_id = std::str::from_utf8(handle).map_err(|_| AuthError::Unauthorized)?;
        let credential_id = super::URL_SAFE_NO_PAD.encode(response.get_credential_id());
        let (token, session) = Self::browser_session()?;
        // The store verifies the assertion and updates its signature counter
        // under the writer lock, then opens the session in the same transaction.
        let verify = |stored: &str| -> Result<String> {
            let mut credential: Credential = serde_json::from_str(stored)?;
            state.set_allowed_credentials(vec![credential.clone()]);
            let result = self.0.webauthn.authenticate_credential(&response, &state)?;
            credential.counter = result.counter();
            credential.backup_state = result.backup_state();
            credential.backup_eligible = result.backup_eligible();
            Ok(serde_json::to_string(&credential)?)
        };
        let user = self
            .0
            .store
            .sign_in_with_passkey(&credential_id, user_id, verify, &session)
            .await?
            .ok_or(AuthError::Unauthorized)?;
        tracing::info!(user_id = %user.id, username = %user.username, "signed in with passkey");
        Ok((user, token))
    }
}
