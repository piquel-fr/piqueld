use http::Method;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{Client, ClientError, Page, decode_envelope, path_segment};

/// One application service that references a logical secret.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct SecretReferenceView {
    /// Stable application identifier.
    pub application_id: String,
    /// Human-readable application name.
    pub application_name: String,
    /// Manifest service name.
    pub service: String,
    /// Whether this reference is present in deployed state.
    pub deployed: bool,
}

/// Metadata for a logical secret; plaintext is never returned.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct SecretMetadata {
    /// Logical secret name.
    pub name: String,
    /// Whether the secret currently has an encrypted value.
    pub value_is_set: bool,
    /// Monotonically increasing value generation.
    pub generation: u64,
    /// Creation timestamp in Unix milliseconds.
    pub created_at_ms: i64,
    /// Last-update timestamp in Unix milliseconds.
    pub updated_at_ms: i64,
    /// Application services that reference this secret.
    pub references: Vec<SecretReferenceView>,
}

/// Cursor and page-size options for listing logical secrets.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ListSecretsOptions {
    /// Cursor returned by a previous page.
    pub cursor: Option<String>,
    /// Maximum number of items to return.
    pub limit: Option<u16>,
}

impl Client {
    /// Lists logical secret metadata.
    ///
    /// # Errors
    ///
    /// Returns a transport or API error when the request fails.
    pub async fn secrets(&self) -> Result<Page<SecretMetadata>, ClientError> {
        self.secrets_with(&ListSecretsOptions::default()).await
    }

    /// Lists one page of logical secret metadata.
    ///
    /// # Errors
    ///
    /// Returns a transport or API error when the request fails.
    pub async fn secrets_with(
        &self,
        options: &ListSecretsOptions,
    ) -> Result<Page<SecretMetadata>, ClientError> {
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        if let Some(cursor) = &options.cursor {
            query.append_pair("cursor", cursor);
        }
        if let Some(limit) = options.limit {
            query.append_pair("limit", &limit.to_string());
        }
        let query = query.finish();
        let path = if query.is_empty() {
            "/api/v1/secrets".to_owned()
        } else {
            format!("/api/v1/secrets?{query}")
        };
        self.send::<_, ()>(Method::GET, &path, None, &[]).await
    }

    /// Fetches metadata for one logical secret.
    ///
    /// # Errors
    ///
    /// Returns a transport or API error when the request fails.
    pub async fn secret(&self, name: &str) -> Result<SecretMetadata, ClientError> {
        self.send::<_, ()>(
            Method::GET,
            &format!("/api/v1/secrets/{}", path_segment(name)),
            None,
            &[],
        )
        .await
    }

    /// Creates a logical secret from in-memory plaintext.
    ///
    /// # Errors
    ///
    /// Returns a transport or API error when the request fails.
    #[cfg(not(target_arch = "wasm32"))]
    pub async fn create_secret(
        &self,
        name: &str,
        value: Vec<u8>,
    ) -> Result<SecretMetadata, ClientError> {
        decode_envelope(
            self.raw_bytes(
                Method::POST,
                &format!("/api/v1/secrets/{}", path_segment(name)),
                value,
                &[("content-type", "application/octet-stream")],
            )
            .await?,
        )
        .await
    }

    /// Replaces a logical secret with a new plaintext generation.
    ///
    /// # Errors
    ///
    /// Returns a transport or API error when the request fails.
    #[cfg(not(target_arch = "wasm32"))]
    pub async fn replace_secret(
        &self,
        name: &str,
        value: Vec<u8>,
    ) -> Result<SecretMetadata, ClientError> {
        decode_envelope(
            self.raw_bytes(
                Method::PUT,
                &format!("/api/v1/secrets/{}", path_segment(name)),
                value,
                &[("content-type", "application/octet-stream")],
            )
            .await?,
        )
        .await
    }

    /// Creates a logical secret from a protected local file.
    ///
    /// # Errors
    ///
    /// Returns a file or transport/API error when the request fails.
    #[cfg(not(target_arch = "wasm32"))]
    pub async fn create_secret_file(
        &self,
        name: &str,
        path: impl AsRef<std::path::Path>,
    ) -> Result<SecretMetadata, ClientError> {
        let value = read_protected_secret_file(path.as_ref().to_owned()).await?;
        self.create_secret(name, value).await
    }

    /// Replaces a logical secret from a protected local file.
    ///
    /// # Errors
    ///
    /// Returns a file or transport/API error when the request fails.
    #[cfg(not(target_arch = "wasm32"))]
    pub async fn replace_secret_file(
        &self,
        name: &str,
        path: impl AsRef<std::path::Path>,
    ) -> Result<SecretMetadata, ClientError> {
        let value = read_protected_secret_file(path.as_ref().to_owned()).await?;
        self.replace_secret(name, value).await
    }

    /// Deletes an unreferenced logical secret.
    ///
    /// # Errors
    ///
    /// Returns a transport or API error when the request fails.
    #[cfg(not(target_arch = "wasm32"))]
    pub async fn delete_secret(&self, name: &str) -> Result<(), ClientError> {
        let response = self
            .raw_request::<()>(
                Method::DELETE,
                &format!("/api/v1/secrets/{}", path_segment(name)),
                None,
                &[],
            )
            .await?;
        if response.status() == http::StatusCode::NO_CONTENT {
            Ok(())
        } else {
            Err(crate::decode_api_error(response).await)
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
async fn read_protected_secret_file(path: std::path::PathBuf) -> Result<Vec<u8>, ClientError> {
    use std::{
        io::Read,
        os::unix::fs::{MetadataExt, PermissionsExt},
    };

    tokio::task::spawn_blocking(move || {
        let descriptor = rustix::fs::openat2(
            rustix::fs::CWD,
            &path,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
            rustix::fs::ResolveFlags::NO_SYMLINKS | rustix::fs::ResolveFlags::NO_MAGICLINKS,
        )
        .map_err(|_| ClientError::SecretFile)?;
        let mut file = std::fs::File::from(descriptor);
        let opened = file.metadata().map_err(|_| ClientError::SecretFile)?;
        if !opened.is_file() || opened.permissions().mode() & 0o077 != 0 {
            return Err(ClientError::SecretFile);
        }
        let path_metadata =
            std::fs::symlink_metadata(&path).map_err(|_| ClientError::SecretFile)?;
        if path_metadata.file_type().is_symlink()
            || path_metadata.dev() != opened.dev()
            || path_metadata.ino() != opened.ino()
        {
            return Err(ClientError::SecretFile);
        }
        let mut value = Vec::new();
        file.by_ref()
            .take(500 * 1024 + 1)
            .read_to_end(&mut value)
            .map_err(|_| ClientError::SecretFile)?;
        if value.is_empty() || value.len() > 500 * 1024 {
            return Err(ClientError::SecretFile);
        }
        Ok(value)
    })
    .await
    .map_err(|_| ClientError::SecretFile)?
}
