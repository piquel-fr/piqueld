//! Typed state and application transfer endpoints.

use http::Method;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
#[cfg(target_arch = "wasm32")]
use zeroize::Zeroize;

use crate::{Client, ClientError, path_segment};

/// Secret handling used by a control-plane state archive.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum StateExportMode {
    /// Includes logical-secret metadata only. This is the safe default.
    #[default]
    Portable,
    /// Includes encrypted envelopes; the master key is never included.
    Encrypted,
}

/// Confirmation returned before a destructive state replacement.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct PrepareStateImportRequest {
    /// `sha256:` digest of the exact archive bytes that will be imported.
    pub archive_digest: String,
}

/// Single-use confirmation bound to one exact archive digest.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct StateImportConfirmation {
    /// Header value accepted by the import endpoint.
    pub token: String,
    /// Digest bound to this confirmation.
    pub archive_digest: String,
    /// Expiration time in Unix milliseconds.
    pub expires_at_ms: i64,
}

/// Dependencies an operator must verify after restoring state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct ImportDependencyReport {
    /// Source archive instance identity.
    pub source_instance_id: String,
    /// Current target instance identity.
    pub target_instance_id: String,
    /// Whether resolved ownership can be retained for this target.
    pub ownership_compatible: bool,
    /// Logical secrets whose values were not present in the archive.
    pub missing_secret_values: Vec<String>,
    /// Encryption key IDs that need operator verification.
    pub incompatible_secret_keys: Vec<String>,
    /// Image references that must be verified before deployment.
    pub image_references_to_verify: Vec<String>,
    /// Git sources that require a fresh source/registry check.
    pub git_sources_to_resolve: Vec<String>,
    /// Runtime Swarm secret names that will be recreated.
    pub runtime_secrets_to_recreate: Vec<String>,
    /// Named volumes retained by the control-plane state.
    pub retained_volumes_to_verify: Vec<String>,
    /// Safe operator-facing caveats.
    pub notes: Vec<String>,
}

/// Result of a transactional state replacement.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct StateImportResult {
    /// Auditable transfer operation ID.
    pub operation_id: String,
    /// SHA-256 digest of the imported archive bytes.
    pub archive_digest: String,
    /// Number of applications restored.
    pub applications_imported: usize,
    /// Number of logical secrets restored.
    pub secrets_imported: usize,
    /// Dependencies requiring operator follow-up.
    pub dependencies: ImportDependencyReport,
}

/// Maximum state archive accepted by the transport-neutral client.
pub const MAX_STATE_ARCHIVE_BYTES: usize = 32 * 1024 * 1024;

#[cfg(not(target_arch = "wasm32"))]
impl Client {
    /// Downloads a checksummed state archive as raw bytes.
    ///
    /// # Errors
    /// Returns a transport, API, or bounded-response error.
    pub async fn export_state(&self, mode: StateExportMode) -> Result<Vec<u8>, ClientError> {
        let mode = match mode {
            StateExportMode::Portable => "portable",
            StateExportMode::Encrypted => "encrypted",
        };
        let response = self
            .raw_request::<()>(
                Method::GET,
                &format!("/api/v1/state/export?mode={mode}"),
                None,
                &[],
            )
            .await?;
        if !response.status().is_success() {
            return Err(self.decode_error(response).await);
        }
        let bytes = self.collect_body(response.into_body()).await?;
        if bytes.len() > MAX_STATE_ARCHIVE_BYTES {
            return Err(ClientError::Decode);
        }
        Ok(bytes.to_vec())
    }

    /// Requests a five-minute confirmation for one exact archive digest.
    ///
    /// # Errors
    /// Returns a transport, API, or response decoding error.
    pub async fn prepare_state_import(
        &self,
        archive_digest: &str,
    ) -> Result<StateImportConfirmation, ClientError> {
        self.send(
            Method::POST,
            "/api/v1/state/import/confirm",
            Some(&PrepareStateImportRequest {
                archive_digest: archive_digest.to_owned(),
            }),
            &[],
        )
        .await
    }

    /// Replaces control-plane state after the daemon validates the archive.
    ///
    /// # Errors
    /// Returns a transport, API, or bounded-response error.
    pub async fn import_state(
        &self,
        archive: Vec<u8>,
        confirmation: &str,
    ) -> Result<StateImportResult, ClientError> {
        if archive.is_empty() || archive.len() > MAX_STATE_ARCHIVE_BYTES {
            return Err(ClientError::Decode);
        }
        let response = self
            .raw_bytes(
                Method::POST,
                "/api/v1/state/import",
                archive,
                &[
                    ("content-type", "application/vnd.piqueld.state-v1+tar"),
                    ("x-replace-confirmation", confirmation),
                ],
            )
            .await?;
        self.decode(response).await
    }

