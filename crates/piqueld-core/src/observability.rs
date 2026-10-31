//! Stable, sanitized observability contracts shared by daemon and clients.
pub use crate::names::{DestinationName, DestinationNameError};
use crate::{ApplicationId, names::string_enum};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use utoipa::ToSchema;

string_enum! {
    #[serde]
    /// Ownership of historical data, independent of its contextual application and environment.
    #[derive(Default)]
    pub enum EventScope {
        /// Removed when the application is deleted.
        #[default]
        Application => "application",
        /// Retained under the daemon history policy.
        Daemon => "daemon",
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
    /// Application ID: its own events and those of all its environments.
    pub application_id: Option<String>,
    /// Contextual environment ID, including references retained in daemon history.
    pub environment_id: Option<String>,
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
    /// `SQLite` write-ahead log size.
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

string_enum! {
    #[serde]
    /// What a notification reports. Each destination chooses the categories it receives.
    #[derive(Hash, Ord, PartialOrd)]
    pub enum NotificationCategory {
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
        /// Changes and activity that affect who can access the daemon: new
        /// administrators, long-lived or administrator tokens, admin
        /// recovery, refused-request bursts, and credentials used from new
        /// addresses.
        Security => "security",
        /// Each daemon start, with its version, once it is ready to serve.
        DaemonStarted => "daemon_started",
        /// Each successful deployment. New destinations do not receive it.
        DeploymentSucceeded => "deployment_succeeded",
    }
}
impl NotificationCategory {
    /// What its notifications report, for people: `Build failures`.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::BuildFailures => "Build failures",
            Self::DeploymentFailures => "Deployment failures",
            Self::ServiceDegradation => "Service degradation",
            Self::DaemonFailures => "Daemon failures",
            Self::Recovery => "Recoveries",
            Self::Security => "Security events",
            Self::DaemonStarted => "Daemon starts",
            Self::DeploymentSucceeded => "Deployments",
        }
    }
}

string_enum! {
    #[serde]
    /// Lifecycle of a durable webhook delivery.
    pub enum DeliveryState {
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

crate::names::validated_string!(
    /// Text sent with every notification of one category, such as a Discord
    /// role mention (`<@&ROLE_ID>`): 1–1000 characters.
    NotificationMessage,
    NotificationMessageError,
    "notification messages must contain 1-1000 characters",
    |value: &str| (1..=NotificationMessage::MAX_CHARS).contains(&value.chars().count())
);

impl NotificationMessage {
    /// Longest message, leaving room for the notification in Discord's
    /// 2000-character message content.
    pub const MAX_CHARS: usize = 1000;
}

string_enum! {
    #[serde]
    /// Payload format a destination receives.
    #[derive(Default)]
    pub enum DestinationKind {
        /// Versioned JSON containing the event.
        #[default]
        Json => "json",
        /// A Discord webhook message.
        Discord => "discord",
    }
}

/// The applications a destination receives notifications about. Daemon-wide
/// notifications, such as daemon failures, security events and daemon starts,
/// reach every destination receiving their category.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ApplicationSelection {
    /// Every application, including ones created later.
    #[default]
    All,
    /// Only these applications, by ID: renaming one keeps it selected, and
    /// deleting one removes it from the list.
    Only(BTreeSet<ApplicationId>),
}

/// How one category is delivered to a destination.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct CategoryRule {
    /// Text sent with each notification of the category. Discord sends it as
    /// the message content, so mentions in it notify.
    pub message: Option<NotificationMessage>,
}

/// What a destination receives and how. Its URL is write-only, see
/// [`DestinationRequest`].
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DestinationSettings {
    /// Payload format.
    pub kind: DestinationKind,
    /// Whether it receives notifications. Disabling cancels pending
    /// deliveries; enabling again never replays what happened meanwhile.
    pub enabled: bool,
    /// Categories it receives. Adding one never replays earlier events.
    pub categories: BTreeMap<NotificationCategory, CategoryRule>,
    /// Applications it receives notifications about.
    pub applications: ApplicationSelection,
    /// Whether it receives notifications about previews.
    pub previews: bool,
}

