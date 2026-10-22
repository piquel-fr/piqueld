//! Durable opaque sessions, host operator sessions, and short-lived,
//! explicitly approved device logins.
use super::{Auth, AuthError, CredentialKind, DAY, MAX_PENDING, Result, now_secs};
use crate::store::{Actor, Caller, NewCredential};
use piqueld_core::access::{Denied, Grants};
use piqueld_core::auth::{
    DeviceRequest, DeviceStart, DeviceToken, HostOperator, OperatorLink, Principal, Session, User,
};
use std::collections::{HashMap, hash_map::Entry};

/// Seconds a host operator sign-in link stays redeemable.
const OPERATOR_LINK_LIFETIME: i64 = 600;
/// Seconds between refreshes of a credential's last-used time.
const TOUCH_INTERVAL: i64 = 60;
/// Name of the sessions CLI device logins issue.
const CLI_SESSION: &str = "piquelctl";
/// Seconds a host operator browser session lasts.
pub(crate) const OPERATOR_SESSION_LIFETIME: i64 = 12 * 3600;

/// The authenticated caller, what proved it, and what it may do.
#[derive(Clone)]
pub struct Identity {
    /// Who is acting and how it authenticated.
    pub credential: Credential,
    /// Effective grants, read when the request was authenticated.
    pub grants: Grants,
    /// Whether the credential is limited to its own grants, like an API
    /// token; such credentials cannot create credentials.
    pub scoped: bool,
    /// Tailnet user or tag the credential is bound to, if any.
    pub tailnet: Option<piqueld_core::tailnet::TailnetBinding>,
}

/// What authenticated a request.
#[derive(Clone)]
pub enum Credential {
    /// One of an account's credentials: a session, CLI login, or token.
    Account {
        /// The account.
        user: User,
        /// Credential ID.
        id: String,
        /// Class of that credential.
        kind: CredentialKind,
    },
    /// The host operator: the kernel's peer credentials on a token-less Unix
    /// socket request, or a browser `session` opened with a sign-in link.
    Operator {
        /// Unix user acting.
        operator: HostOperator,
        /// Browser session ID, if any.
        session: Option<String>,
    },
}

impl Identity {
    /// The host operator with `admin` on every application, through a
    /// browser `session` or (without one) the Unix socket.
    #[must_use]
    pub fn operator(operator: HostOperator, session: Option<String>) -> Self {
        Self {
            credential: Credential::Operator { operator, session },
            grants: Grants::admin(),
            scoped: false,
            tailnet: None,
        }
    }
    /// Who is acting, as clients see it.
    #[must_use]
    pub fn principal(&self) -> Principal {
        match &self.credential {
            Credential::Account { user, .. } => Principal::User(user.clone()),
            Credential::Operator { operator, .. } => Principal::Operator(*operator),
        }
    }
    /// Who is signed in and what it may do, as `GET /auth/me` reports it.
    #[must_use]
    pub fn session(&self) -> Session {
        Session {
            principal: self.principal(),
            grants: self.grants.clone(),
            scoped: self.scoped,
        }
    }
    /// The signed-in account; `None` for the host operator.
    #[must_use]
    pub const fn user(&self) -> Option<&User> {
        match &self.credential {
            Credential::Account { user, .. } => Some(user),
            Credential::Operator { .. } => None,
        }
    }
    /// Who is acting, as history messages name it: the account's username
    /// or `host operator (uid N)`.
    #[must_use]
    pub fn who(&self) -> String {
        match &self.credential {
            Credential::Account { user, .. } => user.username.clone(),
            Credential::Operator { operator, .. } => operator.to_string(),
        }
    }
    /// The credential or host operator session that authenticated the
    /// request; `None` for the host operator over the Unix socket.
    #[must_use]
    pub fn credential_id(&self) -> Option<&str> {
        match &self.credential {
            Credential::Account { id, .. } => Some(id),
            Credential::Operator { session, .. } => session.as_deref(),
        }
    }
    /// The host operator acting over the Unix socket without any credential;
    /// `None` for accounts and operator browser sessions.
    #[must_use]
    pub const fn socket_operator(&self) -> Option<HostOperator> {
        match &self.credential {
            Credential::Operator {
                operator,
                session: None,
            } => Some(*operator),
            _ => None,
        }
    }
    /// Class of the credential: host operator sessions are browser sessions.
    #[must_use]
    pub const fn kind(&self) -> Option<CredentialKind> {
        match &self.credential {
            Credential::Account { kind, .. } => Some(*kind),
            Credential::Operator { session, .. } if session.is_some() => {
                Some(CredentialKind::Browser)
            }
            Credential::Operator { .. } => None,
        }
    }
    /// This caller, for changes that re-read its grants in their transaction.
    #[must_use]
    pub fn actor(&self) -> Actor<'_> {
        match &self.credential {
            Credential::Account { user, id, .. } => Actor::Account(Caller {
                credential_id: id,
                user_id: &user.id,
            }),
            Credential::Operator { operator, session } => Actor::Operator {
                operator: *operator,
                session: session.as_deref(),
            },
        }
    }
    /// This caller as an account, for actions that create something owned
    /// by or issued from an account.
    /// # Errors
    /// Returns [`AuthError::Invalid`] for the host operator.
    pub fn caller(&self) -> Result<Caller<'_>> {
        match self.actor() {
            Actor::Account(caller) => Ok(caller),
            _ => Err(AuthError::Invalid(
                "the host operator is not an account; sign in with an account to do this",
            )),
        }
    }
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
    /// Credential and account IDs of the approving session; the issued token
    /// belongs to its owner.
    approved_by: Option<(String, String)>,
    /// Grants the issued CLI session is limited to; `None` for the approver's
    /// full access.
    grants: Option<Grants>,
    /// The session to issue, prepared by the first poll after approval,
    /// which then tries to store it; set means that attempt began (see
    /// [`Auth::device_poll`]).
    pub(super) prepared: Option<Prepared>,
    /// Whether a poll is issuing the session (see [`Issuing`]).
    issuing: bool,
}

