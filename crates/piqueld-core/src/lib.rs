//! Pure, deterministic contracts shared by every piqueld interface.
//!
//! This crate deliberately has no transport, persistence, container-runtime, or
//! user-interface dependencies.

pub mod api;
pub mod codes;
pub mod environment;
pub use environment::{EnvironmentKind, EnvironmentSource, Preview, TrackedBranch};
pub mod event;
pub mod exec;
pub mod identity;
pub use event::Event;
pub mod manifest;
pub mod names;
pub use names::{
    ApplicationName, ApplicationNameError, EnvironmentName, EnvironmentNameError, GitBranch,
    GitBranchError, JobName, JobNameError, PreviewSlot, PreviewSlotError, PreviewSlug,
    PreviewSlugError, ServiceName, ServiceNameError, VolumeName, VolumeNameError,
};
pub mod operation;
pub mod planner;
mod preview;
pub mod release;
pub use release::{BuildFingerprint, BuildInputs, Release};
pub mod resource;
pub mod retention;
pub use retention::{LocalImages, ReleaseAvailability, RetentionRoot, ServiceImage};
pub mod sync;

pub use identity::{
    ApplicationId, ApplicationIdError, EnvironmentId, EnvironmentIdError, ReleaseId,
    ReleaseIdError, ResourceKind, docker_resource_name, docker_resource_readable_prefix,
};
pub use manifest::{
    APPLICATION_API_VERSION, APPLICATION_KIND, ApplicationSpec, HealthCheck, Metadata, Mount,
    NormalizedApplication, ResourceLimits, Service, Source, ValidatedApplication, ValidationError,
    ValidationErrors, Volume, parse_json, parse_toml,
};
pub use planner::{
    ActionKind, ActionReason, ActionRisk, DiagnosticSeverity, Plan, PlanAction, PlanDiagnostic,
    PlanRequest, PlanSummary,
};
pub use resource::{
    CompileError, Convergence, DesiredJob, DesiredJobRun, DesiredMount, DesiredNetwork,
    DesiredService, DesiredVolume, InstanceId, InstanceIdError, ObservedApplication,
    ObservedNetwork, ObservedService, ObservedTask, ObservedVolume, Ownership, OwnershipState,
    ResolutionRequirement, ResolutionSet, ResolvedApplication, ResolvedSource, Sha256Digest,
    Sha256DigestError, TaskDiagnostic, TaskState, compile_application, compile_release,
    image_repository, preview_resolution, valid_logical_name,
};

pub use operation::{ApplicationState, Operation, OperationKind, OperationState};

/// Typed generated Docker resource names.
pub mod docker_names;
pub use docker_names::{DockerNetworkName, DockerServiceName, DockerVolumeName};

/// Checked image references used by runtime compilation.
pub mod images;
pub use images::{ImageReference, ImmutableImage, RepositoryDigest};

/// Typed saved-configuration editing contracts.
pub mod edit;

/// Permissions, grants, and authorization decisions.
pub mod access;
/// Audit trail contracts.
pub mod audit;
/// Passkey authentication and account management contracts.
pub mod auth;
/// Tailnet identities and the token bindings they satisfy.
pub mod tailnet;
/// Credential-safe TOML parse diagnostics.
pub mod toml_diagnostic;
pub use toml_diagnostic::{TomlDiagnostic, TomlLocation};
/// Durable observability API contracts.
pub mod observability;
