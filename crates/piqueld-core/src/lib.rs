//! Pure, deterministic contracts shared by every piqueld interface.
//!
//! This crate deliberately has no transport, persistence, container-runtime, or
//! user-interface dependencies.

pub mod error;
pub mod identity;
pub mod manifest;

pub use error::{ErrorCode, PublicError};
pub use identity::{ApplicationId, ResourceKind, docker_resource_name, router_name};
pub use manifest::{
    APPLICATION_API_VERSION, APPLICATION_KIND, NormalizedApplication, ValidatedApplication,
    ValidationError, ValidationErrors, parse_json, parse_toml,
};
