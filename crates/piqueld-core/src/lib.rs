//! Pure, deterministic contracts shared by every piqueld interface.
//!
//! This crate deliberately has no transport, persistence, container-runtime, or
//! user-interface dependencies.

pub mod access;
pub mod api;
pub mod audit;
pub mod auth;
mod changes;
pub mod codes;
pub mod docker_names;
pub mod edit;
pub mod environment;
pub mod event;
pub mod exec;
pub mod generated;
pub mod identity;
pub mod images;
pub mod json_schema;
pub mod links;
pub mod manifest;
pub mod names;
pub mod observability;
pub mod operation;
pub mod planner;
pub mod release;
pub mod resource;
pub mod retention;
pub mod sync;
pub mod tailnet;
pub mod toml_diagnostic;
pub mod urls;

pub use docker_names::{DockerConfigName, DockerNetworkName, DockerServiceName, DockerVolumeName};
pub use environment::{EnvironmentKind, EnvironmentSource, Preview, PromotedFrom, TrackedBranch};
pub use event::{Event, EventActor};
pub use identity::{
    ApplicationId, ApplicationIdError, EnvironmentId, EnvironmentIdError, ReleaseId,
    ReleaseIdError, ResourceKind, docker_resource_name, docker_resource_readable_prefix,
};
pub use images::{ImageReference, ImmutableImage, RepositoryDigest};
pub use manifest::{
    APPLICATION_API_VERSION, APPLICATION_KIND, ApplicationSpec, HealthCheck, Metadata, Mount,
    NormalizedApplication, ResourceLimits, Service, Source, ValidatedApplication, ValidationError,
    ValidationErrors, Volume, parse_json, parse_toml,
};
pub use names::{
    ApplicationName, ApplicationNameError, EnvironmentName, EnvironmentNameError, GitBranch,
    GitBranchError, JobName, JobNameError, PreviewSlot, PreviewSlotError, PreviewSlug,
    PreviewSlugError, RouteName, RouteNameError, ServiceName, ServiceNameError, VolumeName,
    VolumeNameError,
};
pub use operation::{ApplicationState, Operation, OperationKind, OperationState};
pub use planner::{
    ActionKind, ActionReason, ActionRisk, DiagnosticSeverity, Plan, PlanAction, PlanDiagnostic,
    PlanRequest, PlanSummary,
};
pub use release::{BuildFingerprint, BuildInputs, Release};
pub use resource::{
    CompileError, Convergence, DesiredFile, DesiredJob, DesiredJobRun, DesiredMount,
    DesiredNetwork, DesiredService, DesiredVolume, InstanceId, InstanceIdError,
    ObservedApplication, ObservedNetwork, ObservedService, ObservedTask, ObservedVolume, Ownership,
    OwnershipState, ResolutionRequirement, ResolutionSet, ResolvedApplication, ResolvedSource,
    Sha256Digest, Sha256DigestError, TaskDiagnostic, TaskState, compile_application,
    compile_release, image_repository, preview_resolution, valid_logical_name,
};
pub use retention::{LocalImages, ReleaseAvailability, RetentionRoot, ServiceImage};
pub use toml_diagnostic::{TomlDiagnostic, TomlLocation};
