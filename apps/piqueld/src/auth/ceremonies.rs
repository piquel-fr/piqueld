//! Fixed `WebAuthn` policy: discoverable credentials, required user verification,
//! exact origin/RP binding, and server-side single-use ceremony state.
use super::{Auth, AuthError, CEREMONY_LIFETIME, MAX_PENDING, Result};
use base64::Engine;
use piqueld_core::auth::{Ceremony, CeremonyFinish, RegistrationStart, User};
use sqlx::Row;
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
        pending.retain(|_, item| item.expires > Self::now());
        if pending.len() >= MAX_PENDING {
            return Err(AuthError::Busy);
        }
        let id = Self::secret()?;
        pending.insert(
            id.clone(),
            Pending {
                expires: Self::now() + CEREMONY_LIFETIME,
                binding: Self::hash(binding),
                kind,
            },
        );
        Ok(Ceremony { id, options })
    }
    async fn consume(&self, id: &str, binding: &str) -> Result<Kind> {
        let mut pending = self.0.ceremonies.lock().await;
        let item = pending.get(id).ok_or(AuthError::Unauthorized)?;
        if item.expires <= Self::now() || item.binding != Self::hash(binding) {
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
        let existing: Vec<String> =
            sqlx::query_scalar("SELECT credential FROM auth_passkeys WHERE user_id=?")
                .bind(&user.id)
                .fetch_all(&self.0.pool)
                .await?;
        let excluded = existing
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
        let id = super::URL_SAFE_NO_PAD.encode(&credential.cred_id);
        let credential = serde_json::to_string(&credential)?;
        let mut tx = self.0.pool.begin_with("BEGIN IMMEDIATE").await?;
        let is_new = invitation.is_some();
        if let Some(hash) = invitation {
            let setup = sqlx::query("UPDATE auth_setup SET initialized=1, secret_hash=NULL WHERE initialized=0 AND secret_hash=?").bind(&hash).execute(&mut *tx).await?.rows_affected();
            if setup == 0 {
                let consumed = sqlx::query(
                    "DELETE FROM auth_invitations WHERE secret_hash=? AND expires_at>?",
                )
                .bind(&hash)
                .bind(Self::now())
                .execute(&mut *tx)
                .await?
                .rows_affected();
                if consumed != 1 {
                    return Err(AuthError::Unauthorized);
                }
            }
            sqlx::query(
                "INSERT INTO auth_users(id,username,display_name,created_at) VALUES(?,?,?,?)",
            )
            .bind(&user.id)
            .bind(&user.username)
            .bind(&user.display_name)
            .bind(Self::now())
            .execute(&mut *tx)
            .await?;
        }
        sqlx::query(
            "INSERT INTO auth_passkeys(id,user_id,name,credential,created_at) VALUES(?,?,?,?,?)",
        )
        .bind(id)
        .bind(&user.id)
        .bind(name)
        .bind(credential)
        .bind(Self::now())
        .execute(&mut *tx)
        .await?;
        let token = if is_new {
            Some(
                Self::issue(
                    &mut tx,
                    &user.id,
                    "browser",
                    "Browser",
                    Some(Self::now() + 7 * super::DAY),
                )
                .await?,
            )
        } else {
            None
        };
        tx.commit().await?;
        Ok((user, token))
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
        // Serializing credential lookup, verification, counter updates, and session
        // issuance prevents a concurrent deletion/replay from resurrecting access.
        let mut tx = self.0.pool.begin_with("BEGIN IMMEDIATE").await?;
        let row = sqlx::query("SELECT p.credential,u.id,u.username,u.display_name FROM auth_passkeys p JOIN auth_users u ON u.id=p.user_id WHERE p.id=? AND u.id=?")
            .bind(&credential_id).bind(user_id).fetch_optional(&mut *tx).await?.ok_or(AuthError::Unauthorized)?;
        let user = Self::user_row(&row);
        let mut credential: Credential = serde_json::from_str(row.get("credential"))?;
        state.set_allowed_credentials(vec![credential.clone()]);
        let result = self.0.webauthn.authenticate_credential(&response, &state)?;
        credential.counter = result.counter();
        credential.backup_state = result.backup_state();
        credential.backup_eligible = result.backup_eligible();
        sqlx::query("UPDATE auth_passkeys SET credential=? WHERE id=?")
            .bind(serde_json::to_string(&credential)?)
            .bind(&credential_id)
            .execute(&mut *tx)
            .await?;
        let token = Self::issue(
            &mut tx,
            user_id,
            "browser",
            "Browser",
            Some(Self::now() + 7 * super::DAY),
        )
        .await?;
        tx.commit().await?;
        Ok((user, token))
    }
}
