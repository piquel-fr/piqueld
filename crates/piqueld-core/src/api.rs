//! Shared requests and responses for the versioned HTTP API.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::manifest::ApplicationManifest;
use crate::{ApplicationState, Convergence, NormalizedApplication, Operation, Plan};

/// Versioned prefix used by all API endpoints.
pub const API_PREFIX: &str = "/api/v1";

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
/// Public application state returned by the API.
pub struct ApplicationView {
    /// Normalized application manifest.
    pub application: NormalizedApplication,
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
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
/// Operation accepted by an application mutation endpoint.
pub struct AcceptedOperation {
    /// Asynchronous operation identifier.
    pub operation_id: String,
    /// Stable application identifier.
    pub application_id: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
/// Dry-run plan returned by the API.
pub struct PlanView {
    /// Stable application identifier.
    pub application_id: String,
    /// Ordered runtime plan.
    pub plan: Plan,
}

#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
/// Current application reconciliation status.
pub struct ApplicationStatusView {
    /// Stable application identifier.
    pub application_id: String,
    /// Machine-readable lifecycle state.
    pub state: ApplicationState,
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
