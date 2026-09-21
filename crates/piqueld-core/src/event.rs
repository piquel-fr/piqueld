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
    /// Lifetime owner, independent of contextual application references.
    #[serde(default)]
    pub scope: crate::observability::EventScope,
    /// Action execution identity.
    #[serde(default)]
    pub action_id: Option<String>,
    /// One-based action request attempt.
    #[serde(default)]
    pub retry: Option<u64>,
    /// Scheduled delay before another request, in milliseconds.
    #[serde(default)]
    pub retry_delay_ms: Option<u64>,
    /// Action duration in milliseconds.
    #[serde(default)]
    pub duration_ms: Option<u64>,
    /// Correlated API request identity.
    #[serde(default)]
    pub request_id: Option<String>,
    /// Safe, independently readable failure details.
    #[serde(default)]
    pub diagnostic: Option<crate::observability::Diagnostic>,
    /// Unix timestamp in milliseconds.
    pub created_at_ms: i64,
}
