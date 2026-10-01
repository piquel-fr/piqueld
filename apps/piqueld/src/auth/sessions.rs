//! Durable opaque sessions and short-lived, explicitly approved device logins.
use super::{Auth, AuthError, CredentialKind, DAY, MAX_PENDING, Result, now_secs};
use piqueld_core::auth::{DeviceRequest, DeviceStart, DeviceToken, User};
use std::collections::HashMap;

/// The authenticated caller and the credential (session, token, or CLI login)
/// that proved it.
#[derive(Clone)]
pub(crate) struct Identity {
    pub user: User,
    pub credential_id: String,
}
/// A pending CLI device login, following the OAuth device authorization flow.
pub(super) struct Device {
    /// Short code the user types in the dashboard, e.g. `ABCD-EF23`.
    user_code: String,
    /// Peer address that started the login; `None` for the Unix socket.
    requester: Option<std::net::IpAddr>,
    created: i64,
    expires: i64,
    /// Earliest time the CLI may poll again without receiving `slow_down`.
    pub(super) next_poll: i64,
    /// Credential ID of the approving session; the issued token belongs to its owner.
    approved_by: Option<String>,
}
impl Auth {
    /// Resolves a bearer or cookie secret to its live credential and owner.
    ///
    /// Secrets that are not 43 characters are rejected without a database lookup.
    /// The credential's last-used time is refreshed at most once per minute.
    pub(crate) async fn authenticate(&self, secret: &str) -> Result<Identity> {
        if secret.len() != 43 {
            return Err(AuthError::Unauthorized);
        }
        let owner = self
            .0
            .store
            .credential_owner(&Self::hash(secret))
            .await?
            .ok_or(AuthError::Unauthorized)?;
        // Keep ordinary requests read-only. A refresh queues with reconciliation
        // writers and rechecks validity after waiting, so revocation still wins.
        if owner.last_used_at <= now_secs() - 60
            && !self.0.store.touch_credential(&owner.credential_id).await?
        {
            return Err(AuthError::Unauthorized);
        }
        Ok(Identity {
            user: owner.user,
            credential_id: owner.credential_id,
        })
    }
    /// Revokes the credential used for the current request.
    pub(crate) async fn logout(&self, credential_id: &str) -> Result<()> {
        Ok(self.0.store.revoke_credential(credential_id).await?)
    }
    /// Starts a CLI device login valid for ten minutes.
    ///
    /// Returns a secret device code for the CLI to poll with and a unique,
    /// unambiguous user code (no `I`, `O`, `0`, or `1`) for the user to approve in
    /// the dashboard. Only the device code's hash is kept.
    pub(crate) async fn device_start(
        &self,
        requester: Option<std::net::IpAddr>,
    ) -> Result<DeviceStart> {
        let mut devices = self.0.devices.lock().await;
        devices.retain(|_, device| device.expires > now_secs());
        if devices.len() >= MAX_PENDING {
            return Err(AuthError::Busy);
        }
        let device_code = Self::secret()?;
        let user_code = loop {
            let mut bytes = [0_u8; 8];
            getrandom::fill(&mut bytes).map_err(|error| AuthError::Random(error.to_string()))?;
            let alphabet = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
            let mut code: String = bytes
                .iter()
                .map(|byte| char::from(alphabet[usize::from(*byte) % alphabet.len()]))
                .collect();
            code.insert(4, '-');
            if !devices.values().any(|device| device.user_code == code) {
                break code;
            }
        };
        let now = now_secs();
        devices.insert(
            Self::hash(&device_code),
            Device {
                user_code: user_code.clone(),
                requester,
                created: now,
                expires: now + 600,
                next_poll: 0,
                approved_by: None,
            },
        );
        Ok(DeviceStart {
            device_code,
            user_code,
            verification_uri: format!("{}/dashboard/auth#device", self.origin()),
            expires_in: 600,
            interval: 5,
            requester: requester.map(|address| address.to_string()),
        })
    }
    /// Finds a live, unapproved request by the code the user typed.
    fn pending_device<'a>(
        devices: &'a mut HashMap<String, Device>,
        code: &str,
    ) -> Result<&'a mut Device> {
        let normalized = code.trim().to_ascii_uppercase();
        let device = devices
            .values_mut()
            .find(|device| device.user_code == normalized && device.expires > now_secs())
            .ok_or(AuthError::Invalid("device code is invalid or expired"))?;
        if device.approved_by.is_some() {
            return Err(AuthError::Invalid("device code has already been approved"));
        }
        Ok(device)
    }
    /// Describes a pending request so the approver can check where it came from.
    pub(crate) async fn device_inspect(&self, code: &str) -> Result<DeviceRequest> {
        let mut devices = self.0.devices.lock().await;
        let device = Self::pending_device(&mut devices, code)?;
        let now = now_secs();
        Ok(DeviceRequest {
            user_code: device.user_code.clone(),
            requester: device.requester.map(|address| address.to_string()),
            age: u32::try_from(now - device.created).unwrap_or(0),
            expires_in: u32::try_from(device.expires - now).unwrap_or(0),
        })
    }
    /// Marks a pending device login as approved by the caller's credential.
    pub(crate) async fn device_approve(&self, code: &str, identity: &Identity) -> Result<()> {
        let mut devices = self.0.devices.lock().await;
        let device = Self::pending_device(&mut devices, code)?;
        device.approved_by = Some(identity.credential_id.clone());
        tracing::info!(
            user_id = %identity.user.id,
            username = %identity.user.username,
            requester = %device
                .requester
                .map_or_else(|| "unix-socket".to_owned(), |address| address.to_string()),
            "approved CLI device login"
        );
        Ok(())
    }
    /// Polls a device login by its secret device code.
    ///
    /// Returns `slow_down` when polled faster than every five seconds,
    /// `authorization_pending` until approved, and `complete` with a 30-day CLI
    /// token once approved. Completion removes the request so it cannot be reused;
    /// expired or unknown codes are unauthorized.
    pub(crate) async fn device_poll(&self, code: &str) -> Result<DeviceToken> {
        let mut devices = self.0.devices.lock().await;
        let key = Self::hash(code);
        let device = devices.get_mut(&key).ok_or(AuthError::Unauthorized)?;
        if device.expires <= now_secs() {
            devices.remove(&key);
            return Err(AuthError::Unauthorized);
        }
        let now = now_secs();
        if device.next_poll > now {
            return Ok(DeviceToken {
                status: "slow_down".into(),
                token: None,
                user: None,
            });
        }
        device.next_poll = now + 5;
        let Some(approved_by) = device.approved_by.clone() else {
            return Ok(DeviceToken {
                status: "authorization_pending".into(),
                token: None,
                user: None,
            });
        };
        let (token, credential) =
            Self::new_credential(CredentialKind::Cli, "piquelctl", Some(now + 30 * DAY))?;
        let user = self
            .0
            .store
            .issue_for_credential_owner(&approved_by, &credential)
            .await?
            .ok_or(AuthError::Unauthorized)?;
        devices.remove(&key);
        Ok(DeviceToken {
            status: "complete".into(),
            token: Some(token),
            user: Some(user),
        })
    }
}
