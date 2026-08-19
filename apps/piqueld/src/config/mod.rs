//! Read-only host configuration for the single-node Docker Swarm daemon.

use serde::Deserialize;
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
};
use thiserror::Error;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};

/// Complete host-specific daemon bootstrap configuration.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
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

impl Default for DaemonConfig {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            database: DatabaseConfig::default(),
            docker: DockerConfig::default(),
            reconciliation: ReconciliationConfig::default(),
        }
    }
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

/// Installs structured JSON tracing, filtered by `RUST_LOG` when present.
///
/// # Errors
///
/// Returns the subscriber initialization error if another global subscriber is
/// already installed or the configured filter cannot be initialized.
pub fn init_tracing() -> Result<(), tracing_subscriber::util::TryInitError> {
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(tracing_subscriber::fmt::layer().json())
        .try_init()
}

#[cfg(test)]
mod tests;
