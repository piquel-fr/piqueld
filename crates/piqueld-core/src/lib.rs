//! Pure, deterministic contracts shared by every piqueld interface.
//!
//! This crate deliberately has no transport, persistence, container-runtime, or
//! user-interface dependencies.

pub mod identity;
pub mod manifest;
pub mod planner;
pub mod resource;

pub use identity::{ApplicationId, ApplicationIdError, ResourceKind, docker_resource_name};
pub use manifest::{
    APPLICATION_API_VERSION, APPLICATION_KIND, NormalizedApplication, ValidatedApplication,
    ValidationError, ValidationErrors, parse_json, parse_toml,
};
pub use planner::{Plan, PlanAction, PlanDiagnostic, PlanRequest};
pub use resource::{
    CompileError, DesiredApplication, InstanceId, InstanceIdError, ObservedApplication,
    ResolutionSet, Sha256Digest, Sha256DigestError, compile_application, preview_resolution,
};
