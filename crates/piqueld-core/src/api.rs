//! Shared requests and responses for the versioned HTTP API.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::manifest::{ApplicationManifest, RolloutOrder, RolloutOrderSource};
use crate::{
    ApplicationId, ApplicationState, Convergence, EnvironmentId, EnvironmentName,
    EnvironmentSource, NormalizedApplication, Operation, Plan,
};

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
    pub id: ApplicationId,
    /// Editable application name.
    pub name: String,
    /// Current manifest or deletion-intent revision.
    pub generation: u64,
    /// Whether deletion has been requested.
    pub delete_intent: bool,
    /// Creation timestamp in Unix milliseconds.
    pub created_at_ms: i64,
    /// Last update timestamp in Unix milliseconds.
    pub updated_at_ms: i64,
    /// Environments in name order.
    pub environments: Vec<EnvironmentView>,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
/// Public application state returned by the API.
pub struct ApplicationView {
    /// Normalized application manifest, shared by every environment.
    pub application: NormalizedApplication,
    /// Current manifest or deletion-intent revision.
    pub generation: u64,
    /// Hash of the normalized desired specification.
    pub spec_hash: String,
    /// Whether deletion has been requested.
    pub delete_intent: bool,
    /// Creation timestamp in Unix milliseconds.
    pub created_at_ms: i64,
    /// Last update timestamp in Unix milliseconds.
    pub updated_at_ms: i64,
    /// Environments in name order.
    pub environments: Vec<EnvironmentView>,
}

impl ApplicationView {
    /// Selects the environment runtime commands use when none is named: the
    /// application's only environment.
    ///
    /// # Errors
    ///
    /// Returns the environment names when there is not exactly one, so callers
    /// never pick an environment silently.
    pub fn sole_environment(&self) -> Result<&EnvironmentView, Vec<EnvironmentName>> {
        match self.environments.as_slice() {
            [environment] => Ok(environment),
            environments => Err(environments
                .iter()
                .map(|environment| environment.name.clone())
                .collect()),
        }
    }

