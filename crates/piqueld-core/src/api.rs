//! Shared requests and responses for the versioned HTTP API.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::manifest::ApplicationManifest;
use crate::{ApplicationState, Convergence, NormalizedApplication, Operation, Plan};

/// Versioned prefix used by all API endpoints.
pub const API_PREFIX: &str = "/api/v1";
/// Maximum number of application summaries returned in one page.
pub const MAX_APPLICATION_PAGE_SIZE: u16 = 100;

/// Successful API response envelope.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Envelope<T> {
    /// Response payload.
    pub data: T,
}

/// Cursor-paginated API response.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct Page<T> {
    /// Items in this page.
    pub items: Vec<T>,
    /// Cursor for the next page, when more items are available.
    pub next_cursor: Option<String>,
}

/// Structured error returned by the API.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct ErrorBody {
    /// Stable machine-readable error code.
    pub code: String,
    /// Safe human-readable error message.
    pub message: String,
    /// Optional structured error details.
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub details: serde_json::Value,
    /// Server-generated request identifier.
    #[serde(default)]
    #[schema(required = true)]
    pub request_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
/// Metadata returned when listing applications.
pub struct ApplicationSummary {
    /// Stable application identifier.
    pub id: crate::ApplicationId,
    /// Editable application name.
    pub name: String,
    /// Current manifest or deletion-intent revision.
    pub generation: u64,
    /// Revision of the last completely resolved target, not a convergence guarantee.
    pub resolved_generation: Option<u64>,
    /// Whether deletion has been requested.
    pub delete_intent: bool,
    /// Creation timestamp in Unix milliseconds.
    pub created_at_ms: i64,
    /// Last update timestamp in Unix milliseconds.
    pub updated_at_ms: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
/// Public application state returned by the API.
pub struct ApplicationView {
    /// Normalized application manifest.
    pub application: NormalizedApplication,
    /// Current manifest or deletion-intent revision.
    pub generation: u64,
    /// Revision of the last completely resolved target, not a convergence guarantee.
    pub resolved_generation: Option<u64>,
    /// Hash of the normalized desired specification.
    pub spec_hash: String,
    /// Whether deletion has been requested.
    pub delete_intent: bool,
    /// Creation timestamp in Unix milliseconds.
    pub created_at_ms: i64,
    /// Last update timestamp in Unix milliseconds.
    pub updated_at_ms: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
/// Desired application state to apply or preview.
pub struct ApplyApplicationRequest {
    /// Application configuration to save or preview. Deployment is an explicit query option.
    pub manifest: ApplicationManifest,
    /// Required for apply unless forced; optional for preview. Zero requires an absent name.
    #[serde(default)]
    pub expected_generation: Option<u64>,
    /// Required for non-forced updates by name; protects against name reuse.
    #[serde(default)]
    pub expected_application_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
/// Operation accepted by an application mutation endpoint.
pub struct AcceptedOperation {
    /// Asynchronous operation identifier.
    pub operation_id: String,
    /// Stable application identifier.
    pub application_id: String,
    /// Accepted intent revision.
    pub generation: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
/// Dry-run plan returned by the API.
pub struct PlanView {
    /// Stable application identifier.
    pub application_id: String,
    /// Current intent revision; zero means the name is absent.
    pub generation: u64,
    /// Whether this specification matches the latest deployment snapshot.
    pub identical: bool,
    /// Latest operation at the time of comparison.
    pub operation: Option<Operation>,
    /// Safe changes relative to the latest deployment snapshot.
    pub changes: Vec<ManifestChange>,
    /// Ordered runtime plan; unresolved images are explicit actions.
    pub plan: Plan,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
/// Current application reconciliation status.
pub struct ApplicationStatusView {
    /// Stable application identifier.
    pub application_id: String,
    /// Machine-readable lifecycle state.
    pub state: ApplicationState,
    /// Observed runtime health, independent of operation progress.
    pub runtime_health: Option<String>,
    /// Optional safe status message.
    pub message: Option<String>,
    /// Last update timestamp in Unix milliseconds.
    pub updated_at_ms: i64,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
/// Sanitized runtime diagnostic shown by the read-only dashboard.
pub struct DiagnosticView {
    /// Stable diagnostic category.
    pub code: String,
    /// Bounded, actionable message safe for a browser.
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
/// Observed service state summarized for browser and operator clients.
pub struct ObservedServiceView {
    /// Logical service name from the desired application.
    pub name: String,
    /// Observed immutable image reference, when the service exists.
    pub image: Option<String>,
    /// Desired replica count from the application manifest.
    pub desired_replicas: u16,
    /// Replicas currently reported by the runtime.
    pub observed_replicas: u16,
    /// Replicas that are running and healthy enough to serve traffic.
    pub healthy_replicas: u16,
    /// Runtime convergence category.
    pub convergence: Convergence,
    /// Sanitized task and service diagnostics.
    pub diagnostics: Vec<DiagnosticView>,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
/// Bounded observed runtime state for one application.
pub struct ObservedApplicationView {
    /// Observed services in desired service order.
    pub services: Vec<ObservedServiceView>,
    /// Number of owned networks observed.
    pub network_count: u32,
    /// Number of owned volumes observed.
    pub volume_count: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
/// Read-only application detail composed at the API boundary.
pub struct ApplicationDetailView {
    /// Desired application.
    pub application: ApplicationView,
    /// Durable application lifecycle status.
    pub status: ApplicationStatusView,
    /// Sanitized runtime observation.
    pub observed: ObservedApplicationView,
    /// Most recent durable operation, when one exists.
    pub latest_operation: Option<Operation>,
    /// Bounded diagnostics from status, runtime, and the latest operation.
    pub diagnostics: Vec<DiagnosticView>,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
/// Current control-plane status.
pub struct SystemStatus {
    /// Machine-readable service status.
    pub status: String,
    /// Version of the exposed API.
    pub api_version: String,
    /// Version of the running daemon binary.
    pub daemon_version: String,
    /// Control-plane instance identifier.
    pub instance_id: String,
}

/// A change to a manifest field. Environment and process values are redacted.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct ManifestChange {
    /// Logical manifest path.
    pub field: String,
    /// Safe previous value; absent for additions.
    pub before: Option<String>,
    /// Safe proposed value; absent for removals.
    pub after: Option<String>,
}

/// Metadata-only rename, conditioned on the inspected intent revision.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RenameApplicationRequest {
    /// New unique application name.
    pub name: String,
    /// Current intent revision, required unless explicitly forced.
    pub expected_generation: Option<u64>,
}

/// Completed metadata-only rename.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct RenamedApplication {
    /// Unchanged application identity.
    pub application_id: String,
    /// Current application name.
    pub name: String,
    /// Revision after renaming.
    pub generation: u64,
}

impl From<&Operation> for AcceptedOperation {
    fn from(operation: &Operation) -> Self {
        Self {
            operation_id: operation.id.clone(),
            application_id: operation.application_id.to_string(),
            generation: operation.generation,
        }
    }
}

/// Saved application configuration, optionally accompanied by a deployment.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct SavedApplication {
    /// Stable application identity.
    pub application_id: String,
    /// Saved configuration revision.
    pub generation: u64,
    /// Deployment operation, only when explicitly requested.
    pub operation_id: Option<String>,
}

/// Durable deployment snapshot and execution summary.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct DeploymentView {
    /// Execution ID also identifies this deployment.
    pub operation: Operation,
    /// Configuration captured when deployment was accepted.
    pub application: NormalizedApplication,
    /// First successful convergence, retained during later drift repair.
    pub succeeded_at_ms: Option<i64>,
    /// Whether this is the currently promoted runtime target.
    pub current_target: bool,
    /// Whether this is the most recent deployment to converge successfully.
    pub last_successful: bool,
}

/// Effective host settings loaded by the daemon; no mutation endpoint exists.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct HostConfiguration {
    /// Settings grouped by server, Docker, reconciliation and retention.
    pub groups: std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>,
}

/// Bounded historical container output read directly from Docker.
#[derive(Clone, Debug, Default, Deserialize, Serialize, ToSchema)]
pub struct ApplicationLogs {
    /// Chronologically ordered records.
    pub items: Vec<LogRecord>,
    /// More output existed than the requested window or safety limit.
    pub truncated: bool,
}
/// A selectable process output stream. Merged terminal output is only included without a filter.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize, Serialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum LogStream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}
impl LogStream {
    /// Wire and storage representation.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}

/// One line of workload output with replica identity.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct LogRecord {
    /// Logical service name.
    pub service: String,
    /// Swarm task identity.
    pub task_id: String,
    /// Docker timestamp, when supplied.
    pub timestamp: String,
    /// stdout, stderr, or console.
    pub stream: String,
    /// Text with terminal control sequences removed.
    pub message: String,
}

impl LogRecord {
    /// Removes terminal control sequences, including CSI colors and OSC titles/links.
    #[must_use]
    pub fn clean_message(value: &str) -> String {
        let mut output = String::new();
        let mut chars = value.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\u{1b}' {
                match chars.next() {
                    Some('[') => {
                        for c in chars.by_ref() {
                            if ('@'..='~').contains(&c) {
                                break;
                            }
                        }
                    }
                    Some(']' | 'P' | '^' | '_') => {
                        while let Some(c) = chars.next() {
                            if c == '\u{7}' || (c == '\u{1b}' && chars.next_if_eq(&'\\').is_some())
                            {
                                break;
                            }
                        }
                    }
                    _ => {}
                }
            } else if !ch.is_control() || matches!(ch, '\t' | '\n') {
                output.push(ch);
            }
        }
        output
    }
}

/// One diagnostic dependency probe, independent of application health.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum DependencyStatus {
    /// The dependency is usable.
    Ready,
    /// The probe failed with a safe explanation.
    Failed {
        /// Explanation when the probe fails.
        message: String,
    },
}
impl DependencyStatus {
    /// Creates a dependency verdict with a safe failure explanation.
    #[must_use]
    pub fn new(ready: bool, failure: &str) -> Self {
        if ready {
            Self::Ready
        } else {
            Self::Failed {
                message: failure.to_owned(),
            }
        }
    }
}
/// Deployment prerequisites. Configuration APIs remain available when false.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct ReadinessStatus {
    /// All deployment dependencies are ready.
    pub ready: bool,
    /// SQLite can execute a read query.
    pub database: DependencyStatus,
    /// Docker answers its ping endpoint.
    pub docker: DependencyStatus,
    /// Docker is a compatible single-node Swarm manager.
    pub swarm: DependencyStatus,
}

