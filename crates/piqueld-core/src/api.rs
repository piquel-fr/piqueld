//! Shared requests and responses for the versioned HTTP API.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::manifest::{
    ApplicationManifest, ApplicationTemplate, RolloutOrder, RolloutOrderSource, SecretSource,
    VariableValue,
};
use crate::{
    ApplicationId, ApplicationState, Convergence, EnvironmentId, EnvironmentName,
    EnvironmentSource, NormalizedApplication, Operation, Plan,
};
use std::collections::{BTreeMap, BTreeSet};

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
    /// Saved manifest, shared by every environment, with references unresolved.
    pub application: ApplicationTemplate,
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

/// Renames an environment, conditioned on the inspected application revision.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentRequest {
    /// Environment name, unique within the application.
    pub name: String,
    /// Current application revision, required unless explicitly forced.
    pub expected_generation: Option<u64>,
}

/// Creates an environment, conditioned on the inspected application revision.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct CreateEnvironmentRequest {
    /// Environment name, unique within the application.
    pub name: String,
    /// Branch of the application's manifest repository to follow. Defaults to
    /// the branch `spec.manifest` names; only for repository-backed applications.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Full commit to pin instead of following `branch`'s head; requires `branch`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Current application revision, required unless explicitly forced.
    pub expected_generation: Option<u64>,
}

/// Points an environment of a repository-backed application at another
/// branch, or pins or unpins its commit, conditioned on the inspected
/// application revision.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentBranchRequest {
    /// Branch of the application's manifest repository to follow.
    pub branch: String,
    /// Full commit to pin instead of following the branch head.
    #[serde(default)]
    pub commit: Option<String>,
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
    /// The value of every variable in scope for the selected environment,
    /// keyed by reference, e.g. `vars.domain`. Empty when none is selected.
    #[serde(default)]
    pub variables: BTreeMap<String, VariableValue>,
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
    /// The manifest this environment deploys, with references unresolved: the
    /// last one fetched from its branch, or the application's saved manifest.
    /// Absent until a repository-backed environment's first fetch.
    pub manifest: Option<ApplicationTemplate>,
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
    /// DNS providers and the certificates issued through them with DNS-01.
    #[serde(default)]
    pub dns: DnsStatus,
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
    /// Manifest captured for this deployment, with references unresolved.
    pub template: ApplicationTemplate,
    /// Values its references rendered to, keyed by reference.
    pub variables: BTreeMap<String, VariableValue>,
    /// Rendered configuration; absent until a repository-backed manifest is fetched.
    pub application: Option<NormalizedApplication>,
    /// Problems found while fetching the manifest that did not stop the
    /// deployment, e.g. `manifest_connection_ignored`.
    #[serde(default)]
    pub warnings: Vec<DiagnosticView>,
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
    pub source: crate::manifest::ValidatedSource,
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
    /// Logical name.
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

/// The environments that may mount a stored secret.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentAccess {
    /// Every environment of the application, including ones created later.
    #[default]
    All,
    /// Only these environments, by ID: renaming one keeps its access, and
    /// deleting one removes it from the list.
    Only(BTreeSet<EnvironmentId>),
}

impl EnvironmentAccess {
    /// Query value for `All`. Environment IDs are at least 8 characters, so
    /// it never names one.
    const ALL_QUERY: &str = "all";

    /// Encodes the list as one query parameter: `all`, or comma-separated
    /// environment IDs (empty for none).
    #[must_use]
    pub fn to_query(&self) -> String {
        match self {
            Self::All => Self::ALL_QUERY.to_owned(),
            Self::Only(ids) => ids
                .iter()
                .map(EnvironmentId::as_str)
                .collect::<Vec<_>>()
                .join(","),
        }
    }

    /// Decodes [`Self::to_query`]'s encoding.
    ///
    /// ```
    /// use piqueld_core::api::EnvironmentAccess;
    /// for access in ["all", "", "env-00000001,env-00000002"] {
    ///     assert_eq!(EnvironmentAccess::from_query(access).unwrap().to_query(), access);
    /// }
    /// ```
    ///
    /// # Errors
    /// Returns the error for a malformed environment ID.
    pub fn from_query(value: &str) -> Result<Self, crate::EnvironmentIdError> {
        if value == Self::ALL_QUERY {
            return Ok(Self::All);
        }
        value
            .split(',')
            .filter(|id| !id.is_empty())
            .map(EnvironmentId::parse)
            .collect::<Result<_, _>>()
            .map(Self::Only)
    }
}

