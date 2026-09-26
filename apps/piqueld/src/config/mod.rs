//! Read-only host configuration for the single-node Docker Swarm daemon.

mod listeners;

use serde::Deserialize;
use std::path::{Path, PathBuf};
use thiserror::Error;
use tracing_subscriber::{EnvFilter, Layer, layer::SubscriberExt, util::SubscriberInitExt};

/// Complete host-specific daemon bootstrap configuration.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct DaemonConfig {
    /// API listeners and state directory.
    pub server: ServerConfig,
    /// Canonical browser origin for passkeys and invitation links.
    pub auth: AuthConfig,
    /// Docker Engine connection and bootstrap policy.
    pub docker: DockerConfig,
    /// Reconciliation scheduling limits.
    pub reconciliation: ReconciliationConfig,
    /// Retention limits for terminal operation history.
    pub retention: RetentionConfig,
    /// Durable build-output bounds.
    pub build_history: BuildHistoryConfig,
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
        let config: Self = toml::from_str(source).map_err(ConfigError::Parse)?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ConfigError> {
        for (name, path) in [
            ("server.data_dir", &self.server.data_dir),
            ("server.runtime_dir", &self.server.runtime_dir),
        ] {
            absolute_directory(name, path)?;
            if path.file_name().is_none() {
                return Err(ConfigError::Invalid(format!(
                    "{name} must name a directory"
                )));
            }
        }
        if self.server.data_dir == self.server.runtime_dir {
            return Err(ConfigError::Invalid(
                "server.data_dir and server.runtime_dir must be different directories".into(),
            ));
        }
        crate::auth::Auth::validate_origin(&self.auth.public_url)
            .map_err(|error| ConfigError::Invalid(error.to_string()))?;
        absolute_file("docker.socket", &self.docker.socket)?;
        if self.server.port == 0 {
            return Err(ConfigError::Invalid(
                "server.port must be greater than zero".into(),
            ));
        }
        if !(1..=86_400).contains(&self.reconciliation.scan_interval_seconds) {
            return Err(ConfigError::Invalid(
                "reconciliation interval must be 1..=86400 seconds".into(),
            ));
        }
        if !(1..=86_400).contains(&self.reconciliation.prepare_timeout_seconds)
            || !(1..=86_400).contains(&self.reconciliation.convergence_timeout_seconds)
        {
            return Err(ConfigError::Invalid(
                "reconciliation timeouts must be 1..=86400 seconds".into(),
            ));
        }
        if !(1..=64 * 1024 * 1024).contains(&self.build_history.log_max_bytes)
            || !(1..=3650).contains(&self.build_history.log_retention_days)
        {
            return Err(ConfigError::Invalid(
                "build logs require 1..=67108864 bytes and 1..=3650 retention days".into(),
            ));
        }
        Ok(())
    }
}

/// Canonical website origin. TLS is terminated by an external proxy.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    /// HTTPS origin, or HTTP localhost for development.
    pub public_url: String,
}
impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            public_url: "http://localhost:7845".into(),
        }
    }
}

/// Persistent build output policy; metadata remains until application deletion.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct BuildHistoryConfig {
    /// Maximum persisted bytes per build.
    pub log_max_bytes: u32,
    /// Days to retain output after completion.
    pub log_retention_days: u32,
}
impl Default for BuildHistoryConfig {
    fn default() -> Self {
        Self {
            log_max_bytes: 4 * 1024 * 1024,
            log_retention_days: 30,
        }
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

/// API listeners and the daemon state directory.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Private directory holding the database and future user data.
    #[serde(default = "default_data_dir")]
    pub data_dir: PathBuf,
    /// Existing directory holding the group-accessible Unix API socket.
    #[serde(default = "default_runtime_dir")]
    pub runtime_dir: PathBuf,
    /// Interfaces exposing the authenticated API over HTTP. Defaults to no TCP.
    #[serde(default)]
    pub listen_mode: ListenMode,
    /// Shared port for all selected TCP addresses.
    #[serde(default = "default_port")]
    pub port: u16,
}

/// Explicit TCP exposure policy; the Unix socket is always independent.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ListenMode {
    /// Unix socket only.
    #[default]
    Off,
    /// IPv4 and IPv6 loopback.
    Localhost,
    /// Addresses discovered from the local Tailscale daemon at startup.
    Tailscale,
    /// Loopback and Tailscale addresses.
    Both,
}

impl std::fmt::Display for ListenMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Off => "off",
            Self::Localhost => "localhost",
            Self::Tailscale => "tailscale",
            Self::Both => "both",
        })
    }
}

