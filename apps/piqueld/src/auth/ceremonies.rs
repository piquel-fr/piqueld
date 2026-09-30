//! Fixed `WebAuthn` policy: discoverable credentials, required user verification,
//! exact origin/RP binding, and server-side single-use ceremony state.
use super::{
    Auth, AuthError, CEREMONY_LIFETIME, CredentialKind, DAY, MAX_PENDING, Result, now_secs,
};
use crate::store::{NewPasskey, PasskeyOwner};
use base64::Engine;
use piqueld_core::auth::{Ceremony, CeremonyFinish, RegistrationStart, User};
use webauthn_rs_core::proto::{
    AuthenticationState, Credential, PublicKeyCredential, RegistrationState, UserVerificationPolicy,
};

pub(super) struct Pending {
    pub(super) expires: i64,
    binding: String,
    kind: Kind,
}
enum Kind {
    Register {
        state: RegistrationState,
        user: User,
        invitation: Option<String>,
        name: String,
    },
    Login(AuthenticationState),
}

impl Auth {
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
    async fn consume(&self, id: &str, binding: &str) -> Result<Kind> {
        let mut pending = self.0.ceremonies.lock().await;
        let item = pending.get(id).ok_or(AuthError::Unauthorized)?;
        if item.expires <= now_secs() || item.binding != Self::hash(binding) {
            return Err(AuthError::Unauthorized);
        }
        Ok(pending.remove(id).ok_or(AuthError::Unauthorized)?.kind)
    }
    pub(crate) async fn registration_start(
        &self,
        input: RegistrationStart,
        binding: &str,
        authenticated: bool,
    ) -> Result<Ceremony> {
        if input.passkey_name.is_empty() || input.passkey_name.len() > 200 {
            return Err(AuthError::Invalid("passkey name must contain 1–200 bytes"));
        }
        let (user, invitation) = if let Some(id) = input.user_id {
            if !authenticated {
                return Err(AuthError::Unauthorized);
            }
            (self.user(&id).await?, None)
        } else {
            let secret = input.invitation.ok_or(AuthError::Unauthorized)?;
            if !self.invitation_valid(&secret).await? {
                return Err(AuthError::Unauthorized);
            }
            Self::validate_profile(&input.username, &input.display_name)?;
            (
                User {
                    id: Self::id(),
                    username: input.username,
                    display_name: input.display_name,
                },
                Some(Self::hash(&secret)),
            )
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
    pub(crate) async fn registration_finish(
        &self,
        input: CeremonyFinish,
        binding: &str,
        authenticated: bool,
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
        if invitation.is_none() && !authenticated {
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
        let is_new = invitation.is_some();
        let (owner, token) = match &invitation {
            Some(invitation_hash) => {
                let (token, session) = Self::browser_session()?;
                let owner = PasskeyOwner::New {
                    user: &user,
                    invitation_hash,
                    session,
                };
                (owner, Some(token))
            }
            None => (PasskeyOwner::Existing(&user.id), None),
        };
        if !self.0.store.add_passkey(owner, passkey).await? {
            return Err(AuthError::Unauthorized);
        }
        tracing::info!(
            user_id = %user.id,
            username = %user.username,
            new_account = is_new,
            "registered passkey"
        );
        Ok((user, token))
    }
    /// A week-long browser session, which also ends after a day without use.
    fn browser_session() -> Result<(String, crate::store::NewCredential<'static>)> {
        Self::new_credential(
            CredentialKind::Browser,
            "Browser",
            Some(now_secs() + 7 * DAY),
        )
    }
    pub(crate) async fn login_start(&self, binding: &str) -> Result<Ceremony> {
        let builder = self.0.webauthn.new_challenge_authenticate_builder(
            Vec::new(),
            Some(UserVerificationPolicy::Required),
        )?;
        let (options, state) = self.0.webauthn.generate_challenge_authenticate(builder)?;
        self.remember(binding, Kind::Login(state), serde_json::to_value(options)?)
            .await
    }
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
