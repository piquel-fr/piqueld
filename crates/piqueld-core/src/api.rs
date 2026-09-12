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
    /// Application manifest to store and reconcile.
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
    /// Whether this manifest already matches accepted intent (excluding deletion).
    pub identical: bool,
    /// Latest operation at the time of comparison.
    pub operation: Option<Operation>,
    /// Safe changes to accepted manifest fields, independent of Docker availability.
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