/// Who may mount a secret from an application's store. The default is every
/// environment and no previews.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SecretAccess {
    /// Environments that may mount the secret.
    pub environments: EnvironmentAccess,
    /// Whether previews may mount the secret. Stored for previews, which do
    /// not exist yet.
    #[serde(default)]
    pub previews: bool,
}

impl SecretAccess {
    /// Whether `environment` may mount the secret.
    #[must_use]
    pub fn allows(&self, environment: &EnvironmentId) -> bool {
        match &self.environments {
            EnvironmentAccess::All => true,
            EnvironmentAccess::Only(allowed) => allowed.contains(environment),
        }
    }

    /// The access in words, naming environments by their current name:
    /// `every environment`, `production, staging and previews`, `no environment`.
    #[must_use]
    pub fn describe(&self, environments: &[EnvironmentView]) -> String {
        let named = match &self.environments {
            EnvironmentAccess::All => "every environment".to_owned(),
            EnvironmentAccess::Only(allowed) if allowed.is_empty() => "no environment".to_owned(),
            EnvironmentAccess::Only(allowed) => allowed
                .iter()
                .map(|id| {
                    environments
                        .iter()
                        .find(|environment| environment.id == *id)
                        .map_or_else(
                            || id.to_string(),
                            |environment| environment.name.to_string(),
                        )
                })
                .collect::<Vec<_>>()
                .join(", "),
        };
        if self.previews {
            format!("{named} and previews")
        } else {
            named
        }
    }
}

/// A manually set secret in an application's store: its metadata and access,
/// never its value.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
pub struct StoredSecret {
    /// Version metadata.
    #[serde(flatten)]
    pub metadata: SecretMetadata,
    /// Who may mount the secret.
    pub access: SecretAccess,
}

/// Where the value of a secret an environment mounts comes from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MountedSecret {
    /// Generated for the environment, because `spec.secrets` declares it.
    Generated,
    /// The application store's secret, which the environment may mount.
    Stored {
        /// Current version.
        generation: i64,
    },
    /// Neither declared nor set in the application store: deploying fails.
    Missing,
    /// The application store's secret, whose access list excludes the
    /// environment: deploying fails with `secret_access_denied`.
    Denied,
    /// The application store's secret, whose current value key recovery
    /// discarded: deploying fails with `secret_unavailable` until it is replaced.
    Unavailable,
}

impl MountedSecret {
    /// Each secret `template`, the manifest `environment` deploys, mounts
    /// there, given the application's `stored` secrets.
    #[must_use]
    pub fn list(
        template: &ApplicationTemplate,
        environment: &EnvironmentView,
        stored: &[StoredSecret],
    ) -> Vec<(String, Self)> {
        template
            .mounted_secrets(&environment.name)
            .into_iter()
            .map(|(name, source)| {
                let mounted = match source {
                    SecretSource::Generated => Self::Generated,
                    SecretSource::Stored => {
                        match stored.iter().find(|secret| secret.metadata.name == name) {
                            None => Self::Missing,
                            Some(secret) if !secret.access.allows(&environment.id) => Self::Denied,
                            Some(secret) if secret.metadata.unavailable => Self::Unavailable,
                            Some(secret) => Self::Stored {
                                generation: secret.metadata.generation,
                            },
                        }
                    }
                };
                (name, mounted)
            })
            .collect()
    }
}

impl std::fmt::Display for MountedSecret {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Generated => formatter.write_str("generated for this environment"),
            Self::Stored { generation } => {
                write!(formatter, "application store, version {generation}")
            }
            Self::Missing => formatter.write_str("not set in the application store"),
            Self::Denied => formatter.write_str("application store, not allowed here"),
            Self::Unavailable => {
                formatter.write_str("application store, value discarded; replace it")
            }
        }
    }
}