/// The CLI session an approved device login issues: its secret, credential
/// ID, and expiry, kept so a later poll can answer the same session when the
/// poll that stored it lost its answer.
#[derive(Clone)]
pub(super) struct Prepared {
    /// Bearer secret answered to the CLI.
    pub(super) token: String,
    /// Credential ID the session is stored under.
    id: String,
    /// Absolute expiry in Unix seconds.
    expires_at: i64,
}

impl Prepared {
    /// The credential to store for the session, limited to `grants`.
    pub(super) fn credential<'a>(&self, grants: Option<&'a Grants>) -> NewCredential<'a> {
        NewCredential {
            id: self.id.clone(),
            secret_hash: Auth::hash(&self.token),
            kind: CredentialKind::Cli,
            name: CLI_SESSION,
            expires_at: Some(self.expires_at),
            grants,
        }
    }
}

/// An approved device login whose session a poll is issuing, marked so no
/// concurrent poll issues or answers it too. Dropped unfinished, when the
/// poll fails or is cancelled, it makes the login pollable again.
struct Issuing<'a> {
    devices: &'a parking_lot::Mutex<HashMap<String, Device>>,
    /// The login's key; `None` once finished.
    key: Option<String>,
}

impl Issuing<'_> {
    /// Removes the login once its session was answered, so it is answered
    /// once, or refused.
    fn finish(mut self) {
        if let Some(key) = self.key.take() {
            self.devices.lock().remove(&key);
        }
    }
}

impl Drop for Issuing<'_> {
    fn drop(&mut self) {
        let Some(key) = &self.key else {
            return;
        };
        if let Some(device) = self.devices.lock().get_mut(key) {
            device.issuing = false;
        }
    }
}

