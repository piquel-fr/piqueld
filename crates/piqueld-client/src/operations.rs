use http::Method;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{Client, ClientError, path_segment};

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
/// One step in an asynchronous operation.
pub struct OperationStepView {
    /// Stable step identifier.
    pub id: String,
    /// Position in the operation plan.
    pub position: u32,
    /// Stable action name.
    pub action: String,
    /// Machine-readable step state.
    pub state: String,
    /// Number of execution attempts.
    pub attempt: u32,
    /// Stable failure code, when present.
    pub error_code: Option<String>,
    /// Safe failure message, when present.
    pub error_message: Option<String>,
    /// Last update timestamp in Unix milliseconds.
    pub updated_at_ms: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
/// Asynchronous operation and its execution steps.
pub struct OperationView {
    /// Stable operation identifier.
    pub id: String,
    /// Stable application identifier.
    pub application_id: String,
    /// Application generation associated with the operation.
    #[schema(minimum = 1)]
    pub generation: u64,
    /// Machine-readable operation kind.
    pub kind: String,
    /// Machine-readable operation state.
    pub state: String,
    /// Stable failure code, when present.
    pub error_code: Option<String>,
    /// Safe failure message, when present.
    pub error_message: Option<String>,
    /// Creation timestamp in Unix milliseconds.
    pub created_at_ms: i64,
    /// Last update timestamp in Unix milliseconds.
    pub updated_at_ms: i64,
    /// Start timestamp in Unix milliseconds.
    pub started_at_ms: Option<i64>,
    /// Completion timestamp in Unix milliseconds.
    pub finished_at_ms: Option<i64>,
    /// Ordered operation steps.
    pub steps: Vec<OperationStepView>,
}

impl Client {
    /// Fetches an asynchronous operation by identifier.
    ///
    /// # Errors
    /// Returns [`ClientError`] when transport, decoding, or API response handling fails.
    pub async fn operation(&self, id: &str) -> Result<OperationView, ClientError> {
        self.send::<_, ()>(
            Method::GET,
            &format!("/api/v1/operations/{}", path_segment(id)),
            None,
            &[],
        )
        .await
    }

    /// Watches one operation through the shared resumable SSE cursor model.
    ///
    /// The caller owns reconnection and passes the last received event ID when
    /// opening the next stream.
    #[cfg(not(target_arch = "wasm32"))]
    #[must_use]
    pub fn watch_operation(
        &self,
        id: &str,
        last_event_id: Option<&str>,
    ) -> tokio::sync::mpsc::Receiver<Result<crate::SseEvent, ClientError>> {
        self.watch_events(
            format!("/api/v1/operations/{}/events", path_segment(id)),
            last_event_id,
        )
    }
}
