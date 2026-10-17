//! Shared asynchronous operation records.

use crate::EnvironmentId;
use crate::names::string_enum;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

string_enum! {
    #[serde]
    /// Environment change being reconciled.
    /// Deployment operations retain the serialized kind `refresh` for compatibility
    /// with existing stored records and API clients.
    pub enum OperationKind {
        /// Apply the requested application state.
        Apply => "apply",
        /// Deploy without changing the generation: resolve the current manifest
        /// again, or run a promoted or earlier release. Used by deploys, saves
        /// and edits that deploy, sync, previews, and promotions.
        Refresh => "refresh",
        /// Delete its resources.
        Delete => "delete",
    }
}

string_enum! {
    #[serde]
    /// Lifecycle of one desired-state operation.
    pub enum OperationState {
        /// Accepted and waiting to run.
        Requested => "requested",
        /// Reconciling and verifying runtime state.
        Running => "running",
        /// Runtime state matches the requested state.
        Succeeded => "succeeded",
        /// Reconciliation failed.
        Failed => "failed",
        /// Cancelled without a replacement operation.
        Cancelled => "cancelled",
        /// Replaced by newer intent; callers may stop waiting successfully.
        Superseded => "superseded",
    }
}

impl OperationState {
    /// Whether this attempt has finished.
    #[must_use]
    pub const fn terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded | Self::Failed | Self::Cancelled | Self::Superseded
        )
    }

    /// Whether an operation can make the requested lifecycle transition.
    ///
    /// Staying in the same state is always allowed. `Running` may return to
    /// `Requested` for a new attempt, and `Succeeded`, `Failed`, or `Cancelled`
    /// may be requeued; `Superseded` is final.
    ///
    /// ```text
    /// Requested -> Running | Cancelled | Superseded
    /// Running   -> Requested | Succeeded | Failed | Cancelled | Superseded
    /// Succeeded | Failed | Cancelled -> Requested
    /// ```
    #[must_use]
    pub fn can_transition_to(self, next: Self) -> bool {
        self == next
            || matches!(
                (self, next),
                (
                    Self::Requested,
                    Self::Running | Self::Cancelled | Self::Superseded
                ) | (
                    Self::Running,
                    Self::Requested
                        | Self::Succeeded
                        | Self::Failed
                        | Self::Cancelled
                        | Self::Superseded
                ) | (
                    Self::Succeeded | Self::Failed | Self::Cancelled,
                    Self::Requested
                )
            )
    }
}

/// Durable asynchronous operation returned by the API.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct Operation {
    /// Stable operation identifier.
    pub id: String,
    /// Environment the operation changes. Records stored before environments
    /// existed name it `application_id`; the value is the same.
    #[serde(alias = "application_id")]
    pub environment_id: EnvironmentId,
    /// Requested change.
    pub kind: OperationKind,
    /// Intent revision this operation targets.
    pub generation: u64,
    /// Number of execution attempts started, including interrupted attempts.
    pub attempt: u64,
    /// Consecutive failed attempts since the last successful execution.
    #[serde(default)]
    pub consecutive_failures: u64,
    /// Current lifecycle state.
    pub state: OperationState,
    /// Current execution phase, retained on failure.
    #[serde(default)]
    pub phase: Option<String>,
    /// Logical or Docker resource currently being processed.
    #[serde(default)]
    pub resource: Option<String>,
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
}

string_enum! {
    #[serde]
    /// Current environment reconciliation status.
    pub enum ApplicationState {
        /// Configuration is saved but no deployment has been requested.
        NotDeployed => "not_deployed",
        /// Desired state is waiting to be reconciled.
        Pending => "pending",
        /// Runtime resources are being reconciled.
        Deploying => "deploying",
        /// Runtime state matches desired state.
        Ready => "ready",
        /// Runtime state is not fully healthy.
        Degraded => "degraded",
        /// Runtime resources are being deleted.
        Deleting => "deleting",
        /// Reconciliation failed.
        Failed => "failed",
    }
}
