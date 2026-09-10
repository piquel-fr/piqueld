//! Informational control-plane history; never used to reconstruct engine state.
use crate::ApplicationId;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// One immutable, sanitized fact about control-plane activity.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Event {
    /// Monotonic event identity used for pagination.
    pub id: i64,
    /// Related application, when applicable.
    pub application_id: Option<ApplicationId>,
    /// Related operation, retained even if that operation is pruned.
    pub operation_id: Option<String>,
    /// Intent generation, when applicable.
    pub generation: Option<u64>,
    /// Execution attempt for attempt-related events.
    pub attempt: Option<u64>,
    /// Stable event category, such as `operation_failed` or `health_changed`.
    pub kind: String,
    /// Sanitized diagnostic or state description.
    pub message: Option<String>,
    /// Stable failure classification, when this event records failure.
    pub error_code: Option<String>,
    /// Execution phase at the time of the event.
    pub phase: Option<String>,
    /// Related logical or Docker resource.
    pub resource: Option<String>,
    /// Unix timestamp in milliseconds.
    pub created_at_ms: i64,
}