/// Metadata-only result of lost-key recovery. Never rotates application credentials.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, utoipa::ToSchema)]
pub struct SecretKeyRecovery {
    /// Environments whose generated values were discarded.
    pub affected_environments: i64,
    /// Applications whose stored values were discarded.
    #[serde(default)]
    pub affected_applications: i64,
    /// Logical secrets whose stored values were discarded.
    pub affected_secrets: i64,
    /// Stored versions discarded; already unavailable versions are excluded.
    pub discarded_versions: i64,
}

/// Managed gateway health, per listener, and route diagnostics.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct IngressStatus {
    /// Effective read-only daemon setting.
    pub enabled: bool,
    /// Whether the gateway has accepted its desired configuration (or is
    /// stopped), and in tunnel mode whether the tunnel is connected. Public
    /// routes depend only on this.
    pub healthy: bool,
    /// Safe diagnostic, with detailed causes in daemon logs.
    pub message: String,
    /// How public routes reach the gateway.
    #[serde(default)]
    pub public: PublicIngressStatus,
    /// The private listener and the apps tailnet node carrying its traffic.
    #[serde(default)]
    pub private: PrivateIngressStatus,
    /// Deployed routes and their latest independent HTTPS probes, limited to
    /// applications the caller can read.
    pub routes: Vec<RouteStatus>,
}

/// How public routes reach the gateway: `[ingress.tunnel]` selects the mode.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum PublicIngressStatus {
    /// The gateway publishes ports 80 and 443 on the host.
    #[default]
    Direct,
    /// `cloudflared` connects out to Cloudflare, which terminates TLS and
    /// forwards to the gateway. No inbound port is open.
    Tunnel {
        /// Tunnel ID from its credentials file.
        id: String,
        /// Connections to Cloudflare's edge, from `cloudflared`'s `/ready`;
        /// zero while the tunnel is disconnected.
        connections: u32,
        /// Safe diagnostic, with detailed causes in daemon logs.
        message: String,
    },
}

/// The gateway's private listener, reached through the apps tailnet node.
/// A broken node degrades only private routes.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct PrivateIngressStatus {
    /// Effective read-only daemon setting, `[ingress.private] enabled`.
    pub enabled: bool,
    /// The node is logged in and the gateway serves the private listener.
    pub healthy: bool,
    /// Tailscale backend state of the node, such as `Running` or `NeedsLogin`.
    pub state: String,
    /// The node's fully qualified `MagicDNS` name.
    pub dns_name: Option<String>,
    /// The node's tailnet addresses, which private hostnames must resolve to.
    pub addresses: Vec<String>,
    /// Safe diagnostic: what to do while the node needs login (its login URL
    /// is only logged), or the failed step, with detailed causes in daemon
    /// logs.
    pub message: String,
}

/// The records a route's hostname needs. piqueld does not create them;
/// operators copy them to their DNS provider.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DnsRecords {
    /// Public routes: A/AAAA records to this server's public addresses.
    ServerAddresses,
    /// Public routes in tunnel mode: a proxied CNAME record to the tunnel.
    TunnelCname {
        /// `<tunnel-id>.cfargotunnel.com`.
        target: String,
    },
    /// Private routes: the apps tailnet node's addresses, empty until it has
    /// joined the tailnet.
    TailnetAddresses {
        /// IPv4 and IPv6 tailnet addresses.
        addresses: Vec<String>,
    },
}

/// `A/AAAA -> this server's public addresses`, the tunnel's CNAME, or the
/// tailnet addresses.
impl std::fmt::Display for DnsRecords {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ServerAddresses => {
                formatter.write_str("A/AAAA -> this server's public addresses")
            }
            Self::TunnelCname { target } => write!(formatter, "CNAME (proxied) -> {target}"),
            Self::TailnetAddresses { addresses } if addresses.is_empty() => {
                formatter.write_str("A/AAAA -> the apps node's tailnet addresses, once it joins")
            }
            Self::TailnetAddresses { addresses } => {
                write!(formatter, "A/AAAA -> {}", addresses.join(", "))
            }
        }
    }
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

