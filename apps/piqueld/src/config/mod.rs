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
    /// Local API listeners and state directory.
    pub server: ServerConfig,
    /// Docker Engine connection and bootstrap policy.
    pub docker: DockerConfig,
    /// Reconciliation scheduling limits.
    pub reconciliation: ReconciliationConfig,
    /// Retention limits for terminal operation history.
    pub retention: RetentionConfig,
}

impl DaemonConfig {
    /// Returns the built-in configuration after applying the same validation
    /// used for file-backed configuration.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] if the built-in defaults violate a configuration
    /// invariant.
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
        absolute_directory("server.ui_dir", &self.server.ui_dir)?;
        absolute_file("docker.socket", &self.docker.socket)?;
            return Err(ConfigError::Invalid(
                "server.data_dir must name a directory".into(),
            ));
        }
        absolute_file("docker.socket", &self.docker.socket)?;
        if let Some(address) = self.server.http_listen {
            if address.port() == 0 {
                return Err(ConfigError::Invalid(
                    "server.http_listen port must be greater than zero".into(),
                ));
            }
            if !address.ip().is_loopback() {
                return Err(ConfigError::Invalid(
                    "server.http_listen must bind to a loopback address".into(),
                ));
            }
        }
        if self.reconciliation.scan_interval_seconds == 0
            || self.reconciliation.max_parallel_operations == 0
        {
            return Err(ConfigError::Invalid(
                "reconciliation interval and concurrency limit must be greater than zero".into(),
            ));
        }
        if self.reconciliation.prepare_timeout_seconds == 0
            || self.reconciliation.convergence_timeout_seconds == 0
        {
            return Err(ConfigError::Invalid(
                "reconciliation timeouts must be greater than zero".into(),
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

/// Local API listeners and the daemon state directory.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// The single private directory holding the socket, the database, and
    /// future user data.
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    /// Optional loopback HTTP listener. Omitting it disables TCP.
    #[serde(default)]
    pub http_listen: Option<SocketAddr>,
    /// Production dashboard asset directory.
    pub ui_dir: PathBuf,
}

fn default_data_dir() -> PathBuf {
    PathBuf::from("/var/lib/piqueld")
}

impl ServerConfig {
    /// Unix API socket path inside the data directory.
    #[must_use]
    pub fn socket_path(&self) -> PathBuf {
        self.data_dir.join("piqueld.sock")
    }

    /// Embedded database path inside the data directory.
    #[must_use]
    pub fn database_path(&self) -> PathBuf {
        self.data_dir.join("piqueld.db")
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            data_dir: PathBuf::from("/var/lib/piqueld"),
            http_listen: Some("127.0.0.1:7845".parse().expect("constant socket address")),
            ui_dir: PathBuf::from("/usr/share/piqueld/ui"),
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
    /// Outer budget for resolving one application's inputs before persistence.
    pub prepare_timeout_seconds: u64,
    /// Maximum time spent waiting for runtime convergence per operation.
    pub convergence_timeout_seconds: u64,
}

impl Default for ReconciliationConfig {
    fn default() -> Self {
        Self {
            scan_interval_seconds: 60,
            max_parallel_operations: 4,
            prepare_timeout_seconds: 300,
            convergence_timeout_seconds: 120,
        }
    }
}

/// Retention limits for terminal operation history.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct RetentionConfig {
    /// Days a finished operation is retained before pruning.
    /// `0` disables pruning.
    pub finished_operation_days: u64,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            finished_operation_days: 10,
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
