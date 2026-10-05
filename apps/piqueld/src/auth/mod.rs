//! Passkey ceremonies, revocable credentials, and account management. What
//! each account may do is decided by its grants (see `piqueld_core::access`).
mod ceremonies;
mod management;
mod sessions;
mod throttle;
pub use sessions::Identity;
pub(crate) use throttle::bucket as network;
#[cfg(test)]
mod tests;

use crate::store::{CredentialKind, NewCredential, now_secs};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use piqueld_core::auth::{AuthStatus, SetupLink};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, path::Path, sync::Arc, time::Duration};
use tokio::sync::Mutex;
use webauthn_rs_core::WebauthnCore;

/// One day in seconds, the unit for credential and invitation lifetimes.
const DAY: i64 = 86_400;
/// Seconds a started passkey ceremony stays redeemable.
const CEREMONY_LIFETIME: i64 = 300;
/// Capacity of each in-memory pending map (ceremonies and device logins).
const MAX_PENDING: usize = 1024;
/// Prefix of credential secrets issued since authorization was introduced.
const CREDENTIAL_PREFIX: &str = "pqd_";

/// Authentication failure with internal diagnostics retained for logging.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// Invalid or expired authentication proof.
    #[error("authentication is required or the proof has expired")]
    Unauthorized,
    /// Invalid input or an unavailable operation.
    #[error("{0}")]
    Invalid(&'static str),
    /// The caller may not perform the action.
    #[error(transparent)]
    Denied(#[from] piqueld_core::access::Denied),
    /// Bounded pending authentication capacity has been reached.
    #[error("too many pending authentication requests; try again shortly")]
    Busy,
    /// Authentication storage failed or refused the change.
    #[error("authentication storage failed")]
    Store(#[from] crate::store::StoreError),
    /// Stored data or challenge encoding failed.
    #[error("authentication encoding failed")]
    Encoding(#[from] serde_json::Error),
    /// Cryptographic authentication proof was rejected.
    #[error("passkey verification failed")]
    Webauthn(#[from] webauthn_rs_core::error::WebauthnError),
    /// First-account setup is closed, so no setup link exists.
    #[error("first-account setup is already completed")]
    SetupCompleted,
    /// First-account setup is still open, so there is no access to recover.
    #[error("first-account setup is not completed")]
    SetupPending,
    /// Operating-system entropy was unavailable.
    #[error("could not generate an authentication secret: {0}")]
    Random(String),
}
type Result<T> = std::result::Result<T, AuthError>;

/// Shared authentication service, using the control plane's `SQLite` database.
#[derive(Clone)]
pub struct Auth(Arc<Inner>);
/// Shared state behind [`Auth`]. Pending ceremonies and device logins live only
/// in memory, so a restart cancels them.
struct Inner {
    store: crate::store::Store,
    webauthn: WebauthnCore,
    /// Serialized public origin, e.g. `https://piqueld.example.com`.
    origin: String,
    /// Whether the origin is HTTPS, which enables `Secure` and `__Host-` cookies.
    secure: bool,
    /// The origin's explicit port, which names its cookies.
    port: Option<u16>,
    /// Pending passkey ceremonies keyed by their random ceremony ID.
    ceremonies: Mutex<HashMap<String, ceremonies::Pending>>,
    /// Pending CLI device logins keyed by the hash of their device code.
    devices: Mutex<HashMap<String, sessions::Device>>,
    throttle: Mutex<throttle::Throttle>,
    /// Link written by [`Auth::prepare_setup`] while the installation is unclaimed.
    setup_link: Mutex<Option<String>>,
    /// Longest API token lifetime in days (`auth.max_token_days`); `None`
    /// allows tokens that never expire.
    max_token_days: Option<u32>,
}

impl Auth {
    /// Initializes authentication and logs how to retrieve first-account setup.
    ///
    /// While no account exists, the setup link is written to `setup-link` in the
    /// data directory and served to `piquelctl setup-link` over the Unix socket.
    /// # Errors
    /// Returns configuration, database, or setup-file errors.
    pub async fn initialize(
        store: &crate::store::Store,
        config: &crate::config::DaemonConfig,
    ) -> anyhow::Result<Self> {
        let auth = Self::configured(store, config.public_url(), config.auth.max_token_days)?;
        let path = config.server.data_dir.join("setup-link");
        auth.prepare_setup(&path).await?;
        if auth.0.setup_link.lock().await.is_some() {
            tracing::info!(
                file = %path.display(),
                "first-account setup is open; run `piquelctl setup-link` to get its link"
            );
        }
        Ok(auth)
    }

    /// Constructs the service for one exact browser origin, without a token
    /// lifetime limit.
    /// # Errors
    /// Rejects origins that `WebAuthn` cannot safely use.
    pub fn new(store: &crate::store::Store, public_url: &str) -> Result<Self> {
        Self::configured(store, public_url, None)
    }

    /// Constructs the service for one exact browser origin, limiting API
    /// tokens to `max_token_days` when set.
    /// # Errors
    /// Rejects origins that `WebAuthn` cannot safely use.
    pub fn configured(
        store: &crate::store::Store,
        public_url: &str,
        max_token_days: Option<u32>,
    ) -> Result<Self> {
        let origin = Self::validate_origin(public_url)?;
        let host = origin
            .domain()
            .ok_or(AuthError::Invalid("public_url requires a DNS hostname"))?;
        let webauthn = WebauthnCore::new_unsafe_experts_only(
            "piqueld",
            host,
            vec![origin.clone()],
            Duration::from_mins(5),
            Some(false),
            Some(false),
        );
        Ok(Self(Arc::new(Inner {
            store: store.clone(),
            webauthn,
            origin: origin.origin().ascii_serialization(),
            secure: origin.scheme() == "https",
            port: origin.port(),
            ceremonies: Mutex::new(HashMap::new()),
            devices: Mutex::new(HashMap::new()),
            throttle: Mutex::new(throttle::Throttle::default()),
            setup_link: Mutex::new(None),
            max_token_days,
        })))
    }

    /// Validates the canonical origin used for `WebAuthn`, links, and CSRF checks.
    ///
    /// The URL must be a bare origin with a DNS hostname: no credentials, path,
    /// query, or fragment.
    ///
    /// ```text
    /// https://piqueld.example.com   accepted
    /// http://localhost:8080         accepted (development)
    /// https://10.0.0.1              rejected (no DNS hostname)
    /// https://example.com/piqueld   rejected (path)
    /// ```
    /// # Errors
    /// Requires HTTPS, except for HTTP localhost development.
    pub fn validate_origin(value: &str) -> Result<url::Url> {
        let url = url::Url::parse(value).map_err(|_| AuthError::Invalid("invalid public_url"))?;
        if url.domain().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
            || !(url.scheme() == "https"
                || (url.scheme() == "http" && url.domain() == Some("localhost")))
        {
            return Err(AuthError::Invalid(
                "public_url must be an HTTPS origin (or http://localhost for development)",
            ));
        }
        Ok(url)
    }

    /// Creates a private setup link on disk while the installation is unclaimed,
    /// and keeps it in memory for [`Auth::setup_link`]. Repeated startup
    /// preserves the secret of an existing valid link, rebuilding the link for
    /// the current origin; initialized daemons never reopen setup, even if the
    /// user table is manually emptied.
    /// # Errors
    /// Returns filesystem or database errors with their underlying cause.
    pub async fn prepare_setup(&self, path: &Path) -> anyhow::Result<()> {
        use anyhow::Context;
        use std::io::Write;
        if self.status().await?.initialized {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error).context("remove consumed setup link"),
            }
            return Ok(());
        }
        let existing = std::fs::read_to_string(path).unwrap_or_default();
        let preserved = match existing.trim().split_once("#invite=") {
            Some((_, secret)) if self.invitation(secret).await?.is_some() => Some(secret),
            _ => None,
        };
        let secret = match preserved {
            Some(secret) => secret.to_owned(),
            None => Self::secret()?,
        };
        let link = self.link("invite", &secret);
        let mut file = tempfile::NamedTempFile::new_in(
            path.parent()
                .context("setup file needs a parent directory")?,
        )?;
        writeln!(file, "{link}")?;
        file.as_file().sync_all()?;
        self.0.store.set_setup_secret(&Self::hash(&secret)).await?;
        file.persist(path).context("persist private setup link")?;
        *self.0.setup_link.lock().await = Some(link);
        Ok(())
    }

    /// Returns the first-account setup link while setup is open.
    /// # Errors
    /// Returns [`AuthError::SetupCompleted`] once the first account exists.
    pub(crate) async fn setup_link(&self) -> Result<SetupLink> {
        if self.0.store.auth_initialized().await? {
            return Err(AuthError::SetupCompleted);
        }
        self.0
            .setup_link
            .lock()
            .await
            .clone()
            .map(|url| SetupLink { url })
            .ok_or(AuthError::Invalid(
                "first-account setup link is unavailable",
            ))
    }

    /// Charges one public ceremony or device start against the peer's throttle
    /// window, failing with [`AuthError::Busy`] once exhausted.
    pub(crate) async fn admit_start(&self, peer: Option<std::net::IpAddr>) -> Result<()> {
        self.0
            .throttle
            .lock()
            .await
            .admit(peer, std::time::Instant::now())
    }
    /// Generates 32 random bytes as unpadded URL-safe base64 (43 characters).
    pub(crate) fn secret() -> Result<String> {
        let mut bytes = [0_u8; 32];
        getrandom::fill(&mut bytes).map_err(|error| AuthError::Random(error.to_string()))?;
        Ok(URL_SAFE_NO_PAD.encode(bytes))
    }
    /// Hashes a secret with SHA-256 for storage and lookup; raw secrets are never
    /// persisted.
    fn hash(value: &str) -> String {
        URL_SAFE_NO_PAD.encode(Sha256::digest(value.as_bytes()))
    }
    /// Generates a time-ordered UUID v7 for new records.
    fn id() -> String {
        uuid::Uuid::now_v7().to_string()
    }
    /// Generates a credential secret, returning it with the record to store.
    /// `grants` limits a scoped credential; `None` acts with the account's
    /// full access.
    ///
    /// Secrets carry the [`CREDENTIAL_PREFIX`] so leaked ones are easy to find
    /// with secret scanners:
    ///
    /// ```text
    /// pqd_q2C5Rk9…   (4 + 43 characters)
    /// ```
    fn new_credential<'a>(
        kind: CredentialKind,
        name: &'a str,
        expires_at: Option<i64>,
        grants: Option<&'a piqueld_core::access::Grants>,
    ) -> Result<(String, NewCredential<'a>)> {
        let secret = format!("{CREDENTIAL_PREFIX}{}", Self::secret()?);
        let credential = NewCredential {
            id: Self::id(),
            secret_hash: Self::hash(&secret),
            kind,
            name,
            expires_at,
            grants,
        };
        Ok((secret, credential))
    }
    pub(crate) fn origin(&self) -> &str {
        &self.0.origin
    }
    /// Returns the cookie name for this origin. HTTPS origins use the `__Host-`
    /// prefix so sibling subdomains, such as deployed applications, cannot set
    /// or shadow piqueld cookies. Browsers share cookies between the ports of a
    /// host, so an explicit port is part of the name: daemons served on several
    /// ports of one host keep separate sessions.
    ///
    /// ```text
    /// https://piqueld.example       __Host-piqueld_session
    /// https://piqueld.example:8443  __Host-piqueld_session_8443
    /// http://localhost:7845         piqueld_session_7845
    /// ```
    pub(crate) fn cookie_name(&self, name: &str) -> String {
        let prefix = if self.0.secure { "__Host-" } else { "" };
        match self.0.port {
            Some(port) => format!("{prefix}{name}_{port}"),
            None => format!("{prefix}{name}"),
        }
    }
    /// Builds a strict, `HttpOnly` `Set-Cookie` value that expires after `age` seconds.
    ///
    /// ```text
    /// __Host-piqueld_session=<secret>; Path=/; HttpOnly; SameSite=Strict; Max-Age=604800; Secure
    /// ```
    pub(crate) fn cookie(&self, name: &str, secret: &str, age: i64) -> String {
        format!(
            "{}={secret}; Path=/; HttpOnly; SameSite=Strict; Max-Age={age}{}",
            self.cookie_name(name),
            if self.0.secure { "; Secure" } else { "" }
        )
    }
    /// Reports whether the first account exists and which public origin is in use.
    pub(crate) async fn status(&self) -> Result<AuthStatus> {
        Ok(AuthStatus {
            initialized: self.0.store.auth_initialized().await?,
            public_url: self.0.origin.clone(),
        })
    }
    /// Enforces username charset and length limits and the display name byte limit.
    fn validate_profile(username: &str, display_name: &str) -> Result<()> {
        if username.is_empty()
            || username.len() > 64
            || !username
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
            || display_name.len() > 200
        {
            return Err(AuthError::Invalid(
                "username must contain 1–64 letters, digits, dots, dashes or underscores; display name must be at most 200 bytes",
            ));
        }
        Ok(())
    }
    /// Describes the live, unused setup secret or invitation matching `secret`.
    async fn invitation(&self, secret: &str) -> Result<Option<crate::store::Invitation>> {
        Ok(self.0.store.invitation(&Self::hash(secret)).await?)
    }
}
