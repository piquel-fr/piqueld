//! Typed state and application transfer endpoints.

use http::Method;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

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
        let status = response.status();
        let bytes = http_body_util::BodyExt::collect(response.into_body())
            .await
            .map_err(ClientError::transport)?
            .to_bytes();
        if !status.is_success() {
            return Err(ClientError::Api {
                status,
                error: super::error_body(&bytes),
            });
        }
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
        super::decode_envelope(response).await
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
        let bytes = http_body_util::BodyExt::collect(response.into_body())
            .await
            .map_err(ClientError::transport)?
            .to_bytes();
        if !status.is_success() {
            return Err(ClientError::Api {
                status,
                error: super::error_body(&bytes),
            });
        }
        if bytes.len() > 4 * 1024 * 1024 {
            return Err(ClientError::Decode);
        }
        String::from_utf8(bytes.to_vec()).map_err(|_| ClientError::Decode)
    }
}
