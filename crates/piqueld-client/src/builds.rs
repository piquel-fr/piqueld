//! Transport-neutral source-build views and read APIs.

use http::Method;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{Client, ClientError, Page, path_segment};

/// Registry-verified source build status.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct BuildView {
    /// Stable build identifier.
    pub id: String,
    /// Owning operation identifier.
    pub operation_id: String,
    /// Owning application identifier.
    pub application_id: String,
    /// Logical service name.
    pub service_name: String,
    /// Current durable build state.
    pub state: String,
    /// Resolved source commit.
    pub source_commit: Option<String>,
    /// Mutable registry tag.
    pub image_reference: Option<String>,
    /// Immutable registry digest reference.
    pub image_digest: Option<String>,
    /// Build identity hash, when an artifact row exists.
    pub build_key: Option<String>,
    /// Deterministic context hash, when an artifact row exists.
    pub context_hash: Option<String>,
    /// Whether the output is registry verified.
    pub verified: bool,
    /// Creation timestamp in Unix milliseconds.
    pub created_at_ms: i64,
    /// Last update timestamp in Unix milliseconds.
    pub updated_at_ms: i64,
    /// Completion timestamp in Unix milliseconds.
    pub finished_at_ms: Option<i64>,
}

/// One redacted source-build log entry.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct BuildLogView {
    /// Owning build identifier.
    pub build_id: String,
    /// Monotonic build-local cursor.
    pub sequence: u64,
    /// Log timestamp in Unix milliseconds.
    pub timestamp_ms: i64,
    /// Redacted log message.
    pub message: String,
}

impl Client {
    /// Fetches one build status.
    ///
    /// # Errors
    /// Returns [`ClientError`] when the request or response cannot be handled.
    pub async fn build(&self, id: &str) -> Result<BuildView, ClientError> {
        self.send::<_, ()>(
            Method::GET,
            &format!("/api/v1/builds/{}", path_segment(id)),
            None,
            &[],
        )
        .await
    }

    /// Fetches builds attached to an operation.
    ///
    /// # Errors
    /// Returns [`ClientError`] when the request or response cannot be handled.
    pub async fn operation_builds(&self, id: &str) -> Result<Page<BuildView>, ClientError> {
        self.send::<_, ()>(
            Method::GET,
            &format!("/api/v1/operations/{}/builds", path_segment(id)),
            None,
            &[],
        )
        .await
    }

    /// Fetches a bounded build-log page after a sequence cursor.
    ///
    /// # Errors
    /// Returns [`ClientError`] when the request or response cannot be handled.
    pub async fn build_logs(
        &self,
        id: &str,
        after: u64,
        limit: u16,
    ) -> Result<Page<BuildLogView>, ClientError> {
        self.send::<_, ()>(
            Method::GET,
            &format!(
                "/api/v1/builds/{}/logs?after={after}&limit={limit}",
                path_segment(id)
            ),
            None,
            &[],
        )
        .await
    }
}
