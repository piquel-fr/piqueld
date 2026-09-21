//! Passkey ceremonies and revocable credentials. Account management deliberately
//! has no ownership checks: every authenticated account has equal capabilities.
mod ceremonies;
mod management;
mod sessions;
pub(crate) use sessions::Identity;
#[cfg(test)]
mod tests;

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use piqueld_core::auth::{AuthStatus, User};
use sha2::{Digest, Sha256};
use sqlx::{Row, SqlitePool};
use std::{
    collections::HashMap,
    path::Path,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::Mutex;
use webauthn_rs_core::WebauthnCore;

const DAY: i64 = 86_400;
const CEREMONY_LIFETIME: i64 = 300;
const MAX_PENDING: usize = 1024;

/// Authentication failure with internal diagnostics retained for logging.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    /// Invalid or expired authentication proof.
    #[error("authentication is required or the proof has expired")]
    Unauthorized,
    /// Invalid input or an unavailable operation.
    #[error("{0}")]
    Invalid(&'static str),
    /// Bounded pending authentication capacity has been reached.
    #[error("too many pending authentication requests; try again shortly")]
    Busy,
    /// Database operation failed.
    #[error("authentication storage failed")]
    Database(#[from] sqlx::Error),
    /// Stored data or challenge encoding failed.
    #[error("authentication encoding failed")]
    Encoding(#[from] serde_json::Error),
    /// Cryptographic authentication proof was rejected.
    #[error("passkey verification failed")]
    Webauthn(#[from] webauthn_rs_core::error::WebauthnError),
    /// Operating-system entropy was unavailable.
    #[error("could not generate an authentication secret: {0}")]
    Random(String),
}
type Result<T> = std::result::Result<T, AuthError>;

/// Shared authentication service, using the control plane's `SQLite` database.
#[derive(Clone)]
pub struct Auth(Arc<Inner>);
struct Inner {
    pool: SqlitePool,
    webauthn: WebauthnCore,
    origin: String,
    secure: bool,
    ceremonies: Mutex<HashMap<String, ceremonies::Pending>>,
    devices: Mutex<HashMap<String, sessions::Device>>,
}

impl Auth {
    /// Initializes authentication and reports where to retrieve first-account setup.
    /// # Errors
    /// Returns configuration, database, or setup-file errors.
    pub async fn initialize(
        store: &crate::store::Store,
        config: &crate::config::DaemonConfig,
    ) -> anyhow::Result<Self> {
        let auth = Self::new(store, &config.auth.public_url)?;
        let path = config.server.data_dir.join("setup-link");
        auth.prepare_setup(&path).await?;
        if path.exists() {
            tracing::info!(path = %path.display(), "first account setup link is available locally");
        }
        Ok(auth)
    }

    /// Constructs the service for one exact browser origin.
    /// # Errors
    /// Rejects origins that `WebAuthn` cannot safely use.
    pub fn new(store: &crate::store::Store, public_url: &str) -> Result<Self> {
        let origin = Self::validate_origin(public_url)?;
        let host = origin
            .domain()
            .ok_or(AuthError::Invalid("public_url requires a DNS hostname"))?;
        let webauthn = WebauthnCore::new_unsafe_experts_only(
            "piqueld",
            host,
            vec![origin.clone()],
            Duration::from_secs(300),
            Some(false),
            Some(false),
        );
        Ok(Self(Arc::new(Inner {
            pool: store.pool.clone(),
            webauthn,
            origin: origin.origin().ascii_serialization(),
            secure: origin.scheme() == "https",
            ceremonies: Mutex::new(HashMap::new()),
            devices: Mutex::new(HashMap::new()),
        })))
    }

    /// Validates the canonical origin used for `WebAuthn`, links, and CSRF checks.
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

    /// Creates a private setup link on disk while the installation is unclaimed.
    /// Repeated startup preserves an existing valid link; initialized daemons
    /// never reopen setup, even if the user table is manually emptied.
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
        if let Ok(existing) = std::fs::read_to_string(path)
            && let Some((_, secret)) = existing.trim().split_once("#invite=")
            && self.invitation_valid(secret).await?
        {
            return Ok(());
        }
        let secret = Self::secret()?;
        let mut file = tempfile::NamedTempFile::new_in(
            path.parent()
                .context("setup file needs a parent directory")?,
        )?;
        writeln!(file, "{}/dashboard/auth#invite={secret}", self.0.origin)?;
        file.as_file().sync_all()?;
        sqlx::query("UPDATE auth_setup SET secret_hash=? WHERE singleton=1 AND initialized=0")
            .bind(Self::hash(&secret))
            .execute(&self.0.pool)
            .await?;
        file.persist(path).context("persist private setup link")?;
        Ok(())
    }

    pub(crate) fn now() -> i64 {
        i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        )
        .unwrap_or(i64::MAX)
    }
    pub(crate) fn secret() -> Result<String> {
        let mut bytes = [0_u8; 32];
        getrandom::fill(&mut bytes).map_err(|error| AuthError::Random(error.to_string()))?;
        Ok(URL_SAFE_NO_PAD.encode(bytes))
    }
    fn hash(value: &str) -> String {
        URL_SAFE_NO_PAD.encode(Sha256::digest(value.as_bytes()))
    }
    fn id() -> String {
        uuid::Uuid::now_v7().to_string()
    }
    pub(crate) fn origin(&self) -> &str {
        &self.0.origin
    }
    pub(crate) fn cookie(&self, name: &str, secret: &str, age: i64) -> String {
        format!(
            "{name}={secret}; Path=/; HttpOnly; SameSite=Strict; Max-Age={age}{}",
            if self.0.secure { "; Secure" } else { "" }
        )
    }
    pub(crate) async fn status(&self) -> Result<AuthStatus> {
        let initialized: bool =
            sqlx::query_scalar("SELECT initialized FROM auth_setup WHERE singleton=1")
                .fetch_one(&self.0.pool)
                .await?;
        Ok(AuthStatus {
            initialized,
            public_url: self.0.origin.clone(),
        })
    }
    async fn user(&self, id: &str) -> Result<User> {
        let row = sqlx::query("SELECT id,username,display_name FROM auth_users WHERE id=?")
            .bind(id)
            .fetch_optional(&self.0.pool)
            .await?
            .ok_or(AuthError::Unauthorized)?;
        Ok(Self::user_row(&row))
    }
    fn user_row(row: &sqlx::sqlite::SqliteRow) -> User {
        User {
            id: row.get("id"),
            username: row.get("username"),
            display_name: row.get("display_name"),
        }
    }
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
    async fn invitation_valid(&self, secret: &str) -> Result<bool> {
        let hash = Self::hash(secret);
        Ok(sqlx::query_scalar::<_, bool>("SELECT EXISTS(SELECT 1 FROM auth_setup WHERE initialized=0 AND secret_hash=?) OR EXISTS(SELECT 1 FROM auth_invitations WHERE secret_hash=? AND expires_at>?)")
            .bind(&hash).bind(&hash).bind(Self::now()).fetch_one(&self.0.pool).await?)
    }
}
