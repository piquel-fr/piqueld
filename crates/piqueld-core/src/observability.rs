//! Stable, sanitized observability contracts shared by daemon and clients.
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Ownership of historical data, independent of its contextual application ID.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum EventScope {
    /// Removed when the application is deleted.
    #[default]
    Application,
    /// Retained under the daemon history policy.
    Daemon,
}
impl EventScope {
    /// Storage representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Application => "application",
            Self::Daemon => "daemon",
        }
    }
}

/// Safe structured explanation of a failure. Never contains raw engine payloads.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Diagnostic {
    /// Stable occurrence identity shared with responses and logs.
    pub id: String,
    /// Stable classification.
    pub code: String,
    /// What failed.
    pub summary: String,
    /// Explicitly sanitized causal facts.
    pub causes: Vec<String>,
    /// Whether automatic recovery is appropriate.
    pub retryable: bool,
    /// What happens next or what the administrator can do.
    pub next_action: String,
    /// Retention ownership.
    pub scope: EventScope,
}

/// Indexed event selection. No arbitrary SQL or message expressions.
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct EventFilter {
    /// Contextual application ID, including references retained in daemon history.
    pub application_id: Option<String>,
    /// Operation identity.
    pub operation_id: Option<String>,
    /// Execution attempt.
    pub attempt: Option<u64>,
    /// Action execution identity.
    pub action_id: Option<String>,
    /// Event category.
    pub kind: Option<String>,
    /// Failure classification.
    pub error_code: Option<String>,
    /// Only diagnostic events.
    pub errors_only: bool,
    /// History owner.
    pub scope: Option<EventScope>,
    /// Inclusive Unix millisecond lower bound.
    pub since_ms: Option<i64>,
    /// Inclusive Unix millisecond upper bound.
    pub until_ms: Option<i64>,
    /// Newest first; false means oldest first.
    pub descending: bool,
}

/// Current daemon measurements. Missing OS values are unavailable, never fabricated zeroes.
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct DaemonStats {
    /// Unix milliseconds when this snapshot was collected.
    pub sampled_at_ms: i64,
    /// Process lifetime in seconds.
    pub uptime_seconds: u64,
    /// Resident memory in bytes.
    pub memory_bytes: Option<u64>,
    /// CPU seconds used by this process.
    pub cpu_seconds: Option<f64>,
    /// CPU usage between samples; 100 means one full CPU core.
    pub cpu_percent: Option<f64>,
    /// Main database file size.
    pub database_bytes: u64,
    /// SQLite write-ahead log size.
    pub wal_bytes: u64,
    /// Free bytes available to this process on the data filesystem.
    pub available_disk_bytes: Option<u64>,
    /// Number of retained events.
    pub events: i64,
    /// Number of retained diagnostic occurrences.
    pub diagnostics: i64,
    /// Retained build-output bytes.
    pub build_output_bytes: i64,
    /// Operations currently executing.
    pub running_operations: i64,
    /// Operations waiting to execute.
    pub queued_operations: i64,
    /// Notification deliveries awaiting a retry.
    pub pending_deliveries: i64,
    /// Notification deliveries that exhausted retries or were rejected.
    pub failed_deliveries: i64,
}

