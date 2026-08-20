//! Read-only host configuration for the single-node Docker Swarm daemon.

use serde::Deserialize;
use std::{
    fmt,
    net::SocketAddr,
    path::{Path, PathBuf},
};
use thiserror::Error;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

/// Complete host-specific daemon bootstrap configuration.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonConfig {
    /// Local API listeners.
    pub server: ServerConfig,
    /// Embedded database location.
    pub database: DatabaseConfig,
    /// Docker Engine connection and bootstrap policy.
    pub docker: DockerConfig,
    /// Local OCI registry endpoint used for built images.
    pub registry: RegistryConfig,
    /// Reconciliation scheduling limits.
    pub reconciliation: ReconciliationConfig,
    /// References to credentials kept outside the database and configuration.
    pub credentials: CredentialsConfig,
}

impl DaemonConfig {
    /// Returns the built-in configuration after applying the same validation
    /// used for file-backed configuration.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Invalid`] if a built-in default violates a
    /// configuration invariant.
    pub fn validated_default() -> Result<Self, ConfigError> {
        let config = Self::default();
        config.validate()?;
        Ok(config)
    }

    /// Reads, parses, and validates configuration without modifying its source.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when the file cannot be read or the configuration
    /// is malformed or invalid.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let source = std::fs::read_to_string(path).map_err(ConfigError::Read)?;
        Self::from_toml(&source)
    }

    /// Parses and validates a TOML configuration document.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when the document is malformed or violates a
    /// host configuration invariant.
    pub fn from_toml(source: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(source).map_err(|_| ConfigError::Parse)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        absolute_file("server.unix_socket", &self.server.unix_socket)?;
        if let Some(ui_dir) = &self.server.ui_dir {
            absolute_directory("server.ui_dir", ui_dir)?;
        }
        absolute_file("database.path", &self.database.path)?;
        absolute_file("docker.socket", &self.docker.socket)?;
        if self.registry.address.port() == 0 {
            return Err(ConfigError::Invalid(
                "registry.address port must be greater than zero".into(),
            ));
        }
        if !self.registry.address.ip().is_loopback() {
            return Err(ConfigError::Invalid(
                "registry.address must use a loopback address".into(),
            ));
        }
        absolute_directory("registry.data_dir", &self.registry.data_dir)?;
        if let Some(reference) = &self.credentials.encryption_key {
            reference.validate()?;
        }
        if let Some(reference) = &self.credentials.git_token {
            reference.validate()?;
        }
        if self.server.http_listen.port() == 0 {
            return Err(ConfigError::Invalid(
                "server.http_listen port must be greater than zero".into(),
            ));
        }
        if !self.server.http_listen.ip().is_loopback() {
            return Err(ConfigError::Invalid(
                "server.http_listen must bind to a loopback address".into(),
            ));
        }
        if self.reconciliation.scan_interval_seconds == 0
            || self.reconciliation.max_parallel_operations == 0
            || self.reconciliation.max_parallel_builds == 0
        {
            return Err(ConfigError::Invalid(
                "reconciliation interval and concurrency limit must be greater than zero".into(),
            ));
        }
        Ok(())
    }
}

fn absolute_directory(name: &str, path: &Path) -> Result<(), ConfigError> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(ConfigError::Invalid(format!(
            "{name} must be an absolute path"
        )))
    }
}

fn absolute_file(name: &str, path: &Path) -> Result<(), ConfigError> {
    absolute_directory(name, path)?;
    if path.file_name().is_some() {
        Ok(())
    } else {
        Err(ConfigError::Invalid(format!("{name} must name a file")))
    }
}

/// Local API listeners.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    /// Filesystem-protected Unix domain socket.
    pub unix_socket: PathBuf,
    /// Loopback HTTP listener.
    pub http_listen: SocketAddr,
    /// Optional dashboard asset directory override. When absent, the daemon
    /// uses the package-provided `PIQUELD_UI_DIR` wrapper value or its local
    /// default.
    pub ui_dir: Option<PathBuf>,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            unix_socket: PathBuf::from("/run/piqueld/piqueld.sock"),
            http_listen: "127.0.0.1:7845".parse().expect("constant socket address"),
            ui_dir: None,
        }
    }
}

/// Returns the dashboard asset directory used when configuration does not
/// specify `server.ui_dir`. The Nix package sets this through a transparent
/// wrapper, so operators never need to copy a store path into configuration.
#[must_use]
pub fn default_ui_dir() -> PathBuf {
    std::env::var_os("PIQUELD_UI_DIR")
        .map_or_else(|| PathBuf::from("/usr/share/piqueld/ui"), PathBuf::from)
}

