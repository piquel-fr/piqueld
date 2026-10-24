//! Read-only host configuration for the single-node Docker Swarm daemon.

mod credential;
mod listeners;
mod telemetry;
mod tunnel;
mod view;
use credential::Resolution;
pub use credential::{Credential, CredentialError, CredentialFile};
pub use telemetry::{OtlpEndpoint, TelemetryConfig};
pub use tunnel::{TunnelConfig, TunnelConfigError, TunnelCredentials};

use piqueld_core::TomlDiagnostic;
use piqueld_core::manifest::PreviewLimits;
use piqueld_core::names::{DNS_LABEL, is_label};
use serde::Deserialize;
use std::num::NonZeroU16;
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
    /// Dedicated tailnet node serving the website over HTTPS.
    pub tailscale: TailscaleConfig,
    /// Docker Engine connection and bootstrap policy.
    pub docker: DockerConfig,
    /// Installation-owned HTTP ingress. Read once at startup.
    pub ingress: IngressConfig,
    /// DNS provider accounts used for DNS-01 certificates.
    pub dns: DnsConfig,
    /// Reconciliation scheduling limits.
    pub reconciliation: ReconciliationConfig,
    /// Retention limits for terminal operation history.
    pub retention: RetentionConfig,
    /// Durable build-output bounds.
    pub build_history: BuildHistoryConfig,
    /// Retention and cleanup of the images this installation builds.
    pub images: ImagesConfig,
    /// Export of the daemon's traces, metrics and logs over OTLP.
    pub telemetry: TelemetryConfig,
    /// How many previews may exist and what their services may use.
    pub previews: PreviewLimits,
    /// Snapshots of local worktrees, uploaded by `preview deploy --local`.
    pub snapshots: SnapshotsConfig,
    /// The disk Git checkouts may take at once.
    pub checkouts: CheckoutsConfig,
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

    /// Checks a configuration file as [`Self::load`] would, without reading
    /// credential files or touching anything else on the host. This is
    /// `piqueld check-config`, which can run where the credentials are absent,
    /// such as a Nix build.
    ///
    /// Credential files are only located: relative paths are accepted without
    /// `$CREDENTIALS_DIRECTORY`, and checks of their contents (readable,
    /// non-empty, a tunnel credentials file, a valid webhook URL) wait for
    /// startup.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when the file cannot be read or the configuration
    /// is malformed or invalid.
    pub fn check(path: impl AsRef<Path>) -> Result<(), ConfigError> {
        Resolution::Structural.during(|| Self::load(path)).map(drop)
    }

    /// Parses and validates a TOML configuration document. An unset
    /// `auth.public_url` becomes the dashboard hostname's HTTPS origin.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when the document is malformed or violates a
    /// host configuration invariant.
    pub fn from_toml(source: &str) -> Result<Self, ConfigError> {
        let mut config: Self = toml::from_str(source)
            .map_err(|error| ConfigError::Parse(TomlDiagnostic::new(source, &error)))?;
        if config.auth.public_url.is_none() {
            config.auth.public_url = config
                .ingress
                .private
                .dashboard_hostname
                .as_ref()
                .map(|hostname| format!("https://{hostname}"));
        }
        config.validate()?;
        Ok(config)
    }

    /// Checks cross-field and range invariants that serde cannot express:
    /// non-zero ports, distinct absolute state directories,
    /// the public origin, the Docker socket path, `server.allowed_hosts` DNS
    /// hostname syntax, timeout and build-log bounds, and preview bounds a
    /// manifest could set itself.
    fn validate(&self) -> Result<(), ConfigError> {
        self.telemetry.validate()?;
        self.previews
            .validate()
            .map_err(|error| ConfigError::Invalid(error.to_string()))?;
        for (name, path) in [
            ("server.data_dir", &self.server.data_dir),
            ("server.runtime_dir", &self.server.runtime_dir),
        ] {
            absolute_path(name, path)?;
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
        if let Some(public_url) = &self.auth.public_url {
            crate::auth::Auth::validate_origin(public_url)
                .map_err(|error| ConfigError::Invalid(error.to_string()))?;
        }
        if self.auth.max_token_days == Some(0) {
            return Err(ConfigError::Invalid(
                "auth.max_token_days must be at least 1; omit it to allow tokens without expiry"
                    .into(),
            ));
        }
        self.tailscale.validate()?;
        self.ingress.validate()?;
        if self.tailscale.enabled
            && self.ingress.private.enabled
            && self.tailscale.hostname == self.ingress.private.hostname
        {
            return Err(ConfigError::Invalid(
                "ingress.private.hostname must differ from tailscale.hostname".into(),
            ));
        }
        absolute_file("docker.socket", &self.docker.socket)?;
        for host in &self.server.allowed_hosts {
            if host.len() > 253 || !host.split('.').all(|label| is_label(label, DNS_LABEL)) {
                return Err(ConfigError::Invalid(
                    "server.allowed_hosts must contain DNS hostnames without schemes, ports, or wildcards".into(),
                ));
            }
        }
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
        if !(1..=86_400).contains(&self.images.cleanup_interval_seconds) {
            return Err(ConfigError::Invalid(
                "images.cleanup_interval_seconds must be 1..=86400 seconds".into(),
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

/// Canonical website origin and credential limits.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    /// HTTPS origin, or HTTP localhost for development. Defaults to the
    /// dashboard hostname's HTTPS URL when one is set, then to the tailnet
    /// node's when it is enabled, otherwise localhost.
    pub public_url: Option<String>,
    /// Longest lifetime of new API tokens in days; unset allows tokens that
    /// never expire.
    pub max_token_days: Option<u32>,
}

impl DaemonConfig {
    /// Effective website origin. The dashboard hostname fills an unset value
    /// at load, and otherwise a tailnet node does when it joins, before
    /// authentication starts.
    #[must_use]
    pub fn public_url(&self) -> &str {
        self.auth
            .public_url
            .as_deref()
            .unwrap_or("http://localhost:7845")
    }
}

/// The daemon's own tailnet node, which terminates HTTPS for the website.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct TailscaleConfig {
    /// Join the tailnet and serve the website on `https_port` of the node.
    pub enabled: bool,
    /// Node name, which becomes `<hostname>.<tailnet>.ts.net`. Unused with
    /// `socket`, whose node already has a name.
    pub hostname: String,
    /// File holding an auth key for the first login. Without one, the daemon
    /// logs an interactive login URL. Later starts reuse the node state. It is
    /// passed to Tailscale by path, so piqueld never reads the key itself.
    pub auth_key_file: Option<CredentialFile>,
    /// Socket of a logged-in `tailscaled` to share instead of starting one.
    /// piqueld only adds `https_port` to its Serve configuration, so several
    /// daemons can share one node, each on its own port. That `tailscaled`
    /// must run as the daemon's user.
    pub socket: Option<PathBuf>,
    /// Node port serving the website over HTTPS.
    pub https_port: NonZeroU16,
}

impl Default for TailscaleConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            hostname: "piqueld".into(),
            auth_key_file: None,
            socket: None,
            https_port: const { NonZeroU16::new(443).unwrap() },
        }
    }
}