/// Durable source-preparation attempt, independent of the build executor.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct BuildRecord {
    /// Monotonic build identifier.
    pub id: i64,
    /// Owning application ID.
    pub application_id: String,
    /// Source operation ID, retained even after operation pruning.
    pub operation_id: String,
    /// Logical service name.
    pub service: String,
    /// Requested build source.
    pub source: crate::manifest::Source,
    /// Current attempt outcome.
    pub state: BuildState,
    /// Start time in Unix milliseconds.
    pub started_at_ms: i64,
    /// Completion time, absent while running.
    pub finished_at_ms: Option<i64>,
    /// Resolved Git commit, when checkout completed.
    pub commit: Option<String>,
    /// Built image identifier, when successful.
    pub image_id: Option<String>,
    /// Number of currently retained output bytes.
    pub log_bytes: i64,
    /// Output exceeded the configured per-build cap.
    pub log_truncated: bool,
    /// Output was removed by retention.
    pub log_expired: bool,
}
/// Outcome of a source-preparation attempt.
#[derive(
    Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum BuildState {
    /// Source preparation is running.
    Running,
    /// An image was produced successfully.
    Succeeded,
    /// Checkout or build execution failed.
    Failed,
    /// Execution was cancelled or interrupted by daemon shutdown.
    Interrupted,
}
/// Bounded byte-offset page of build output.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct BuildLogPage {
    /// Structured output chunks in chronological order.
    pub items: Vec<BuildLogChunk>,
    /// Exclusive byte cursor for loading an older page.
    pub previous_offset: Option<i64>,
    /// The build exceeded its total output cap.
    pub truncated: bool,
    /// Retention removed the output.
    pub expired: bool,
}

