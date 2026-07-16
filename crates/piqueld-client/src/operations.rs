use http::Method;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{Client, ClientError, SseEvent, path_segment};

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct OperationStepView {
    pub id: String,
    pub position: u32,
    pub kind: String,
    pub state: String,
    pub attempt: u32,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub updated_at_ms: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct OperationView {
    pub id: String,
    pub application_id: String,
    #[schema(minimum = 1)]
    pub generation: u64,
    pub kind: String,
    pub state: String,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
    pub started_at_ms: Option<i64>,
    pub finished_at_ms: Option<i64>,
    pub steps: Vec<OperationStepView>,
}

impl Client {
    pub async fn operation(&self, id: &str) -> Result<OperationView, ClientError> {
        self.send::<_, ()>(
            Method::GET,
            &format!("/api/v1/operations/{}", path_segment(id)),
            None,
            &[],
        )
        .await
    }

    /// Watches operation progress. Dropping the receiver cancels socket reading and closes the connection.
    #[must_use]
    pub fn watch_operation(
        &self,
        id: &str,
        last_event_id: Option<&str>,
    ) -> tokio::sync::mpsc::Receiver<Result<SseEvent, ClientError>> {
        self.watch_events(
            format!("/api/v1/operations/{}/events", path_segment(id)),
            last_event_id,
        )
    }
}
