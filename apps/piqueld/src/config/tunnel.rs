//! `[ingress.tunnel]`: serves public routes through a locally managed
//! Cloudflare Tunnel instead of the host's ports 80 and 443.
//!
//! ```toml
//! [ingress.tunnel]
//! enabled = true
//! credentials_file = "cloudflared-tunnel.json"   # from `cloudflared tunnel create piqueld`
//! ```
//!
//! The credentials file is read once at startup, like other `_file` settings,
//! and must hold the tunnel's `AccountTag`, `TunnelSecret` and `TunnelID`.

use super::{Credential, CredentialError, CredentialFile};
use serde::Deserialize;
use std::path::PathBuf;
use thiserror::Error;
use uuid::Uuid;

/// The Cloudflare Tunnel public routes arrive through.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(try_from = "RawTunnelConfig")]
pub struct TunnelConfig {
    /// The tunnel's credentials while it is enabled, `None` while disabled.
    /// Disabled, public routes are served on ports 80 and 443.
    pub credentials: Option<TunnelCredentials>,
}

/// A tunnel's credentials file, as `cloudflared tunnel create` writes it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TunnelCredentials {
    /// The file's `TunnelID`.
    pub id: Uuid,
    /// The whole file, which `cloudflared` reads. Never log or display it.
    pub file: Credential,
}

impl TunnelCredentials {
    /// The tunnel's DNS name, which public hostnames need a proxied CNAME to.
    ///
    /// ```text
    /// 6ff42ae2-765d-4adf-8112-31c55c1551ef.cfargotunnel.com
    /// ```
    #[must_use]
    pub fn hostname(&self) -> String {
        format!("{}.cfargotunnel.com", self.id)
    }

    /// Reads and checks the credentials file.
    fn read(file: CredentialFile) -> Result<Self, TunnelConfigError> {
        /// The fields `cloudflared` requires. Secrets are only checked for
        /// presence, so decoding errors never quote them.
        #[derive(Deserialize)]
        #[expect(dead_code, reason = "fields are only checked for presence")]
        struct Fields {
            #[serde(rename = "AccountTag")]
            account: serde::de::IgnoredAny,
            #[serde(rename = "TunnelSecret")]
            secret: serde::de::IgnoredAny,
            #[serde(rename = "TunnelID")]
            id: String,
        }
        let path = file.path().to_owned();
        let file = Credential::read("credentials", file)?;
        let fields: Fields = serde_json::from_str(file.expose())
            .map_err(|error| TunnelConfigError::Decode { path, error })?;
        let id =
            Uuid::parse_str(&fields.id).map_err(|_| TunnelConfigError::Id { id: fields.id })?;
        Ok(Self { id, file })
    }
}

/// `[ingress.tunnel]` as written.
#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RawTunnelConfig {
    enabled: bool,
    credentials_file: Option<CredentialFile>,
}

impl TryFrom<RawTunnelConfig> for TunnelConfig {
    type Error = TunnelConfigError;
    fn try_from(raw: RawTunnelConfig) -> Result<Self, Self::Error> {
        if !raw.enabled {
            return Ok(Self::default());
        }
        let file = raw
            .credentials_file
            .ok_or(TunnelConfigError::MissingCredentials)?;
        Ok(Self {
            credentials: Some(TunnelCredentials::read(file)?),
        })
    }
}

/// `[ingress.tunnel]` could not be loaded. Messages never contain the secret.
#[derive(Debug, Error)]
pub enum TunnelConfigError {
    /// The tunnel is enabled without credentials.
    #[error("ingress.tunnel.credentials_file is required while the tunnel is enabled")]
    MissingCredentials,
    /// The credentials file could not be read.
    #[error(transparent)]
    Credential(#[from] CredentialError),
    /// The file is not a tunnel credentials file, such as `cert.pem`.
    #[error("{} is not a cloudflared tunnel credentials file: {error}", path.display())]
    Decode {
        /// Resolved path.
        path: PathBuf,
        /// What is missing or malformed.
        error: serde_json::Error,
    },
    /// `TunnelID` is not a UUID.
    #[error("the tunnel credentials' TunnelID {id:?} is not a UUID")]
    Id {
        /// The configured ID, which is not secret.
        id: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DaemonConfig;

    const ID: &str = "6ff42ae2-765d-4adf-8112-31c55c1551ef";

    /// Parses `[ingress.tunnel]` with `credentials` in its credentials file.
    fn load(credentials: &str) -> Result<TunnelConfig, String> {
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(file.path(), credentials).unwrap();
        let document = format!(
            "[ingress.tunnel]\nenabled = true\ncredentials_file = '{}'",
            file.path().display()
        );
        DaemonConfig::from_toml(&document)
            .map(|config| config.ingress.tunnel)
            .map_err(|error| format!("{:#}", anyhow::Error::new(error)))
    }

    #[test]
    fn credentials_are_read_once_enabled() {
        let tunnel = load(&format!(
            r#"{{"AccountTag":"account","TunnelSecret":"c2VjcmV0","TunnelID":"{ID}"}}"#
        ))
        .unwrap();
        let credentials = tunnel.credentials.unwrap();
        assert_eq!(credentials.id.to_string(), ID);
        assert_eq!(credentials.hostname(), format!("{ID}.cfargotunnel.com"));
        // Disabled, the file is not read at all.
        let disabled = DaemonConfig::from_toml(
            "[ingress.tunnel]\nenabled = false\ncredentials_file = '/missing/credentials.json'",
        )
        .unwrap();
        assert_eq!(disabled.ingress.tunnel, TunnelConfig::default());
    }

    #[test]
    fn invalid_credentials_are_rejected_without_quoting_the_secret() {
        for (credentials, cause) in [
            (
                r#"{"AccountTag":"a","TunnelID":"x"}"#.to_owned(),
                "TunnelSecret",
            ),
            (
                r#"{"AccountTag":"a","TunnelSecret":"hunter2","TunnelID":"tunnel"}"#.into(),
                "not a UUID",
            ),
            (
                "-----BEGIN ARGO TUNNEL TOKEN-----".into(),
                "not a cloudflared",
            ),
        ] {
            let error = load(&credentials).unwrap_err();
            assert!(error.contains(cause), "{error}");
            assert!(!error.contains("hunter2"), "{error}");
        }
        let error = DaemonConfig::from_toml("[ingress.tunnel]\nenabled = true").unwrap_err();
        assert!(
            format!("{:#}", anyhow::Error::new(error)).contains("credentials_file is required")
        );
    }
}