/// Captured output with stream identity and capture time.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct BuildLogChunk {
    /// Byte position in the retained output.
    pub offset: i64,
    /// Capture time in Unix milliseconds.
    pub timestamp_ms: i64,
    /// Captured source stream.
    pub stream: LogStream,
    /// Lossy UTF-8 output, possibly containing partial lines.
    pub text: String,
}

#[cfg(test)]
mod log_tests {
    use super::LogRecord;
    #[test]
    fn terminal_controls_are_removed_but_text_and_lines_survive() {
        assert_eq!(
            LogRecord::clean_message("\x1b[31mred\x1b[0m\r\0\ttext\n"),
            "red\ttext\n"
        );
        assert_eq!(
            LogRecord::clean_message(
                "\x1b]0;hidden title\x07text \x1b]8;;https://example.test\x1b\\link\x1b]8;;\x1b\\"
            ),
            "text link"
        );
    }
}

/// Metadata only: secret values are never returned.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct SecretMetadata {
    /// Application-scoped logical name.
    pub name: String,
    /// Current version, used for optimistic writes.
    pub generation: i64,
    /// Last update time in Unix milliseconds.
    pub updated_at_ms: i64,
    /// Cleanup has started; retry deletion to finish it. Replacement is disabled.
    #[serde(default)]
    pub deleting: bool,
    /// The current value was discarded during key recovery; supply a new version.
    #[serde(default)]
    pub unavailable: bool,
}

/// Replace the daemon-wide storage encryption key. Never rotates credentials.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ReplaceSecretKeyRequest {
    /// Explicitly discard all stored values instead of decrypting and preserving them.
    #[serde(default)]
    pub discard_values: bool,
}

/// Metadata-only result of a daemon-wide key replacement.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct SecretKeyReplacement {
    /// Whether stored values were deliberately discarded.
    pub discarded_values: bool,
    /// Applications with retained values affected by this operation.
    pub affected_applications: i64,
    /// Logical secrets with retained values affected by this operation.
    pub affected_secrets: i64,
    /// Retained values re-encrypted or discarded; unavailable versions are excluded.
    pub affected_versions: i64,
}
