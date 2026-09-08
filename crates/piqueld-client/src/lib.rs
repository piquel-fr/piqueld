//! Typed asynchronous client for the versioned piqueld API.
//!
//! Shared API contracts are reexported from `piqueld-core`. The client runs one
//! shared request pipeline on every target, with platform differences
//! confined inside `client`: loopback TCP and Unix-domain sockets natively,
//! and same-origin browser fetch under WASM.

#[cfg(target_os = "windows")]
compile_error!("Windows is not supported by piqueld-client");

/// Application desired-state, planning, and observation contracts.
pub mod applications;
/// Generated OpenAPI document retrieval.
pub mod openapi;
/// Operation inspection contracts.
pub mod operations;
/// Control-plane status contracts.
pub mod system;

mod client;

pub use applications::{
    AcceptedOperation, ApplicationDetailView, ApplicationStatusView, ApplicationView,
    ApplyApplicationRequest, DiagnosticView, ListApplicationsOptions, ObservedApplicationView,
    ObservedServiceView, PlanView,
};
pub use client::Client;
pub use piqueld_core::manifest::{
    ApplicationManifest, ApplicationSpec, HealthCheck, Metadata, Mount, ResourceLimits, Service,
    Source, Volume,
};
pub use piqueld_core::planner::{ActionReason, ActionRisk};
pub use piqueld_core::{ApplicationId, ValidatedApplication, ValidationError, ValidationErrors};
pub use piqueld_core::{ApplicationState, Convergence, Operation, OperationKind, OperationState};
pub use system::SystemStatus;

use http::StatusCode;
use thiserror::Error;

pub use piqueld_core::api::{API_PREFIX, Envelope, ErrorBody, Page};

/// Validates a TOML application manifest and returns its editable name.
///
/// The daemon repeats this validation. The helper lets local CLI workflows
/// display the application name without importing the core crate directly.
///
/// # Errors
/// Returns field-level validation errors when the manifest is malformed or
/// outside the supported application schema.
pub fn application_name_from_toml(input: &str) -> Result<String, ValidationErrors> {
    piqueld_core::parse_toml(input).map(|application| application.name().to_owned())
}

#[derive(Debug, Error)]
/// Errors produced while making an API request.
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
}

/// Returns the client crate version embedded at build time.
#[must_use]
pub const fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