impl TailscaleConfig {
    fn validate(&self) -> Result<(), ConfigError> {
        dns_label("tailscale.hostname", &self.hostname)?;
        if let Some(socket) = &self.socket {
            absolute_file("tailscale.socket", socket)?;
            if self.auth_key_file.is_some() {
                return Err(ConfigError::Invalid(
                    "tailscale.auth_key_file is only used by piqueld's own tailscaled; \
                     log the shared tailscaled in instead"
                        .into(),
                ));
            }
        }
        Ok(())
    }
}

/// Requires a tailnet node name: a single DNS label.
fn dns_label(name: &str, value: &str) -> Result<(), ConfigError> {
    if !is_label(value, DNS_LABEL) {
        return Err(ConfigError::Invalid(format!(
            "{name} must be a single DNS label"
        )));
    }
    Ok(())
}

/// Explicit installation-wide ingress enablement.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct IngressConfig {
    /// Start the managed Caddy gateway and expose public routes on ports
    /// 80/443, or through `tunnel`.
    pub enabled: bool,
    /// This server's public IPv4 and IPv6 addresses. Public routes in zones
    /// whose provider sets `manage_records` get A/AAAA records to them;
    /// without any, those records stay manual. Unused in tunnel mode.
    pub public_addresses: Vec<std::net::IpAddr>,
    /// ACME account used for DNS-01 certificates.
    pub acme: AcmeConfig,
    /// The apps tailnet node, which serves private routes.
    pub private: PrivateIngressConfig,
    /// The Cloudflare Tunnel that replaces ports 80/443 for public routes.
    pub tunnel: TunnelConfig,
    /// Public hostname serving GitHub push webhooks for sync. The daemon
    /// serves them on a socket in the gateway's control mount, which the
    /// public listener or the tunnel forwards the webhook path to while
    /// ingress is enabled. No application may route the hostname or its
    /// subdomains.
    pub webhook_hostname: Option<piqueld_core::manifest::Hostname>,
}

