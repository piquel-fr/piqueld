//! Pure, deterministic contracts shared by every piqueld interface.
//!
//! This crate deliberately has no transport, persistence, container-runtime, or
//! user-interface dependencies.

pub mod codes;
pub mod identity;
pub mod manifest;
pub mod planner;
pub mod resource;

pub use identity::{
    ApplicationId, ApplicationIdError, ResourceKind, docker_resource_name,
    docker_resource_readable_prefix,
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
    CompileError, Convergence, DesiredApplication, DesiredMount, DesiredNetwork, DesiredService,
    DesiredVolume, InstanceId, InstanceIdError, ObservedApplication, ObservedNetwork,
    ObservedService, ObservedTask, ObservedVolume, Ownership, OwnershipState,
    ResolutionRequirement, ResolutionSet, ResolvedApplication, ResolvedSource, Sha256Digest,
    Sha256DigestError, TaskDiagnostic, TaskState, compile_application, image_repository,
    preview_resolution, valid_logical_name,
};
