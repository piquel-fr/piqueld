//! Local OCI Distribution registry boundary.
#![allow(missing_docs)]
#![allow(clippy::missing_errors_doc)]

use crate::build::{BuildError, verified_digest};
use reqwest::{Client, StatusCode, header};
use sha2::{Digest, Sha256};
use std::time::Duration;
use url::Url;

#[derive(Clone)]
pub struct RegistryClient {
    endpoint: Url,
    client: Client,
}

impl RegistryClient {
    pub fn new(endpoint: &str, timeout: Duration) -> Result<Self, BuildError> {
        let mut endpoint = Url::parse(endpoint).map_err(|_| BuildError::Registry)?;
        if !matches!(endpoint.scheme(), "http" | "https")
            || endpoint.host_str().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
        {
            return Err(BuildError::Registry);
        }
        endpoint.set_path("");
        endpoint.set_query(None);
        endpoint.set_fragment(None);
        let client = Client::builder()
            .timeout(timeout)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| BuildError::Registry)?;
        Ok(Self { endpoint, client })
    }

    pub async fn ready(&self) -> Result<(), BuildError> {
        let response = self
            .client
            .get(
                self.endpoint
                    .join("v2/")
                    .map_err(|_| BuildError::Registry)?,
            )
            .send()
            .await
            .map_err(|_| BuildError::Registry)?;
        if response.status() == StatusCode::OK {
            Ok(())
        } else {
            Err(BuildError::Registry)
        }
    }

    /// Fetches a manifest and checks its payload hash against any registry digest header.
    pub async fn resolve_manifest_digest(
        &self,
        repository: &str,
        reference: &str,
    ) -> Result<String, BuildError> {
        if !safe_repository(repository) || !safe_reference(reference) {
            return Err(BuildError::Registry);
        }
        let url = self
            .endpoint
            .join(&format!("v2/{repository}/manifests/{reference}"))
            .map_err(|_| BuildError::Registry)?;
        let response = self.client.get(url).header(header::ACCEPT, "application/vnd.oci.image.manifest.v1+json, application/vnd.docker.distribution.manifest.v2+json").send().await.map_err(|_| BuildError::Registry)?;
        if response.status() != StatusCode::OK {
            return Err(BuildError::Registry);
        }
        let declared = response
            .headers()
            .get("docker-content-digest")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let body = response.bytes().await.map_err(|_| BuildError::Registry)?;
        let computed = format!("sha256:{:x}", Sha256::digest(&body));
        if declared
            .as_deref()
            .is_some_and(|digest| !digest.eq_ignore_ascii_case(&computed))
        {
            return Err(BuildError::Digest);
        }
        Ok(computed)
    }

    pub async fn verified_reference(
        &self,
        repository: &str,
        tag: &str,
    ) -> Result<String, BuildError> {
        let digest = self.resolve_manifest_digest(repository, tag).await?;
        verified_digest(
            &format!("{}/{repository}", authority(&self.endpoint)),
            &digest,
        )
    }
}

fn authority(url: &Url) -> String {
    match url.port() {
        Some(port) => format!("{}:{port}", url.host_str().unwrap_or_default()),
        None => url.host_str().unwrap_or_default().into(),
    }
}
fn safe_repository(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 255
        && value.split('/').all(|part| {
            !part.is_empty()
                && part != "."
                && part != ".."
                && part.len() <= 64
                && part
                    .bytes()
                    .next()
                    .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
                && part
                    .bytes()
                    .last()
                    .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
                && part.bytes().all(|b| {
                    b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'-' | b'_' | b'.')
                })
        })
}
fn safe_reference(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn endpoint_rejects_embedded_credentials_and_unsafe_names() {
        assert!(RegistryClient::new("http://u:p@127.0.0.1:5000", Duration::from_secs(1)).is_err());
        assert!(!safe_repository("../escape"));
        assert!(!safe_reference("bad/tag"));
    }
}