impl IngressConfig {
    /// Checks the ACME account, the apps node's name, and that the dashboard
    /// hostname has private ingress to serve it and differs from the webhook
    /// hostname.
    fn validate(&self) -> Result<(), ConfigError> {
        self.acme.validate()?;
        dns_label("ingress.private.hostname", &self.private.hostname)?;
        if let Some(dashboard) = &self.private.dashboard_hostname {
            if !self.enabled || !self.private.enabled {
                return Err(ConfigError::Invalid(
                    "ingress.private.dashboard_hostname requires ingress.enabled and ingress.private.enabled".into(),
                ));
            }
            if self.webhook_hostname.as_ref() == Some(dashboard) {
                return Err(ConfigError::Invalid(
                    "ingress.private.dashboard_hostname must differ from ingress.webhook_hostname"
                        .into(),
                ));
            }
        }
        Ok(())
    }
}

/// The apps tailnet node: a Tailscale container that carries private routes'
/// traffic to the gateway's private listener. It is separate from the
/// daemon's own node (`[tailscale]`), with its own ACLs and tags.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(try_from = "RawPrivateIngressConfig")]
pub struct PrivateIngressConfig {
    /// Run the node and serve private routes on the tailnet. While disabled,
    /// private routes report `disabled`.
    pub enabled: bool,
    /// Node name, which becomes `<hostname>.<tailnet>.ts.net`.
    pub hostname: String,
    /// Auth key for the first login, read from `auth_key_file`. Without one,
    /// the node's login URL is relayed to the daemon logs.
    pub auth_key: Option<Credential>,
    /// Private hostname serving the website (dashboard and API) to the
    /// tailnet. The private listener forwards it to a socket in the gateway's
    /// control mount, which the daemon serves. An unset `auth.public_url`
    /// becomes its HTTPS origin. No application may route the hostname or its
    /// subdomains.
    pub dashboard_hostname: Option<piqueld_core::manifest::Hostname>,
}

impl Default for PrivateIngressConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            hostname: "piqueld-apps".into(),
            auth_key: None,
            dashboard_hostname: None,
        }
    }
}

/// `[ingress.private]` as written: the auth key is given only as a file.
#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawPrivateIngressConfig {
    enabled: bool,
    hostname: String,
    auth_key_file: Option<CredentialFile>,
    dashboard_hostname: Option<piqueld_core::manifest::Hostname>,
}

impl Default for RawPrivateIngressConfig {
    fn default() -> Self {
        let defaults = PrivateIngressConfig::default();
        Self {
            enabled: defaults.enabled,
            hostname: defaults.hostname,
            auth_key_file: None,
            dashboard_hostname: defaults.dashboard_hostname,
        }
    }
}

impl TryFrom<RawPrivateIngressConfig> for PrivateIngressConfig {
    type Error = CredentialError;
    fn try_from(raw: RawPrivateIngressConfig) -> Result<Self, Self::Error> {
        Ok(Self {
            enabled: raw.enabled,
            hostname: raw.hostname,
            auth_key: raw
                .auth_key_file
                .map(|file| Credential::read("auth_key", file))
                .transpose()?,
            dashboard_hostname: raw.dashboard_hostname,
        })
    }
}

/// The CA piqueld itself orders DNS-01 certificates from.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct AcmeConfig {
    /// ACME directory URL. Defaults to Let's Encrypt production.
    pub directory: String,
    /// Optional account contact address, sent only when the account is
    /// first registered with the CA.
    pub email: Option<String>,
}

impl Default for AcmeConfig {
    fn default() -> Self {
        Self {
            directory: "https://acme-v02.api.letsencrypt.org/directory".into(),
            email: None,
        }
    }
}

impl AcmeConfig {
    /// Requires an HTTPS directory URL and an email address without whitespace.
    fn validate(&self) -> Result<(), ConfigError> {
        if url::Url::parse(&self.directory).map_or(true, |url| url.scheme() != "https") {
            return Err(ConfigError::Invalid(
                "ingress.acme.directory must be an HTTPS URL".into(),
            ));
        }
        if self.email.as_ref().is_some_and(|email| {
            email
                .split_once('@')
                .is_none_or(|(user, domain)| user.is_empty() || domain.is_empty())
                || email.contains(char::is_whitespace)
        }) {
            return Err(ConfigError::Invalid(
                "ingress.acme.email must be an email address".into(),
            ));
        }
        Ok(())
    }
}

/// DNS provider accounts, used for DNS-01 certificates and, where
/// `manage_records` is set, routes' records. Zones are discovered through
/// each provider's API.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct DnsConfig {
    /// Providers in configuration order.
    pub providers: Vec<crate::dns::provider::DnsProviderConfig>,
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

/// Snapshots of local worktrees, uploaded by `preview deploy --local`. Kept
/// apart from `[previews]`, whose limits the API also reports.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct SnapshotsConfig {
    /// Largest Git bundle one upload may carry, in bytes.
    pub max_upload_bytes: std::num::NonZeroU64,
}

impl Default for SnapshotsConfig {
    fn default() -> Self {
        Self {
            max_upload_bytes: std::num::NonZeroU64::new(256 * 1024 * 1024)
                .expect("the default upload limit is not zero"),
        }
    }
}