const fn default_port() -> u16 {
    7845
}

fn default_data_dir() -> PathBuf {
    PathBuf::from("/var/lib/piqueld")
}

fn default_runtime_dir() -> PathBuf {
    PathBuf::from("/run/piqueld")
}

impl ServerConfig {
    /// Unix API socket path inside the runtime directory.
    #[must_use]
    pub fn socket_path(&self) -> PathBuf {
        self.runtime_dir.join(crate::runtime_dir::SOCKET_NAME)
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
            data_dir: default_data_dir(),
            runtime_dir: default_runtime_dir(),
            listen_mode: ListenMode::default(),
            port: default_port(),
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
    /// Outer budget for resolving one application's inputs before persistence.
    pub prepare_timeout_seconds: u64,
    /// Maximum time spent waiting for runtime convergence per operation.
    pub convergence_timeout_seconds: u64,
}

impl Default for ReconciliationConfig {
    fn default() -> Self {
        Self {
            scan_interval_seconds: 60,
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
    /// Days informational events are retained; zero disables pruning.
    pub event_days: u64,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            finished_operation_days: 10,
            event_days: 30,
        }
    }
}

/// Configuration loading or validation failure.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// Reading the source file failed.
    #[error("could not read configuration")]
    Read(#[source] std::io::Error),
    /// The TOML document was syntactically malformed, had the wrong shape, or
    /// contained unknown keys.
    #[error("configuration is not valid TOML")]
    Parse(#[source] toml::de::Error),
    /// A parsed setting violated a semantic invariant.
    #[error("configuration is invalid: {0}")]
    Invalid(String),
}

/// Installs tracing filtered by `RUST_LOG` when present.
///
/// Interactive terminals receive plain-text events for readability; piped or
/// supervised runs (systemd, containers, log collectors) receive structured
/// JSON.
///
/// # Errors
///
/// Invalid or absent `RUST_LOG` values fall back to the `info` filter. The
/// returned error is only from subscriber initialization, such as when another
/// global subscriber is already installed.
pub fn init_tracing() -> Result<(), tracing_subscriber::util::TryInitError> {
    use std::io::IsTerminal as _;
    let layer = if std::io::stdout().is_terminal() {
        tracing_subscriber::fmt::layer().compact().boxed()
    } else {
        tracing_subscriber::fmt::layer().json().boxed()
    };
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(layer)
        .try_init()
}

#[cfg(test)]
mod tests;

impl DaemonConfig {
    /// Projects effective settings into the read-only public dashboard response.
    #[must_use]
    pub fn view(&self) -> piqueld_core::api::HostConfiguration {
        let groups = [
            (
                "Server",
                vec![
                    ("Data directory", self.server.data_dir.display().to_string()),
                    (
                        "Database",
                        self.server.database_path().display().to_string(),
                    ),
                    (
                        "API socket",
                        self.server.socket_path().display().to_string(),
                    ),
                    ("HTTP listen mode", self.server.listen_mode.to_string()),
                    ("HTTP port", self.server.port.to_string()),
                ],
            ),
            (
                "Docker",
                vec![
                    ("Socket", self.docker.socket.display().to_string()),
                    (
                        "Initialize Swarm",
                        self.docker.auto_initialize_swarm.to_string(),
                    ),
                ],
            ),
            (
                "Reconciliation",
                vec![
                    (
                        "Scan interval (seconds)",
                        self.reconciliation.scan_interval_seconds.to_string(),
                    ),
                    (
                        "Preparation timeout (seconds)",
                        self.reconciliation.prepare_timeout_seconds.to_string(),
                    ),
                    (
                        "Convergence timeout (seconds)",
                        self.reconciliation.convergence_timeout_seconds.to_string(),
                    ),
                ],
            ),
            (
                "Retention",
                vec![
                    ("Deployment history", "Until application deletion".into()),
                    (
                        "Other finished operations (days)",
                        self.retention.finished_operation_days.to_string(),
                    ),
                    ("Events (days)", self.retention.event_days.to_string()),
                    (
                        "Build output (days)",
                        self.build_history.log_retention_days.to_string(),
                    ),
                    (
                        "Build output limit (bytes)",
                        self.build_history.log_max_bytes.to_string(),
                    ),
                ],
            ),
        ]
        .into_iter()
        .map(|(group, values)| {
            (
                group.into(),
                values
                    .into_iter()
                    .map(|(key, value)| (key.into(), value))
                    .collect(),
            )
        })
        .collect();
        piqueld_core::api::HostConfiguration { groups }
    }
}