impl Auth {
    /// Resolves a bearer or cookie secret to its live credential and owner,
    /// or to a host operator session.
    ///
    /// Secrets that are neither 43 characters (issued before the prefix) nor
    /// `pqd_` followed by 43 are rejected without a database lookup.
    /// An account credential's last-used time is refreshed at most once per
    /// minute.
    pub(crate) async fn authenticate(&self, secret: &str) -> Result<Identity> {
        let (identity, last_used_at) = self.identify(secret).await?;
        // Keep ordinary requests read-only. A refresh queues with reconciliation
        // writers and rechecks validity after waiting, so revocation still wins.
        if let Credential::Account { id, .. } = &identity.credential
            && last_used_at <= now_secs() - TOUCH_INTERVAL
            && self.claim_touch(id)
            && !self.0.store.touch_credential(id).await?
        {
            return Err(AuthError::Unauthorized);
        }
        Ok(identity)
    }
    /// Claims the refresh of credential `id`'s last-used time for the next
    /// [`TOUCH_INTERVAL`]. Concurrent requests that all read a stale time
    /// then leave the write to the first, rather than each queueing for the
    /// writer; `identify` has checked their credential either way. A claim
    /// whose refresh failed or was cancelled still holds for the interval,
    /// which is harmless: the next request after it refreshes, long before
    /// the 24-hour idle limit could end a browser session.
    fn claim_touch(&self, id: &str) -> bool {
        let now = now_secs();
        let mut touched = self.0.touched.lock();
        touched.retain(|_, at| *at > now - TOUCH_INTERVAL);
        if touched.contains_key(id) {
            return false;
        }
        touched.insert(id.to_owned(), now);
        true
    }
    /// Resolves a live credential's secret to its caller and when it was last
    /// used (now for host operator sessions, which have no idle timeout),
    /// without refreshing it. Refused requests use this directly, so they are
    /// attributed without keeping a session alive.
    pub(crate) async fn identify(&self, secret: &str) -> Result<(Identity, i64)> {
        let prefixed = secret.starts_with(super::CREDENTIAL_PREFIX)
            && secret.len() == super::CREDENTIAL_PREFIX.len() + 43;
        if secret.len() != 43 && !prefixed {
            return Err(AuthError::Unauthorized);
        }
        let hash = Self::hash(secret);
        let Some(owner) = self.0.store.credential_owner(&hash).await? else {
            let (session, operator) = self
                .0
                .store
                .operator_session(&hash)
                .await?
                .ok_or(AuthError::Unauthorized)?;
            return Ok((Identity::operator(operator, Some(session)), now_secs()));
        };
        let identity = Identity {
            credential: Credential::Account {
                user: owner.user,
                id: owner.credential_id,
                kind: owner.kind,
            },
            grants: owner.grants,
            scoped: owner.scoped,
            tailnet: owner.tailnet,
        };
        Ok((identity, owner.last_used_at))
    }
    /// Revokes the credential or host operator session used for the current
    /// request; the host operator over the Unix socket has none.
    pub(crate) async fn logout(&self, identity: &Identity) -> Result<()> {
        if let Some(id) = identity.credential_id() {
            self.0.store.revoke_credential(identity.actor(), id).await?;
        }
        Ok(())
    }
    /// Issues a one-time link that signs a browser in as `operator`, valid
    /// for ten minutes, and raises a security event. Only its secret's hash
    /// is stored.
    pub(crate) async fn sign_in_link(&self, operator: HostOperator) -> Result<OperatorLink> {
        let secret = Self::secret()?;
        let expires_at = now_secs() + OPERATOR_LINK_LIFETIME;
        self.0
            .store
            .create_operator_link(&Self::id(), &Self::hash(&secret), expires_at, operator)
            .await?;
        tracing::warn!(uid = operator.uid, "issued a host operator sign-in link");
        Ok(OperatorLink {
            url: self.link("operator", &secret),
            expires_at,
        })
    }
    /// Redeems a host operator sign-in link, returning the operator and the
    /// secret of its new browser session. Used, expired, and unknown links
    /// are unauthorized.
    pub(crate) async fn operator_sign_in(&self, secret: &str) -> Result<(HostOperator, String)> {
        let token = format!("{}{}", super::CREDENTIAL_PREFIX, Self::secret()?);
        let expires_at = now_secs() + OPERATOR_SESSION_LIFETIME;
        let (_, operator) = self
            .0
            .store
            .redeem_operator_link(&Self::hash(secret), &Self::hash(&token), expires_at)
            .await?
            .ok_or(AuthError::Unauthorized)?;
        tracing::info!(
            uid = operator.uid,
            "host operator signed in to the dashboard"
        );
        Ok((operator, token))
    }
    /// Starts a CLI device login valid for ten minutes.
    ///
    /// Returns a secret device code for the CLI to poll with and a unique,
    /// unambiguous user code (no `I`, `O`, `0`, or `1`) for the user to approve in
    /// the dashboard. Only the device code's hash is kept. With `grants`, the
    /// issued session is limited to them.
    pub(crate) fn device_start(
        &self,
        requester: Option<std::net::IpAddr>,
        grants: Option<Grants>,
    ) -> Result<DeviceStart> {
        let mut devices = self.0.devices.lock();
        devices.retain(|_, device| device.expires > now_secs());
        if devices.len() >= MAX_PENDING {
            return Err(AuthError::Busy);
        }
        let device_code = Self::secret()?;
        let user_code = loop {
            let bytes = Self::random::<8>()?;
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
                grants: grants.clone(),
                prepared: None,
                issuing: false,
            },
        );
        Ok(DeviceStart {
            device_code,
            user_code,
            verification_uri: format!("{}/dashboard/auth#device", self.origin()),
            expires_in: 600,
            interval: 5,
            requester: requester.map(|address| address.to_string()),
            grants,
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
    pub(crate) fn device_inspect(&self, code: &str) -> Result<DeviceRequest> {
        let mut devices = self.0.devices.lock();
        let device = Self::pending_device(&mut devices, code)?;
        let now = now_secs();
        Ok(DeviceRequest {
            user_code: device.user_code.clone(),
            requester: device.requester.map(|address| address.to_string()),
            age: u32::try_from(now - device.created).unwrap_or(0),
            expires_in: u32::try_from(device.expires - now).unwrap_or(0),
            grants: device.grants.clone(),
        })
    }
    /// Marks a pending device login as approved by the caller's credential,
    /// which must belong to an account, have its full access, and hold any
    /// requested grants. Both are checked again when the session is issued.
    pub(crate) fn device_approve(&self, code: &str, identity: &Identity) -> Result<()> {
        let caller = identity.caller()?;
        if identity.scoped {
            return Err(Denied::Scoped.into());
        }
        let mut devices = self.0.devices.lock();
        let device = Self::pending_device(&mut devices, code)?;
        if let Some(grants) = &device.grants {
            identity.grants.may_grant(grants)?;
        }
        device.approved_by = Some((caller.credential_id.to_owned(), caller.user_id.to_owned()));
        tracing::info!(
            user_id = %caller.user_id,
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
    ///
    /// Issuing does not hold the pending logins while storage writes: the
    /// login stays in place, marked as issuing, so a concurrent poll answers
    /// `authorization_pending`, and only the issuing poll completes it.
    ///
    /// The first poll after approval prepares the session (see [`Prepared`])
    /// and stores it. A poll that fails or is cancelled leaves the login to
    /// the next one, which stores nothing: it answers the prepared session
    /// if it is stored and live, e.g. committed just before the cancellation.
    /// Otherwise it cannot tell a session never stored from one revoked since,
    /// so it refuses the login (401) rather than resurrect a revoked one; the
    /// CLI then signs in again.
    pub(crate) async fn device_poll(&self, code: &str) -> Result<DeviceToken> {
        let pending = |status: &str| DeviceToken {
            status: status.into(),
            token: None,
            user: None,
        };
        let key = Self::hash(code);
        let (approver, grants, prepared, retry) = {
            let mut devices = self.0.devices.lock();
            let Entry::Occupied(mut entry) = devices.entry(key.clone()) else {
                return Err(AuthError::Unauthorized);
            };
            let now = now_secs();
            let device = entry.get_mut();
            if device.expires <= now {
                entry.remove();
                return Err(AuthError::Unauthorized);
            }
            if device.next_poll > now {
                return Ok(pending("slow_down"));
            }
            device.next_poll = now + 5;
            let Some(approver) = device.approved_by.clone().filter(|_| !device.issuing) else {
                return Ok(pending("authorization_pending"));
            };
            let retry = device.prepared.is_some();
            let prepared = match &device.prepared {
                Some(prepared) => prepared.clone(),
                None => device.prepared.insert(Self::prepare()?).clone(),
            };
            device.issuing = true;
            (approver, device.grants.clone(), prepared, retry)
        };
        let issuing = Issuing {
            devices: &self.0.devices,
            key: Some(key),
        };
        let user = if retry {
            let hash = Self::hash(&prepared.token);
            match self.0.store.credential_owner(&hash).await? {
                Some(owner) if owner.credential_id == prepared.id => owner.user,
                _ => {
                    issuing.finish();
                    return Err(AuthError::Unauthorized);
                }
            }
        } else {
            let (credential_id, user_id) = &approver;
            let approver = Caller {
                credential_id,
                user_id,
            };
            let credential = prepared.credential(grants.as_ref());
            let issued = self
                .0
                .store
                .issue_for_credential_owner(approver, &credential);
            issued.await?.ok_or(AuthError::Unauthorized)?
        };
        issuing.finish();
        Ok(DeviceToken {
            status: "complete".into(),
            token: Some(prepared.token),
            user: Some(user),
        })
    }
    /// Prepares the 30-day CLI session a device login issues.
    fn prepare() -> Result<Prepared> {
        let expires_at = now_secs() + 30 * DAY;
        let (token, credential) =
            Self::new_credential(CredentialKind::Cli, CLI_SESSION, Some(expires_at), None)?;
        Ok(Prepared {
            token,
            id: credential.id,
            expires_at,
        })
    }
}