/// The Git checkouts deployments read manifests from and build sources in.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct CheckoutsConfig {
    /// Most bytes the working trees of all checkouts may take at once. A
    /// checkout waits for room; a larger one, or a local snapshot that
    /// would be, is refused.
    pub max_bytes: std::num::NonZeroU64,
}

impl Default for CheckoutsConfig {
    fn default() -> Self {
        Self {
            max_bytes: std::num::NonZeroU64::new(4 * 1024 * 1024 * 1024)
                .expect("the default checkout budget is not zero"),
        }
    }
}

/// Retention and cleanup of the images this installation builds.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct ImagesConfig {
    /// Successful deployments per environment whose images are kept for
    /// restores, besides what environments and previews run or are deploying.
    pub keep_deployments: u32,
    /// Seconds between cleanups; cleanup also runs after each deployment.
    pub cleanup_interval_seconds: u64,
}
impl Default for ImagesConfig {
    fn default() -> Self {
        Self {
            keep_deployments: 3,
            cleanup_interval_seconds: 3600,
        }
    }
}

/// Rejects relative paths, naming the offending setting in the error.
fn absolute_path(name: &str, path: &Path) -> Result<(), ConfigError> {
    if path.is_absolute() {
        Ok(())
    } else {
        Err(ConfigError::Invalid(format!(
            "{name} must be an absolute path"
        )))
    }
}

/// Requires an absolute path that ends in a file name (not `/` or `..`).
fn absolute_file(name: &str, path: &Path) -> Result<(), ConfigError> {
    absolute_path(name, path)?;
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
    /// Trusted DNS hostnames for TCP requests, in addition to localhost and IP literals.
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
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

    /// Private tailnet node state inside the data directory, so the node
    /// identity (and the passkeys bound to its name) follows the data.
    #[must_use]
    pub fn tailscale_dir(&self) -> PathBuf {
        self.data_dir.join("tailscale")
    }
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            data_dir: default_data_dir(),
            runtime_dir: default_runtime_dir(),
            listen_mode: ListenMode::default(),
            port: default_port(),
            allowed_hosts: Vec::new(),
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
    /// Maximum time an operation waits without a service converging.
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
    /// Days daemon-scoped history is retained; zero disables pruning.
    pub daemon_event_days: u64,
    /// Days the audit trail is retained; zero disables pruning.
    pub audit_days: u64,
}

impl Default for RetentionConfig {
    fn default() -> Self {
        Self {
            finished_operation_days: 10,
            event_days: 90,
            daemon_event_days: 90,
            audit_days: 365,
        }
    }
}

/// Configuration loading or validation failure.
#[derive(Debug, Error)]
pub enum ConfigError {
    /// Reading the source file failed.
    #[error("could not read configuration")]
    Read(#[source] std::io::Error),
    /// The TOML document was syntactically malformed, had the wrong shape,
    /// contained unknown keys, or named an unreadable credential file. Never
    /// carries configuration source text.
    #[error("configuration could not be parsed")]
    Parse(#[source] TomlDiagnostic),
    /// A parsed setting violated a semantic invariant.
    #[error("configuration is invalid: {0}")]
    Invalid(String),
}

/// Installs tracing filtered by `RUST_LOG` when present.
///
/// With `telemetry` enabled, API request spans and log events are also
/// exported over OTLP.
///
/// Interactive terminals and the systemd journal receive plain text. The
/// journal gets it without colors or timestamps, which it records itself.
/// Other pipes (containers, log collectors) receive structured JSON. A
/// `log_file` additionally receives JSON, so tools can query the logs while a
/// person watches the terminal.
///
/// # Errors
///
/// Invalid or absent `RUST_LOG` values fall back to the `info` filter. The
/// returned error is only from subscriber initialization, such as when another
/// global subscriber is already installed.
pub fn init_tracing(
    log_file: Option<std::fs::File>,
    telemetry: &crate::telemetry::Telemetry,
) -> Result<(), tracing_subscriber::util::TryInitError> {
    use std::io::IsTerminal as _;
    let layer = if std::io::stdout().is_terminal() {
        tracing_subscriber::fmt::layer().compact().boxed()
    } else if std::env::var_os("JOURNAL_STREAM").is_some() {
        tracing_subscriber::fmt::layer()
            .compact()
            .with_ansi(false)
            .without_time()
            .boxed()
    } else {
        tracing_subscriber::fmt::layer().json().boxed()
    };
    tracing_subscriber::registry()
        .with(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .with(layer)
        .with(log_file.map(|file| {
            tracing_subscriber::fmt::layer()
                .json()
                .with_writer(std::sync::Arc::new(file))
        }))
        .with(telemetry.layer())
        .try_init()
}

#[cfg(test)]
mod tests;