impl DestinationSettings {
    /// Settings for a new destination: enabled, every application without
    /// previews, and every category but `deployment_succeeded`, without a
    /// message.
    #[must_use]
    pub fn new(kind: DestinationKind) -> Self {
        Self {
            kind,
            enabled: true,
            categories: NotificationCategory::ALL
                .into_iter()
                .filter(|category| *category != NotificationCategory::DeploymentSucceeded)
                .map(|category| (category, CategoryRule::default()))
                .collect(),
            applications: ApplicationSelection::All,
            previews: false,
        }
    }
}

/// A notification destination; its URL is never returned.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct NotificationDestination {
    /// Unique name.
    pub name: DestinationName,
    /// What it receives and how.
    pub settings: DestinationSettings,
    /// Creation timestamp.
    pub created_at_ms: i64,
    /// Last change timestamp.
    pub updated_at_ms: i64,
}

/// Creates or replaces a destination. Its `Debug` hides the URL, a credential.
#[derive(Clone, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DestinationRequest {
    /// What it receives and how.
    pub settings: DestinationSettings,
    /// Webhook URL: HTTPS, or HTTP to a loopback address. Required to create
    /// a destination; omitted, an existing one keeps its URL. A new URL or
    /// kind makes it a new receiver: pending deliveries are cancelled, and
    /// recoveries of failures sent before are never sent to it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

impl std::fmt::Debug for DestinationRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DestinationRequest")
            .field("settings", &self.settings)
            .field("url", &self.url.as_ref().map(|_| "<redacted>"))
            .finish()
    }
}

/// Outcome of a test notification.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct DestinationTest {
    /// Whether the receiver acknowledged it with a 2xx status.
    pub delivered: bool,
    /// Safe failure summary, such as the receiver's HTTP status, when it was not.
    pub error: Option<String>,
}

/// A retained notification delivery; credentials and receiver response bodies are excluded.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct NotificationDelivery {
    /// Stable delivery identity, preserved through retries.
    pub id: String,
    /// Source event.
    pub event_id: i64,
    /// Name of the destination it was sent to, kept after the destination is deleted.
    pub destination: String,
    /// What it is about, by name, as of when it was queued.
    #[serde(default)]
    pub subject: NotificationSubject,
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

/// What a notification is about, by the names its application, environment
/// or preview had when it was queued; empty for daemon notifications.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(default)]
pub struct NotificationSubject {
    /// The application.
    pub application: Option<crate::ApplicationName>,
    /// The environment, or the preview's slug.
    pub environment: Option<crate::EnvironmentName>,
    /// Whether the environment is a preview.
    pub preview: bool,
    /// The branch the environment or preview deploys, if any.
    pub branch: Option<crate::GitBranch>,
}

impl NotificationSubject {
    /// `shop / production`, `shop`, or `None` for daemon notifications.
    #[must_use]
    pub fn describe(&self) -> Option<String> {
        let application = self.application.as_ref()?;
        Some(match &self.environment {
            Some(environment) => format!("{application} / {environment}"),
            None => application.to_string(),
        })
    }
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

mod diagnostic;
pub use diagnostic::{DeploymentFailure, DiagnosticCode};

#[cfg(test)]
mod tests {
    use super::{DeliveryState, NotificationCategory};
    use utoipa::PartialSchema;

    #[test]
    fn notification_schemas_match_wire_values() {
        for (schema, values) in [
            (
                DeliveryState::schema(),
                serde_json::to_value(DeliveryState::ALL).unwrap(),
            ),
            (
                NotificationCategory::schema(),
                serde_json::to_value(NotificationCategory::ALL).unwrap(),
            ),
        ] {
            assert_eq!(serde_json::to_value(schema).unwrap()["enum"], values);
        }
    }
}