/// Embedded database location.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct DatabaseConfig {
    /// Absolute embedded database file path.
    pub path: PathBuf,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            path: PathBuf::from("/var/lib/piqueld/piqueld.db"),
        }
    }
}

/// Docker Engine connection and bootstrap policy.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct DockerConfig {
    /// Absolute Docker Engine Unix socket path.
    pub socket: PathBuf,
    /// Whether an inactive Docker Engine should be initialized as a single-node Swarm.
    pub auto_initialize_swarm: bool,
}

impl Default for DockerConfig {
    fn default() -> Self {
        Self {
            socket: PathBuf::from("/var/run/docker.sock"),
            auto_initialize_swarm: true,
        }
    }
}

/// Local OCI registry configuration.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct RegistryConfig {
    /// Loopback registry endpoint used for built images.
    pub address: SocketAddr,
    /// Persistent directory reserved for registry data.
    pub data_dir: PathBuf,
}

impl Default for RegistryConfig {
    fn default() -> Self {
        Self {
            address: "127.0.0.1:5000".parse().expect("constant socket address"),
            data_dir: PathBuf::from("/var/lib/piqueld/registry"),
        }
    }
}

/// Reconciliation scheduling limits.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ReconciliationConfig {
    /// Period between full drift scans.
    pub scan_interval_seconds: u64,
    /// Global cap on concurrently mutating application operations.
    pub max_parallel_operations: usize,
    /// Global cap on concurrent image builds.
    pub max_parallel_builds: usize,
}

impl Default for ReconciliationConfig {
    fn default() -> Self {
        Self {
            scan_interval_seconds: 60,
            max_parallel_operations: 4,
            max_parallel_builds: 1,
        }
    }
}

/// References to credentials owned by systemd or a protected host file.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct CredentialsConfig {
    /// Optional external master-encryption-key reference.
    pub encryption_key: Option<CredentialReference>,
    /// Optional protected HTTPS Git bearer-token reference.
    pub git_token: Option<CredentialReference>,
}

/// A credential source that contains a reference, never inline secret material.
#[derive(Clone, Deserialize, Eq, PartialEq)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum CredentialReference {
    /// A protected absolute file outside the Nix store.
    File {
        /// Absolute path to the protected credential file.
        path: PathBuf,
    },
    /// A basename exposed in `$CREDENTIALS_DIRECTORY` by systemd.
    SystemdCredential {
        /// Systemd credential basename.
        name: String,
    },
}

impl CredentialReference {
    fn validate(&self) -> Result<(), ConfigError> {
        match self {
            Self::File { path } => {
                absolute_file("credentials.encryption_key.path", path)?;
                if path.starts_with("/nix/store") {
                    return Err(ConfigError::Invalid(
                        "credentials.encryption_key.path must be outside the Nix store".into(),
                    ));
                }
            }
            Self::SystemdCredential { name }
                if name.is_empty()
                    || name == "."
                    || name == ".."
                    || name.len() > 255
                    || name.contains(['/', '\\', '\0']) =>
            {
                return Err(ConfigError::Invalid(
                    "systemd credential name must be a non-empty basename".into(),
                ));
            }
            Self::SystemdCredential { .. } => {}
        }
        Ok(())
    }
}

impl fmt::Debug for CredentialReference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::File { .. } => formatter
                .debug_struct("File")
                .field("path", &"[REDACTED]")
                .finish(),
            Self::SystemdCredential { name } => formatter
                .debug_struct("SystemdCredential")
                .field("name", name)
                .finish(),
        }
    }
}

/// Configuration loading or validation failure.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// Reading the source file failed.
    #[error("could not read configuration")]
    Read(#[source] std::io::Error),
    /// TOML syntax or shape was invalid.
    #[error("configuration is not valid TOML")]
    Parse,
    /// A parsed setting violated a semantic invariant.
    #[error("configuration is invalid: {0}")]
    Invalid(String),
}

/// Installs structured JSON tracing, filtered by `RUST_LOG` when present.
///
/// # Errors
///
/// Invalid or absent `RUST_LOG` values fall back to the `info` filter. The
/// returned error is only from subscriber initialization, such as when another
/// global subscriber is already installed.
pub fn init_tracing() -> Result<(), tracing_subscriber::util::TryInitError> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer().json())
        .try_init()
}

#[cfg(test)]
mod tests;
