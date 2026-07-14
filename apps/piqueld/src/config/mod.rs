//! Read-only host configuration loading and validation.

use serde::Deserialize;
use std::{
    fmt,
    net::SocketAddr,
    path::{Path, PathBuf},
};
use thiserror::Error;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

/// Complete host-specific daemon bootstrap configuration.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonConfig {
    /// Persistent control-plane state directory.
    pub data_dir: PathBuf,
    /// Local API listeners.
    pub server: ServerConfig,
    /// Embedded database location.
    pub database: DatabaseConfig,
    /// Docker Engine connection and bootstrap policy.
    pub docker: DockerConfig,
    /// Build-output registry settings.
    pub registry: RegistryConfig,
    /// Managed Traefik infrastructure settings.
    pub traefik: TraefikConfig,
    /// Reconciliation scheduling limits.
    pub reconciliation: ReconciliationConfig,
    /// Optional references to externally supplied credentials.
    pub credentials: CredentialsConfig,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("/var/lib/piqueld"),
            server: ServerConfig::default(),
            database: DatabaseConfig::default(),
            docker: DockerConfig::default(),
            registry: RegistryConfig::default(),
            traefik: TraefikConfig::default(),
            reconciliation: ReconciliationConfig::default(),
            credentials: CredentialsConfig::default(),
        }
    }
}

impl DaemonConfig {
    /// Reads, parses, and validates configuration without modifying its source.
    ///
    /// # Errors
    ///
    /// Returns an error when the file cannot be read, parsed, or validated.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let source = std::fs::read_to_string(path).map_err(ConfigError::Read)?;
        Self::from_toml(&source)
    }

    /// Parses and validates a TOML configuration document.
    ///
    /// # Errors
    ///
    /// Returns an error when the document has invalid TOML, unknown fields, or
    /// settings that violate a host-configuration invariant.
    pub fn from_toml(source: &str) -> Result<Self, ConfigError> {
        // Do not retain the parser error: its display output can quote source
        // lines, which could disclose a mistakenly inlined credential.
        let config: Self = toml::from_str(source).map_err(|_| ConfigError::Parse)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        absolute_directory("data_dir", &self.data_dir)?;
        absolute_file("server.unix_socket", &self.server.unix_socket)?;
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
        absolute_file("database.path", &self.database.path)?;
        absolute_file("docker.socket", &self.docker.socket)?;
        if self.registry.address.port() == 0 {
            return Err(ConfigError::Invalid(
                "registry.address port must be greater than zero".into(),
            ));
        }
        if !self.registry.address.ip().is_loopback() {
            return Err(ConfigError::Invalid(
                "registry.address must use a loopback address in the prototype".into(),
            ));
        }
        absolute_directory("registry.data_dir", &self.registry.data_dir)?;
        if self.traefik.origin_port == 0 {
            return Err(ConfigError::Invalid(
                "traefik.origin_port must be greater than zero".into(),
            ));
        }
        if self.reconciliation.scan_interval_seconds == 0
            || self.reconciliation.max_parallel_operations == 0
            || self.reconciliation.max_parallel_builds == 0
        {
            return Err(ConfigError::Invalid(
                "reconciliation intervals and concurrency limits must be greater than zero".into(),
            ));
        }
        if let Some(reference) = &self.credentials.encryption_key {
            reference.validate()?;
        }
        Ok(())
    }
}

fn absolute_directory(name: &str, path: &Path) -> Result<(), ConfigError> {
    if !path.is_absolute() {
        return Err(ConfigError::Invalid(format!(
            "{name} must be an absolute path"
        )));
    }
    Ok(())
}

fn absolute_file(name: &str, path: &Path) -> Result<(), ConfigError> {
    absolute_directory(name, path)?;
    if path.file_name().is_none() {
        return Err(ConfigError::Invalid(format!("{name} must name a file")));
    }
    Ok(())
}

/// Local API listeners.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ServerConfig {
    /// Filesystem-protected Unix domain socket.
    pub unix_socket: PathBuf,
    /// Loopback HTTP listener for a trusted private proxy.
    pub http_listen: SocketAddr,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            unix_socket: PathBuf::from("/run/piqueld/piqueld.sock"),
            http_listen: "127.0.0.1:7845".parse().expect("constant socket address"),
        }
    }
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
    /// Persistent registry blob directory.
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

/// Traefik infrastructure configuration.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct TraefikConfig {
    /// Whether piqueld should eventually ensure Traefik infrastructure.
    pub enabled: bool,
    /// Private origin port used by the external Cloudflare Tunnel.
    pub origin_port: u16,
}
impl Default for TraefikConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            origin_port: 8080,
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

/// References to credentials owned outside the configuration file.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct CredentialsConfig {
    /// Optional external master-encryption-key reference.
    pub encryption_key: Option<CredentialReference>,
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
    /// A credential exposed in `$CREDENTIALS_DIRECTORY` by systemd.
    SystemdCredential {
        /// Basename made available by systemd's credential directory.
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
                Ok(())
            }
            Self::SystemdCredential { name }
                if name.is_empty()
                    || name == "."
                    || name == ".."
                    || name.contains(['/', '\0'])
                    || name.len() > 255 =>
            {
                Err(ConfigError::Invalid(
                    "systemd credential name must be a non-empty basename".into(),
                ))
            }
            Self::SystemdCredential { .. } => Ok(()),
        }
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
/// Returns an error if a global tracing subscriber was already installed.
pub fn init_tracing() -> Result<(), tracing_subscriber::util::TryInitError> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer().json())
        .try_init()
}

#[cfg(test)]
mod tests;
