//! Export of the daemon's own traces, metrics and logs over OTLP/gRPC.
use super::{ConfigError, Credential, CredentialError, CredentialFile, schema_from_raw};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use thiserror::Error;
use tonic::metadata::{AsciiMetadataKey, AsciiMetadataValue, MetadataMap};

/// The `[telemetry]` table. Telemetry is off without an endpoint. Its schema
/// is that of its TOML form, `RawTelemetryConfig`.
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

/// Export of the daemon's traces, metrics and logs
/// over OTLP/gRPC to a collector. Export is off until `endpoint` is set.
//
// The TOML form of [`TelemetryConfig`], before its headers file is read.
#[derive(Deserialize, utoipa::ToSchema)]
#[serde(default, deny_unknown_fields)]
struct RawTelemetryConfig {
    /// The OTLP/gRPC collector receiving every signal:
    /// `https://host[:port]`, `http://host[:port]` or `unix:///path` for a
    /// Unix socket, without credentials or a path.
    #[schema(value_type = Option<String>)]
    endpoint: Option<OtlpEndpoint>,
    /// gRPC metadata sent with every export, such as a collector token, one
    /// `name: value` per line. Requires `endpoint`.
    headers_file: Option<CredentialFile>,
    /// Also export the output and resource usage of every container piqueld
    /// runs.
    containers: bool,
}

impl Default for RawTelemetryConfig {
    fn default() -> Self {
        Self {
            endpoint: None,
            headers_file: None,
            containers: TelemetryConfig::default().containers,
        }
    }
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

schema_from_raw!(TelemetryConfig => RawTelemetryConfig);

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
            Headers::parse(value).map_err(|error| ConfigError::Invalid(error.to_string()))?;
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

/// As configured, like [`std::fmt::Display`].
impl Serialize for OtlpEndpoint {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
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

/// gRPC metadata sent with every export, parsed from `headers_file`: one
/// `name: value` per line, ignoring blank lines.
///
/// ```text
/// authorization: Bearer 0123456789
/// x-scope-orgid: piqueld
/// ```
#[derive(Clone, Default)]
pub struct Headers(MetadataMap);

/// Shows how many headers there are, never their values.
impl std::fmt::Debug for Headers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Headers({} redacted)", self.0.len())
    }
}

/// A line of a headers file is not valid ASCII gRPC metadata. Never contains
/// the value.
#[derive(Debug, Error)]
#[error("telemetry headers line {line} must be `name: value` with an ASCII name and value")]
pub struct HeadersError {
    /// One-based line number.
    line: usize,
}

impl Headers {
    /// Parses a headers file's contents.
    ///
    /// # Errors
    /// Every non-blank line must be an ASCII `name: value` pair; binary
    /// (`-bin`) metadata is not supported.
    pub fn parse(source: &str) -> Result<Self, HeadersError> {
        let mut map = MetadataMap::new();
        for (index, line) in source.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let error = || HeadersError { line: index + 1 };
            let (name, value) = line.split_once(':').ok_or_else(error)?;
            let name = AsciiMetadataKey::from_bytes(name.trim().as_bytes()).map_err(|_| error())?;
            let mut value = Some(value.trim())
                .filter(|value| value.is_ascii())
                .and_then(|value| AsciiMetadataValue::try_from(value).ok())
                .ok_or_else(error)?;
            value.set_sensitive(true);
            map.append(name, value);
        }
        Ok(Self(map))
    }

    /// The metadata to send with each export.
    #[must_use]
    pub fn metadata(&self) -> &MetadataMap {
        &self.0
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
        let config = crate::DaemonConfig::from_toml(&source(endpoint)).unwrap();
        assert_eq!(
            config.telemetry.headers.unwrap().to_string(),
            format!("from {path}")
        );
        assert!(crate::DaemonConfig::from_toml(&source("")).is_err());
        std::fs::write(file.path(), "authorization Bearer secret\n").unwrap();
        let error = crate::DaemonConfig::from_toml(&source(endpoint)).unwrap_err();
        assert!(!error.to_string().contains("secret"), "{error}");
    }

    #[test]
    fn headers_are_ascii_name_value_lines() {
        let headers =
            Headers::parse("Authorization: Bearer token\n\nx-scope-orgid:piqueld\n").unwrap();
        assert_eq!(headers.0.get("authorization").unwrap(), "Bearer token");
        assert_eq!(headers.0.get("x-scope-orgid").unwrap(), "piqueld");
        for invalid in [
            "no separator",
            "bad name: x",
            "key-bin: x",
            "key: caf\u{e9}",
        ] {
            assert!(Headers::parse(invalid).is_err(), "{invalid}");
        }
    }
}