/// DNS providers from daemon TOML and the DNS-01 certificates piqueld issues
/// through them.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct DnsStatus {
    /// Configured providers, in configuration order.
    pub providers: Vec<DnsProviderStatus>,
    /// Certificates that routes need, or that are kept until they expire.
    pub certificates: Vec<CertificateStatus>,
}

/// Zone discovery state of one configured DNS provider.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct DnsProviderStatus {
    /// Provider kind, such as `cloudflare` or `ovh`.
    pub kind: String,
    /// Zones discovered through the provider's API, including conflicting ones.
    pub zones: Vec<String>,
    /// The latest discovery succeeded and no zone is claimed by another provider.
    pub healthy: bool,
    /// Safe diagnostic: the discovery error or the conflicting zones.
    pub message: String,
}

/// One certificate issued through DNS-01, which covers a single name.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct CertificateStatus {
    /// Covered name: a wildcard such as `*.example.com`, or an exact hostname.
    pub name: String,
    /// Route hostnames served with it; empty once no route needs it.
    pub hostnames: Vec<String>,
    /// Expiry of the stored certificate, absent until one is issued.
    pub expires_at_ms: Option<i64>,
    /// Latest issuance or renewal failure, cleared by the next success.
    pub error: Option<String>,
}

/// HTTPS readiness is separate from application rollout success.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct RouteStatus {
    /// Owning environment identity.
    pub environment_id: String,
    /// Exact public DNS hostname.
    pub hostname: String,
    /// Effective visibility, which selects the listener serving the route.
    pub visibility: crate::manifest::Visibility,
    /// The records the hostname needs.
    pub dns: DnsRecords,
    /// Backend service or redirect.
    #[serde(flatten)]
    pub target: crate::manifest::RouteTarget,
    /// disabled, pending, ready, or failed.
    pub state: String,
    /// Diagnostic explaining DNS, TLS, or gateway readiness.
    pub message: String,
}

#[cfg(test)]
mod environment_tests {
    use super::{
        ApplicationView, EnvironmentAccess, EnvironmentView, MountedSecret, SecretAccess,
        SecretMetadata, StoredSecret,
    };
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
        let manifest = crate::manifest::parse_template_toml(
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

    /// Each mounted secret reports what its next deployment would find, so
    /// failures show before deploying.
    #[test]
    fn mounted_secrets_report_what_deploying_would_find() {
        let manifest = crate::manifest::parse_template_toml(
            r"api_version='piqueld.dev/v1alpha1'
kind='Application'
[metadata]
name='notes'
[[spec.services]]
name='web'
source={type='image',image='nginx:alpine'}
secrets=[{name='session',target='/run/secrets/1'},{name='stripe',target='/run/secrets/2'},{name='other',target='/run/secrets/3'},{name='discarded',target='/run/secrets/4'},{name='unset',target='/run/secrets/5'}]
[[spec.secrets]]
name='session'
generate={type='random',bytes=16}
",
        )
        .unwrap()
        .normalize(ApplicationId::parse("app-notes-01").unwrap());
        let production = environment("app-notes-01", "production");
        let stored = |name: &str, environments, unavailable| StoredSecret {
            access: SecretAccess {
                environments,
                previews: false,
            },
            metadata: SecretMetadata {
                name: name.into(),
                generation: 3,
                updated_at_ms: 1,
                deleting: false,
                unavailable,
            },
        };
        let others =
            EnvironmentAccess::Only([EnvironmentId::parse("env-staging-01").unwrap()].into());
        let stored = [
            stored("stripe", EnvironmentAccess::All, false),
            stored("other", others, false),
            stored("discarded", EnvironmentAccess::All, true),
        ];
        assert_eq!(
            MountedSecret::list(&manifest, &production, &stored),
            [
                ("discarded".into(), MountedSecret::Unavailable),
                ("other".into(), MountedSecret::Denied),
                ("session".into(), MountedSecret::Generated),
                ("stripe".into(), MountedSecret::Stored { generation: 3 }),
                ("unset".into(), MountedSecret::Missing),
            ]
        );
    }
}
