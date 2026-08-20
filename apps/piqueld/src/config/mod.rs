//! Read-only host configuration for the single-node Docker Swarm daemon.

use serde::Deserialize;
use std::{
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
    /// Reconciliation scheduling limits.
    pub reconciliation: ReconciliationConfig,
}

impl DaemonConfig {
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

    /// Parses and validates a TOML host configuration document.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::Parse`] if the document is malformed or contains
    /// unknown fields. Returns a validation error if the configuration violates a
    /// host configuration invariant.
    ///
    /// # Examples
    ///
    /// ```
    /// let config = DaemonConfig::from_toml("").unwrap();
    /// ```
    pub fn from_toml(source: &str) -> Result<Self, ConfigError> {
    pub fn from_toml(source: &str) -> Result<Self, ConfigError> {
        let config: Self = toml::from_str(source).map_err(|_| ConfigError::Parse)?;
        config.validate()?;
        Ok(config)
    }

    /// Validates socket paths, the loopback HTTP listener, and reconciliation limits.
    ///
    /// # Examples
    ///
    /// ```
    /// let config = DaemonConfig::default();
    /// assert!(config.validate().is_ok());
    /// ```
    fn validate(&self) -> Result<(), ConfigError> {
        absolute_file("server.unix_socket", &self.server.unix_socket)?;
        absolute_file("database.path", &self.database.path)?;
        absolute_file("docker.socket", &self.docker.socket)?;
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
        {
            return Err(ConfigError::Invalid(
                "reconciliation interval and concurrency limit must be greater than zero".into(),
            ));
        }
        Ok(())
    }
}

/// Validates that a configuration directory path is absolute.
///
/// # Examples
///
/// ```
/// use std::path::Path;
///
/// assert!(absolute_directory("data directory", Path::new("/var/lib/app")).is_ok());
/// assert!(absolute_directory("data directory", Path::new("var/lib/app")).is_err());
/// ```
///
/// # Errors
///
/// Returns [`ConfigError::Invalid`] when `path` is relative.
///
/// # Arguments
///
/// * `name` - The configuration field name used in the error message.
/// * `path` - The directory path to validate.
fn absolute_directory(name: &str, path: &Path) -> Result<(), ConfigError> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(ConfigError::Invalid(format!(
            "{name} must be an absolute path"
        )))
    }
}

/// Validates that a path is absolute and names a file.
///
/// # Examples
///
/// ```
/// use std::path::Path;
///
/// assert!(absolute_file("config", Path::new("/etc/piqueld.toml")).is_ok());
/// assert!(absolute_file("config", Path::new("/etc")).is_err());
/// ```
///
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
    /// Creates the default database configuration using `/var/lib/piqueld/piqueld.db` as the database path.
    ///
    /// # Examples
    ///
    /// ```
    /// let config = DatabaseConfig::default();
    /// assert_eq!(config.path, std::path::PathBuf::from("/var/lib/piqueld/piqueld.db"));
    /// ```
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
    /// Provides the default Docker Engine socket and enables automatic Swarm initialization.
    ///
    /// # Examples
    ///
    /// ```
    /// let config = DockerConfig::default();
    /// assert_eq!(config.socket, std::path::PathBuf::from("/var/run/docker.sock"));
    /// assert!(config.auto_initialize_swarm);
    /// ```
    fn default() -> Self {
        Self {
            socket: PathBuf::from("/var/run/docker.sock"),
            auto_initialize_swarm: true,
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
}

impl Default for ReconciliationConfig {
    /// Provides the default reconciliation settings.
    ///
    /// # Examples
    ///
    /// ```
    /// let config = ReconciliationConfig::default();
    /// assert_eq!(config.scan_interval_seconds, 60);
    /// assert_eq!(config.max_parallel_operations, 4);
    /// ```
    ///
    /// # Returns
    ///
    /// A reconciliation configuration with a 60-second scan interval and a maximum of four parallel operations.
    fn default() -> Self {
        Self {
            scan_interval_seconds: 60,
            max_parallel_operations: 4,
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

/// Installs a JSON tracing subscriber using the `RUST_LOG` filter when available.
///
/// Invalid or absent `RUST_LOG` values use the `info` filter.
///
/// # Errors
///
/// Returns an error if the subscriber cannot be initialized, such as when a
/// global subscriber has already been installed.
///
/// # Examples
///
/// ```
/// let _ = init_tracing();
/// ```
pub fn init_tracing() -> Result<(), tracing_subscriber::util::TryInitError> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer().json())
        .try_init()
}

#[cfg(test)]
mod tests;