macro_rules! notification_values {
    ($(#[$meta:meta])* $name:ident { $($(#[$variant_meta:meta])* $variant:ident => $value:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq, ToSchema)]
        #[serde(rename_all = "snake_case")]
        pub enum $name {
            $($(#[$variant_meta])* #[serde(rename = $value)] $variant),+
        }
        impl $name {
            /// Stable storage and wire representation.
            #[must_use]
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $value),+ }
            }
            /// Parses a stored value, rejecting unknown categories or states.
            #[must_use]
            pub fn parse(value: &str) -> Option<Self> {
                match value { $($value => Some(Self::$variant)),+, _ => None }
            }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}
notification_values! {
    /// Global notification categories supported by daemon configuration.
    NotificationCategory {
        /// Source checkout or image build failures.
        BuildFailures => "build_failures",
        /// Failed deployment attempts.
        DeploymentFailures => "deployment_failures",
        /// Continuously observed degraded application health.
        ServiceDegradation => "service_degradation",
        /// Shared dependency and internal daemon failures.
        DaemonFailures => "daemon_failures",
        /// Recovery of a condition reported to this destination.
        Recovery => "recovery",
    }
}
notification_values! {
    /// Lifecycle of a durable webhook delivery.
    DeliveryState {
        /// Waiting for delivery, retry, or its failure notification.
        Pending => "pending",
        /// The receiver acknowledged the delivery.
        Delivered => "delivered",
        /// Retries expired or the receiver rejected delivery.
        Failed => "failed",
        /// Configuration or incident policy cancelled delivery.
        Cancelled => "cancelled",
    }
}

/// A retained notification delivery; credentials and receiver response bodies are excluded.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct NotificationDelivery {
    /// Stable delivery identity, preserved through retries.
    pub id: String,
    /// Source event.
    pub event_id: i64,
    /// Configured destination name.
    pub destination: String,
    /// Notification category, including recovery.
    pub category: NotificationCategory,
    /// Current delivery lifecycle state.
    pub state: DeliveryState,
    /// Number of delivery requests attempted.
    pub attempts: i64,
    /// Creation timestamp.
    pub created_at_ms: i64,
    /// Scheduled attempt timestamp.
    pub next_attempt_ms: i64,
    /// Last update timestamp.
    pub updated_at_ms: i64,
    /// Safe delivery failure summary.
    pub last_error: Option<String>,
}

/// Aggregated duration for one action phase.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct ActionDuration {
    /// Action phase.
    pub phase: String,
    /// Completed actions measured.
    pub count: i64,
    /// Mean duration in milliseconds.
    pub mean_ms: f64,
}
/// Frequency of a diagnostic classification.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct FailureCount {
    /// Stable failure code.
    pub code: String,
    /// Recorded occurrences.
    pub count: i64,
}
/// Analytics derived from durable records within a selected time interval.
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct DeploymentAnalytics {
    /// Inclusive lower time bound.
    pub since_ms: i64,
    /// Inclusive upper time bound.
    pub until_ms: i64,
    /// First time detailed observability was available.
    pub history_started_at_ms: i64,
    /// History was removed through this timestamp, when known.
    pub pruned_through_ms: Option<i64>,
    /// Selected interval extends beyond available detailed history.
    pub incomplete: bool,
    /// Distinct deployments with terminal attempts in the interval.
    pub deployments: i64,
    /// Deployments whose latest attempt in the interval succeeded.
    pub succeeded: i64,
    /// Deployments whose latest attempt in the interval failed.
    pub failed: i64,
    /// Failed execution attempts, including subsequent recoveries.
    pub failed_attempts: i64,
    /// Execution attempts beyond the first.
    pub retry_attempts: i64,
    /// Individual Docker/action failures followed by retry.
    pub action_retries: i64,
    /// Mean completed deployment attempt duration.
    pub mean_duration_ms: Option<f64>,
    /// Durations grouped by phase.
    pub actions: Vec<ActionDuration>,
    /// Common failures, descending by count.
    pub failures: Vec<FailureCount>,
}

impl Diagnostic {
    /// Classifies a stable failure code without inspecting arbitrary error text.
    #[must_use]
    pub fn new(id: String, code: &str, summary: String) -> Self {
        let scope = match code {
            "docker_unavailable"
            | "swarm_manager_unavailable"
            | "swarm_topology_unsupported"
            | "journal_unavailable"
            | "storage_unavailable"
            | "stored_state_corrupt"
            | "schema_mismatch"
            | "internal_error"
            | "application_compilation_failed" => EventScope::Daemon,
            _ => EventScope::Application,
        };
        let retryable = matches!(
            code,
            "docker_unavailable"
                | "image_resolution_failed"
                | "docker_request_failed"
                | "convergence_timeout"
                | "journal_unavailable"
                | "storage_unavailable"
        );
        let next_action = match code {
            "docker_unavailable" => "Check Docker Engine availability. Reconciliation retries after connectivity recovers.",
            "image_resolution_rejected" => "Check the image reference and registry credentials, then retry the deployment.",
            "git_build_failed" => "Open the build output, fix the source or build configuration, then deploy again.",
            "service_update_failed" => "Inspect service health and logs. The previous healthy task may still be running.",
            "convergence_timeout" => "Inspect service health and resource capacity. Reconciliation will retry.",
            "journal_unavailable" | "storage_unavailable" => "Restore writable control-plane storage before infrastructure changes can resume.",
            "ownership_conflict" | "docker_configuration_conflict" => "Inspect the conflicting resource and resolve its ownership or immutable configuration.",
            _ if retryable => "Reconciliation will retry. Inspect the affected resource if the failure persists.",
            _ => "Inspect the diagnostic and related events; resolve the cause before retrying.",
        }.to_owned();
        Self {
            id,
            code: code.to_owned(),
            summary,
            causes: Vec::new(),
            retryable,
            next_action,
            scope,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{DeliveryState, NotificationCategory};
    use utoipa::PartialSchema;

    #[test]
    fn notification_schemas_match_wire_values() {
        for (schema, values) in [
            (
                DeliveryState::schema(),
                serde_json::to_value([
                    DeliveryState::Pending,
                    DeliveryState::Delivered,
                    DeliveryState::Failed,
                    DeliveryState::Cancelled,
                ])
                .unwrap(),
            ),
            (
                NotificationCategory::schema(),
                serde_json::to_value([
                    NotificationCategory::BuildFailures,
                    NotificationCategory::DeploymentFailures,
                    NotificationCategory::ServiceDegradation,
                    NotificationCategory::DaemonFailures,
                    NotificationCategory::Recovery,
                ])
                .unwrap(),
            ),
        ] {
            assert_eq!(serde_json::to_value(schema).unwrap()["enum"], values);
        }
    }
}