    /// Finds an environment by stable ID, then by name. IDs win because a name
    /// can equal another environment's ID.
    #[must_use]
    pub fn environment(&self, id_or_name: &str) -> Option<&EnvironmentView> {
        let environments = &self.environments;
        environments
            .iter()
            .find(|environment| environment.id.as_str() == id_or_name)
            .or_else(|| {
                environments
                    .iter()
                    .find(|environment| environment.name.as_str() == id_or_name)
            })
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
/// A deployable unit of an application, with its own deployment history,
/// status, volumes, generated secrets, routes, and Docker network.
pub struct EnvironmentView {
    /// Stable environment identifier; runtime names derive from it.
    pub id: EnvironmentId,
    /// Owning application.
    pub application_id: ApplicationId,
    /// Name, unique within the application.
    pub name: EnvironmentName,
    /// Where deployments come from.
    pub source: EnvironmentSource,
    /// Application revision of the last completely resolved target, not a
    /// convergence guarantee.
    pub resolved_generation: Option<u64>,
    /// Whether deletion has been requested.
    pub delete_intent: bool,
    /// Creation timestamp in Unix milliseconds.
    pub created_at_ms: i64,
    /// Last update timestamp in Unix milliseconds.
    pub updated_at_ms: i64,
}

/// Creates or renames an environment, conditioned on the inspected application revision.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentRequest {
    /// Environment name, unique within the application.
    pub name: String,
    /// Current application revision, required unless explicitly forced.
    pub expected_generation: Option<u64>,
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
/// Operation accepted by a mutation endpoint.
pub struct AcceptedOperation {
    /// Asynchronous operation identifier.
    pub operation_id: String,
    /// Environment the operation changes. Receipts stored before environments
    /// existed name it `application_id`.
    #[serde(alias = "application_id")]
    pub environment_id: String,
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
    /// Whether this specification matches the baseline: the latest deployment
    /// snapshot of the selected environment (by default the application's only
    /// one), or the saved configuration when none is selected and it has several.
    pub identical: bool,
    /// Latest operation of the selected environment at the time of comparison.
    pub operation: Option<Operation>,
    /// Safe changes relative to the baseline.
    pub changes: Vec<ManifestChange>,
    /// Ordered runtime plan for the selected environment; unresolved images are
    /// explicit actions. Empty when no environment is selected.
    pub plan: Plan,
    /// Effective rollout of each service, sorted by service name.
    pub rollouts: Vec<ServiceRolloutView>,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
/// A service's effective rollout order and monitor window.
pub struct ServiceRolloutView {
    /// Logical service name.
    pub service: String,
    /// Effective update order.
    pub order: RolloutOrder,
    /// Whether the order is set explicitly or derived from the mounts.
    pub order_source: RolloutOrderSource,
    /// Effective monitor window in seconds.
    pub monitor_seconds: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
/// Current environment reconciliation status.
pub struct EnvironmentStatusView {
    /// Stable environment identifier.
    pub environment_id: String,
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
/// Bounded observed runtime state for one environment.
pub struct ObservedApplicationView {
    /// Observed services in desired service order.
    pub services: Vec<ObservedServiceView>,
    /// Number of owned networks observed.
    pub network_count: u32,
    /// Number of owned volumes observed.
    pub volume_count: u32,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
/// Read-only environment detail composed at the API boundary.
pub struct EnvironmentDetailView {
    /// The environment.
    pub environment: EnvironmentView,
    /// Its application and shared desired configuration.
    pub application: ApplicationView,
    /// Durable environment lifecycle status.
    pub status: EnvironmentStatusView,
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
    /// Dedicated tailnet node serving the website over HTTPS.
    #[serde(default)]
    pub tailscale: TailnetStatus,
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

// Summarizes a newly accepted operation for mutation responses.
impl From<&Operation> for AcceptedOperation {
    fn from(operation: &Operation) -> Self {
        Self {
            operation_id: operation.id.clone(),
            environment_id: operation.environment_id.to_string(),
            generation: operation.generation,
        }
    }
}

/// Accepted application deletion: one delete operation per environment.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct DeletedApplication {
    /// Stable application identity.
    pub application_id: String,
    /// Revision after requesting deletion.
    pub generation: u64,
    /// Deletion of each environment; the application disappears with the last one.
    pub operations: Vec<AcceptedOperation>,
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
    /// Other control characters are dropped too, except tabs and newlines.
    ///
    /// ```text
    /// "\x1b[31mred\x1b[0m\r\ttext\n"                -> "red\ttext\n"
    /// "\x1b]0;title\x07text \x1b]8;;url\x1b\\link" -> "text link"
    /// ```
    #[must_use]
    pub fn clean_message(value: &str) -> String {
        let mut output = String::new();
        let mut chars = value.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\u{1b}' {
                match chars.next() {
                    // CSI: parameters run until a final byte in `@..=~`.
                    Some('[') => {
                        for c in chars.by_ref() {
                            if ('@'..='~').contains(&c) {
                                break;
                            }
                        }
                    }
                    // OSC, DCS, PM, APC: strings end at BEL or ST (`ESC \`).
                    Some(']' | 'P' | '^' | '_') => {
                        while let Some(c) = chars.next() {
                            if c == '\u{7}' || (c == '\u{1b}' && chars.next_if_eq(&'\\').is_some())
                            {
                                break;
                            }
                        }
                    }
                    // Other two-byte escapes drop just the escape and its selector.
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
    /// `SQLite` can execute a read query.
    pub database: DependencyStatus,
    /// Docker answers its ping endpoint.
    pub docker: DependencyStatus,
    /// Docker is a compatible single-node Swarm manager.
    pub swarm: DependencyStatus,
    /// Managed ingress is independent of core deployment dependencies.
    #[serde(default)]
    pub ingress: IngressStatus,
}

/// Durable source-preparation attempt or one-shot job run, independent of the executor.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct BuildRecord {
    /// Monotonic build identifier.
    pub id: i64,
    /// Owning environment ID.
    pub environment_id: String,
    /// Source operation ID, retained even after operation pruning.
    pub operation_id: String,
    /// Logical service name; for job runs, the service whose container the job reuses.
    pub service: String,
    /// Logical job name when this record is a job run rather than a source build.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job: Option<String>,
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
    /// Built image identifier, when successful; for job runs, the image that ran.
    pub image_id: Option<String>,
    /// Job container exit code, when the job ran to completion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i64>,
    /// Number of currently retained output bytes.
    pub log_bytes: i64,
    /// Output exceeded the configured per-build cap.
    pub log_truncated: bool,
    /// Output was removed by retention.
    pub log_expired: bool,
}
/// Outcome of a source-preparation attempt or job run.
#[derive(
    Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize, utoipa::ToSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum BuildState {
    /// Source preparation is running.
    Running,
    /// An image was produced successfully, or the job exited with status zero.
    Succeeded,
    /// Checkout or build execution failed, or the job failed or timed out.
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
    /// Environment-scoped logical name.
    pub name: String,
    /// Current version, used for optimistic writes.
    pub generation: i64,
    /// Last update time in Unix milliseconds.
    pub updated_at_ms: i64,
    /// Cleanup has started; retry deletion to finish it. Replacement is disabled.
    #[serde(default)]
    pub deleting: bool,
    /// The current value was discarded by lost-key recovery; supply a new version.
    #[serde(default)]
    pub unavailable: bool,
}

/// Metadata-only result of lost-key recovery. Never rotates application credentials.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct SecretKeyRecovery {
    /// Environments whose stored values were discarded.
    pub affected_environments: i64,
    /// Logical secrets whose stored values were discarded.
    pub affected_secrets: i64,
    /// Stored versions discarded; already unavailable versions are excluded.
    pub discarded_versions: i64,
}

/// Managed gateway health and public route diagnostics.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct IngressStatus {
    /// Effective read-only daemon setting.
    pub enabled: bool,
    /// Whether the gateway has accepted its desired configuration (or is stopped).
    pub healthy: bool,
    /// Safe diagnostic, with detailed causes in daemon logs.
    pub message: String,
    /// Deployed routes and their latest independent HTTPS probes.
    pub routes: Vec<RouteStatus>,
}

/// Login and certificate state of the daemon's own tailnet node.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct TailnetStatus {
    /// Effective read-only daemon setting.
    pub enabled: bool,
    /// The node is logged in and serves an unexpired certificate.
    pub healthy: bool,
    /// Tailscale backend state, such as `Running` or `NeedsLogin`.
    pub state: String,
    /// The node's fully qualified `MagicDNS` name.
    pub dns_name: Option<String>,
    /// Expiry of the served HTTPS certificate.
    pub certificate_expires_at_ms: Option<i64>,
    /// Whether `auth.public_url` is the node's HTTPS origin.
    pub public_url_matches: bool,
    /// Safe diagnostic, with detailed causes in daemon logs.
    pub message: String,
}

/// Public HTTPS readiness is separate from application rollout success.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct RouteStatus {
    /// Owning environment identity.
    pub environment_id: String,
    /// Exact public DNS hostname.
    pub hostname: String,
    /// Backend service or redirect.
    #[serde(flatten)]
    pub target: crate::manifest::RouteTarget,
    /// disabled, pending, ready, or failed.
    pub state: String,
    /// Public diagnostic explaining DNS, TLS, or gateway readiness.
    pub message: String,
}

#[cfg(test)]
mod environment_tests {
    use super::{ApplicationView, EnvironmentView};
    use crate::{ApplicationId, EnvironmentId, EnvironmentName, EnvironmentSource};

    fn environment(id: &str, name: &str) -> EnvironmentView {
        EnvironmentView {
            id: EnvironmentId::parse(id).unwrap(),
            application_id: ApplicationId::parse("app-notes-01").unwrap(),
            name: EnvironmentName::parse(name).unwrap(),
            source: EnvironmentSource::Saved,
            resolved_generation: None,
            delete_intent: false,
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }

    #[test]
    fn stable_ids_select_before_names_that_look_like_them() {
        let manifest = crate::parse_toml(
            "api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='notes'\n[spec]",
        )
        .unwrap()
        .normalize(ApplicationId::parse("app-notes-01").unwrap());
        let view = ApplicationView {
            spec_hash: manifest.spec_hash(),
            application: manifest,
            generation: 1,
            delete_intent: false,
            created_at_ms: 1,
            updated_at_ms: 1,
            // Name order puts the impostor first.
            environments: vec![
                environment("env-impostor-01", "app-notes-01"),
                environment("app-notes-01", "production"),
            ],
        };
        assert_eq!(
            view.environment("app-notes-01").unwrap().name.as_str(),
            "production"
        );
        assert_eq!(
            view.environment("production").unwrap().id.as_str(),
            "app-notes-01"
        );
    }
}
