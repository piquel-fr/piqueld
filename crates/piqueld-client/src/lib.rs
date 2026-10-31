//! Typed asynchronous client for the versioned piqueld API.
//!
//! Shared API contracts are reexported from `piqueld-core`. The client runs one
//! shared request pipeline on every target, with platform differences
//! confined inside `client`: TCP and Unix-domain sockets natively,
//! and same-origin browser fetch under WASM.

#[cfg(target_os = "windows")]
compile_error!("Windows is not supported by piqueld-client");

/// Application desired-state and planning contracts.
pub mod applications;
/// Passkey login and account management.
pub mod auth;
/// Environment lifecycle, deployment, and observation contracts.
pub mod environments;
/// One-off commands in running service tasks.
pub mod exec;
/// Durable history and observability controls.
pub mod observability;
/// Generated OpenAPI document retrieval.
pub mod openapi;
/// Operation inspection contracts.
pub mod operations;
/// Preview lifecycle and branch state contracts.
pub mod previews;
/// Deploying on push: environment opt-out and GitHub webhooks.
pub mod sync;
/// Control-plane status contracts.
pub mod system;

mod builds;
mod client;
mod editing;
mod generated;
mod secrets;

/// Permissions and grants.
pub use piqueld_core::access;
/// Audit trail contracts.
pub use piqueld_core::audit;
/// Stable API error and diagnostic codes.
pub use piqueld_core::codes;
/// Typed saved-configuration editing contracts.
pub use piqueld_core::edit;
/// Private links between applications.
pub use piqueld_core::links;
/// Tailnet identities and the token bindings they satisfy.
pub use piqueld_core::tailnet;

pub use applications::{
    AcceptedOperation, ApplicationSummary, ApplicationView, ApplyApplicationRequest, ApplyOptions,
    DeletedApplication, DeploymentView, DiagnosticView, ListApplicationsOptions,
    ObservedApplicationView, ObservedServiceView, PlanView, ReleaseView, RenameApplicationRequest,
    RenamedApplication, RuntimeCoverage, SavedApplication,
};
pub use client::Client;
pub use environments::{
    AcceptedPromotion, CreateEnvironmentRequest, DeploymentOrigin, EnvironmentBranchRequest,
    EnvironmentDetailView, EnvironmentName, EnvironmentRequest, EnvironmentSource,
    EnvironmentSourceRequest, EnvironmentStatusView, EnvironmentTelemetryRequest, EnvironmentView,
    PromoteRequest, PromotedFrom, ReleasePlan, ReleasePromotion, RestoreRequest, RouteUrl,
    RunState, SecretProblem, TelemetryReport, TrackedBranch, UrlCondition, UrlState,
};
/// Request size and page size limits the daemon enforces.
pub use piqueld_core::api::{
    DEFAULT_PAGE_SIZE, MAX_PAGE_SIZE, MAX_REQUEST_BODY_BYTES, MAX_SECRET_VALUE_BYTES,
};
pub use piqueld_core::manifest::{
    ApplicationManifest, ApplicationSpec, ApplicationTemplate, Build, DeclaredSource,
    EnvironmentChange, EnvironmentConfig, Export, FileMode, FileSource, GitRepository, HealthCheck,
    Import, Job, JobRun, JobVolumeAccess, ManifestRepository, ManifestRevision, Metadata, Mount,
    Redirect, RedirectStatus, RepositoryManifest, ResourceLimits, Rollback, Rollout, RolloutOrder,
    RolloutOrderSource, Route, RouteTarget, SecretDeclaration, SecretEncoding, SecretGenerator,
    SecretMount, SecretSource, Service, ServiceFile, Source, SourceRepository, SystemVariable,
    Template, Typed, ValidatedBuild, ValidatedRollout, ValidatedSource, Variable, VariableValue,
    Visibility, Volume,
};
pub use piqueld_core::planner::{ActionReason, ActionRisk};
pub use piqueld_core::{
    ApplicationId, ApplicationName, BuildFingerprint, BuildInputs, EnvironmentId, Release,
    ReleaseAvailability, ReleaseId, ResolvedSource, RouteName, ServiceImage, ServiceName,
    TomlDiagnostic, ValidatedApplication, ValidationError, ValidationErrors,
};
pub use piqueld_core::{
    ApplicationState, Convergence, Event, EventActor, Operation, OperationKind, OperationState,
};
pub use previews::{
    ApplicationPreviews, BranchState, CountedPreview, CreatePreviewRequest, CreatedPreview,
    DeletedPreview, EnvironmentKind, LastDeployment, LocalSnapshot, Preview, PreviewLimit,
    PreviewLimitReached, PreviewLimits, PreviewSlot, PreviewSlug, PreviewUsage, PreviewView,
    PrunePreviewsRequest,
};
pub use system::{
    CertificateStatus, DnsProviderStatus, DnsStatus, ImageStatus, SnapshotLimits, SystemStatus,
    TailnetStatus, TelemetryDestinationStatus, TelemetryReceiver, TelemetryStatus,
};

pub use piqueld_core::api::{
    API_PREFIX, ApplicationLogs, BuildLogChunk, BuildLogPage, BuildRecord, BuildState, Envelope,
    EnvironmentAccess, ErrorBody, LogRecord, LogStream, ManifestChange, MountedSecret, Page,
    SecretAccess, SecretKeyRecovery, SecretMetadata, StoredSecret,
};

use http::StatusCode;
use thiserror::Error;

/// Validates a TOML application manifest and returns its editable name.
///
/// The daemon repeats this validation. The helper lets local CLI workflows
/// display the application name without importing the core crate directly.
///
/// # Errors
/// Returns field-level validation errors when the manifest is malformed or
/// outside the supported application schema.
pub fn application_name_from_toml(input: &str) -> Result<String, ValidationErrors> {
    piqueld_core::manifest::parse_template_toml(input)
        .map(|application| application.name().to_string())
}

/// The observed transport failure, without inferring daemon state.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TransportFailure {
    /// Opening the socket failed with this operating-system error kind.
    Connect(std::io::ErrorKind),
    /// The request exceeded its deadline.
    Timeout,
    /// Sending the request or receiving its response failed.
    Exchange,
}

/// Errors produced while making an API request.
#[derive(Debug, Error)]
pub enum ClientError {
    /// The endpoint URL or request could not be constructed.
    #[error("invalid request: {message}")]
    Endpoint {
        /// Detail describing which part of the construction was rejected.
        message: String,
    },
    /// The connection, protocol, or request timeout failed.
    #[error("API transport failed: {message}")]
    Transport {
        /// Safe transport failure detail suitable for operator diagnostics.
        message: String,
        /// Structured failure category retained for actionable diagnostics.
        kind: TransportFailure,
    },
    /// The server returned a non-success response.
    #[error("API returned {status}: {} ({})", error.code, error.message)]
    Api {
        /// HTTP response status.
        status: StatusCode,
        /// Structured server error.
        error: ErrorBody,
    },
    /// The server response could not be decoded.
    #[error("API returned an invalid response: {source}")]
    Decode {
        /// Decoder failure with line and column context.
        #[source]
        source: serde_json::Error,
    },
    /// A text response was not valid UTF-8.
    #[error("API returned invalid UTF-8: {source}")]
    TextDecode {
        /// UTF-8 decoder failure.
        #[source]
        source: std::string::FromUtf8Error,
    },
}

/// Returns the client crate version embedded at build time.
#[must_use]
pub const fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