    /// Downloads one canonical application manifest.
    ///
    /// # Errors
    /// Returns a transport, API, or bounded-response error.
    pub async fn export_application(
        &self,
        id: &str,
        include_resolved: bool,
    ) -> Result<String, ClientError> {
        let path = format!(
            "/api/v1/applications/{}/export?include_resolved={include_resolved}",
            path_segment(id)
        );
        let response = self
            .raw_request::<()>(Method::GET, &path, None, &[("accept", "application/toml")])
            .await?;
        let status = response.status();
        if !status.is_success() {
            return Err(self.decode_error(response).await);
        }
        let bytes = self.collect_body(response.into_body()).await?;
        if bytes.len() > 4 * 1024 * 1024 {
            return Err(ClientError::Decode);
        }
        String::from_utf8(bytes.to_vec()).map_err(|_| ClientError::Decode)
    }
}

#[cfg(target_arch = "wasm32")]
impl Client {
    /// Requests a single-use confirmation bound to one exact browser archive.
    ///
    /// # Errors
    /// Returns a transport, API, or response decoding error.
    pub async fn prepare_state_import(
        &self,
        archive_digest: &str,
    ) -> Result<StateImportConfirmation, ClientError> {
        let response = crate::browser_request(
            Method::POST,
            "/api/v1/state/import/confirm",
            &[("content-type", "application/json")],
        )
        .json(&PrepareStateImportRequest {
            archive_digest: archive_digest.to_owned(),
        })
        .map_err(|_| ClientError::Decode)?
        .send()
        .await
        .map_err(ClientError::transport)?;
        crate::decode_browser_response(response).await
    }

    /// Downloads a checksummed state archive in the browser.
    ///
    /// # Errors
    /// Returns a transport, API, or bounded-response error.
    pub async fn export_state(&self, mode: StateExportMode) -> Result<Vec<u8>, ClientError> {
        let mode = match mode {
            StateExportMode::Portable => "portable",
            StateExportMode::Encrypted => "encrypted",
        };
        let response = crate::browser_request(
            Method::GET,
            &format!("/api/v1/state/export?mode={mode}"),
            &[("accept", "application/octet-stream")],
        )
        .send()
        .await
        .map_err(ClientError::transport)?;
        if !response.ok() {
            return Err(crate::decode_browser_error(response).await);
        }
        let archive = response.binary().await.map_err(ClientError::transport)?;
        if archive.len() > MAX_STATE_ARCHIVE_BYTES {
            return Err(ClientError::Decode);
        }
        Ok(archive)
    }

    /// Replaces control-plane state in the browser after daemon confirmation.
    ///
    /// # Errors
    /// Returns a transport, API, or bounded-response error.
    pub async fn import_state(
        &self,
        archive: Vec<u8>,
        confirmation: &str,
    ) -> Result<StateImportResult, ClientError> {
        if archive.is_empty() || archive.len() > MAX_STATE_ARCHIVE_BYTES {
            return Err(ClientError::Decode);
        }
        let mut archive = archive;
        let body = js_sys::Uint8Array::from(archive.as_slice());
        archive.zeroize();
        let request = crate::browser_request(
            Method::POST,
            "/api/v1/state/import",
            &[
                ("content-type", "application/vnd.piqueld.state-v1+tar"),
                ("x-replace-confirmation", confirmation),
            ],
        )
        .body(body.clone())
        .map_err(|_| ClientError::Endpoint)?;
        let response = request.send().await.map_err(ClientError::transport);
        body.fill(0, 0, body.length());
        let response = response?;
        crate::decode_browser_response(response).await
    }

    /// Downloads one canonical application manifest in the browser.
    ///
    /// # Errors
    /// Returns a transport, API, or bounded-response error.
    pub async fn export_application(
        &self,
        id: &str,
        include_resolved: bool,
    ) -> Result<String, ClientError> {
        let response = crate::browser_request(
            Method::GET,
            &format!(
                "/api/v1/applications/{}/export?include_resolved={include_resolved}",
                path_segment(id)
            ),
            &[("accept", "application/toml")],
        )
        .send()
        .await
        .map_err(ClientError::transport)?;
        if !response.ok() {
            return Err(crate::decode_browser_error(response).await);
        }
        let document = response.text().await.map_err(ClientError::transport)?;
        if document.len() > 4 * 1024 * 1024 {
            return Err(ClientError::Decode);
        }
        Ok(document)
    }
}
