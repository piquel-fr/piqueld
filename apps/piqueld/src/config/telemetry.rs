//! Export of the daemon's own traces, metrics and logs over OTLP/gRPC.
use super::{ConfigError, Credential, CredentialError, CredentialFile};
use serde::Deserialize;
use std::path::PathBuf;

/// The `[telemetry]` table. Telemetry is off without an endpoint.
///
/// ```toml
/// [telemetry]
/// endpoint = "https://otel.example.com:4317" # or "http://…", "unix:///run/otel/otlp.sock"
/// headers_file = "otlp-headers"              # optional, one `name: value` per line
/// containers = true                          # container logs and resource usage
/// ```
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(try_from = "RawTelemetryConfig")]
pub struct TelemetryConfig {
    /// OTLP/gRPC collector receiving every signal.
    pub endpoint: Option<OtlpEndpoint>,
    /// gRPC metadata sent with every export, such as a collector token.
    pub headers: Option<Credential>,
    /// Whether the output and resource usage of every container piqueld
    /// runs are exported too.
    pub containers: bool,
}

impl Default for TelemetryConfig {
    fn default() -> Self {
        Self {
            endpoint: None,
            headers: None,
            containers: true,
        }
    }
}

/// TOML form of [`TelemetryConfig`], before its headers file is read.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTelemetryConfig {
    #[serde(default)]
    endpoint: Option<OtlpEndpoint>,
    #[serde(default)]
    headers_file: Option<CredentialFile>,
    #[serde(default = "enabled")]
    containers: bool,
}

/// Container export is on by default.
const fn enabled() -> bool {
    true
}

impl TryFrom<RawTelemetryConfig> for TelemetryConfig {
    type Error = CredentialError;
    fn try_from(raw: RawTelemetryConfig) -> Result<Self, Self::Error> {
        Ok(Self {
            endpoint: raw.endpoint,
            headers: raw
                .headers_file
                .map(|file| Credential::read("headers", file))
                .transpose()?,
            containers: raw.containers,
        })
    }
}

impl TelemetryConfig {
    /// Checks that headers come with an endpoint and parse as gRPC metadata.
    /// Headers in a file a structural check left unread are checked when the
    /// daemon starts.
    pub(super) fn validate(&self) -> Result<(), ConfigError> {
        let Some(headers) = &self.headers else {
            return Ok(());
        };
        if self.endpoint.is_none() {
            return Err(ConfigError::Invalid(
                "telemetry.headers_file requires telemetry.endpoint".into(),
            ));
        }
        if let Some(value) = headers.value() {
            crate::telemetry::Headers::parse(value)
                .map_err(|error| ConfigError::Invalid(error.to_string()))?;
        }
        Ok(())
    }
}

/// Where OTLP/gRPC is sent: a collector over TCP, with or without TLS, or a
/// Unix socket. Displays as configured; it never contains credentials.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(try_from = "String")]
pub enum OtlpEndpoint {
    /// `https://host:port` or `http://host:port`.
    Tcp(url::Url),
    /// `unix:///absolute/path`.
    Unix(PathBuf),
}

impl TryFrom<String> for OtlpEndpoint {
    type Error = String;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        if let Some(path) = value.strip_prefix("unix://") {
            let path = PathBuf::from(path);
            return if path.is_absolute() && path.file_name().is_some() {
                Ok(Self::Unix(path))
            } else {
                Err("a unix:// telemetry endpoint needs an absolute socket path".into())
            };
        }
        let url = url::Url::parse(&value)
            .map_err(|error| format!("invalid telemetry endpoint: {error}"))?;
        // gRPC ignores paths, and credentials belong in `headers_file`.
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || !matches!(url.path(), "" | "/")
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err("telemetry endpoint must be https://host[:port], http://host[:port], or unix:///path, without credentials or a path".into());
        }
        Ok(Self::Tcp(url))
    }
}

/// Shows `https://host:port` or `unix:///path`.
impl std::fmt::Display for OtlpEndpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Tcp(url) => f.write_str(url.as_str().trim_end_matches('/')),
            Self::Unix(path) => write!(f, "unix://{}", path.display()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoints_are_tcp_or_unix_without_credentials() {
        for (endpoint, accepted) in [
            ("https://otel.example.com:4317", true),
            ("http://collector:4317/", true),
            ("unix:///run/piqueld/telemetry/otlp.sock", true),
            ("unix://relative.sock", false),
            ("https://user:token@otel.example.com", false),
            ("https://otel.example.com/v1/traces", false),
            ("https://otel.example.com?token=x", false),
            ("ftp://otel.example.com", false),
            ("otel.example.com:4317", false),
        ] {
            let parsed = OtlpEndpoint::try_from(endpoint.to_owned());
            assert_eq!(parsed.is_ok(), accepted, "{endpoint}");
        }
        assert_eq!(
            OtlpEndpoint::try_from("http://collector:4317/".to_owned())
                .unwrap()
                .to_string(),
            "http://collector:4317"
        );
    }

    #[test]
    fn headers_come_from_a_file_and_need_an_endpoint() {
        let file = tempfile::NamedTempFile::new().unwrap();
        let path = file.path().display().to_string();
        let source = |endpoint: &str| format!("[telemetry]\n{endpoint}headers_file = '{path}'");
        let endpoint = "endpoint = 'unix:///run/otel/otlp.sock'\n";
        std::fs::write(
            file.path(),
            "authorization: Bearer secret\n\nx-scope-orgid: piqueld\n",
        )
        .unwrap();
        let config = crate::config::DaemonConfig::from_toml(&source(endpoint)).unwrap();
        assert_eq!(
            config.telemetry.headers.unwrap().to_string(),
            format!("from {path}")
        );
        assert!(crate::config::DaemonConfig::from_toml(&source("")).is_err());
        std::fs::write(file.path(), "authorization Bearer secret\n").unwrap();
        let error = crate::config::DaemonConfig::from_toml(&source(endpoint)).unwrap_err();
        assert!(!error.to_string().contains("secret"), "{error}");
    }
}
