//! Shared asynchronous operation records.

use crate::ApplicationId;
use serde::{Deserialize, Serialize};
use std::fmt;
use utoipa::ToSchema;

/// Application change being reconciled.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    /// Apply the requested application state.
    Apply,
    /// Resolve the current manifest again without changing its generation.
    Refresh,
    /// Delete its resources.
    Delete,
}

impl OperationKind {
    /// Returns the serialized operation name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Apply => "apply",
            Self::Refresh => "refresh",
            Self::Delete => "delete",
        }
    }
}

impl fmt::Display for OperationKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Lifecycle of one desired-state operation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum OperationState {
    /// Accepted and waiting to run.
    Requested,
    /// Reconciling and verifying runtime state.
    Running,
    /// Runtime state matches the requested state.
    Succeeded,
    /// Reconciliation failed.
    Failed,
    /// Cancelled or superseded by another operation.
    Cancelled,
}

impl OperationState {
    /// Returns the serialized state name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Requested => "requested",
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// Whether this attempt has finished.
    #[must_use]
    pub const fn terminal(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }

    /// Whether an operation can make the requested lifecycle transition.
    #[must_use]
    pub fn can_transition_to(self, next: Self) -> bool {
        self == next
            || matches!(
                (self, next),
                (Self::Requested, Self::Running | Self::Cancelled)
                    | (
                        Self::Running,
                        Self::Requested | Self::Succeeded | Self::Failed | Self::Cancelled
                    )
                    | (
                        Self::Succeeded | Self::Failed | Self::Cancelled,
                        Self::Requested
                    )
            )
    }
}

impl fmt::Display for OperationState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Durable asynchronous operation returned by the API.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct Operation {
    /// Stable operation identifier.
    pub id: String,
    /// Stable application identifier.
    pub application_id: ApplicationId,
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

/// Current application reconciliation status.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApplicationState {
    /// Desired state is waiting to be reconciled.
    Pending,
    /// Runtime resources are being reconciled.
    Deploying,
    /// Runtime state matches desired state.
    Ready,
    /// Runtime state is not fully healthy.
    Degraded,
    /// Runtime resources are being deleted.
    Deleting,
    /// Reconciliation failed.
    Failed,
}

impl ApplicationState {
    /// Returns the serialized status name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Deploying => "deploying",
            Self::Ready => "ready",
            Self::Degraded => "degraded",
            Self::Deleting => "deleting",
            Self::Failed => "failed",
        }
    }
}

impl fmt::Display for ApplicationState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}
