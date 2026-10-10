//! Informational control-plane history; never used to reconstruct engine state.
use crate::{ApplicationId, EnvironmentId};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// One immutable, sanitized fact about control-plane activity.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Event {
    /// Monotonic event identity used for pagination.
    pub id: i64,
    /// Related application, including for events of its deleted environments.
    #[serde(default)]
    pub application_id: Option<ApplicationId>,
    /// Related environment; absent for application-wide events such as edits.
    pub environment_id: Option<EnvironmentId>,
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
    /// Lifetime owner, independent of contextual environment references.
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
    /// Who caused this event, including runtime actions of the operation it
    /// requested; absent for the daemon's own work.
    #[serde(default)]
    pub actor: Option<EventActor>,
    /// Safe, independently readable failure details.
    #[serde(default)]
    pub diagnostic: Option<crate::observability::Diagnostic>,
    /// Unix timestamp in milliseconds.
    pub created_at_ms: i64,
}

/// Who caused an event: exactly one kind of actor.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EventActor {
    /// An account's API request.
    Account {
        /// The account.
        user_id: String,
        /// The credential that authenticated the request, when there was one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        credential_id: Option<String>,
    },
    /// The host operator, instead of an account.
    Operator {
        /// Its Unix user.
        operator: crate::auth::HostOperator,
        /// The browser session it acted through, if any.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        session_id: Option<String>,
    },
    /// An automated actor of the daemon, e.g. `sync:poll`.
    System {
        /// Which one.
        actor: crate::sync::SystemActor,
    },
}

impl EventActor {
    /// The credential or host operator session the request used, which the
    /// audit trail is filtered by.
    #[must_use]
    pub fn credential_id(&self) -> Option<&str> {
        match self {
            Self::Account { credential_id, .. } => credential_id.as_deref(),
            Self::Operator { session_id, .. } => session_id.as_deref(),
            Self::System { .. } => None,
        }
    }
}

/// ```text
/// user-01…    host operator (uid 0)    sync:poll
/// ```
impl std::fmt::Display for EventActor {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Account { user_id, .. } => formatter.write_str(user_id),
            Self::Operator { operator, .. } => operator.fmt(formatter),
            Self::System { actor } => actor.fmt(formatter),
        }
    }
}
