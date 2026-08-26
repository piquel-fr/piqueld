//! Typed asynchronous client and transport contracts for the versioned piqueld API.
//!
//! Each contract module holds its request and response types next to the
//! [`Client`] endpoint methods that serve them. The client runs one
//! shared request pipeline on every target, with platform differences
//! confined inside `client`: loopback TCP and Unix-domain sockets natively,
//! and same-origin browser fetch under WASM.

#[cfg(target_os = "windows")]
compile_error!("Windows is not supported by piqueld-client");

/// Application CRUD, planning, and observation contracts.
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
    CreateApplicationRequest, DeleteApplicationRequest, DiagnosticView, ExpectedGeneration,
    ListApplicationsOptions, ObservedApplicationView, ObservedServiceView, PlanApplicationRequest,
    PlanView, ReplaceApplicationRequest, ReplacePlanRequest,
};
pub use client::Client;
pub use operations::{OperationStepView, OperationView};
pub use piqueld_core::manifest::{
    ApplicationManifest, ApplicationSpecInput, HealthCheckInput, MetadataInput, MountInput,
    ResourceLimitsInput, ServiceInput, Source, SourceInput, VolumeInput,
};
pub use piqueld_core::planner::{ActionReason, ActionRisk};
pub use piqueld_core::{ApplicationId, ValidatedApplication, ValidationError, ValidationErrors};
pub use system::SystemStatus;

use http::StatusCode;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use utoipa::ToSchema;

/// Versioned prefix used by all API endpoints.
pub const API_PREFIX: &str = "/api/v1";

/// Validates a TOML application manifest and returns its editable name.
///
/// The daemon repeats this validation. The helper lets local CLI workflows
/// resolve a replacement target without importing the core crate directly.
///
/// # Errors
/// Returns field-level validation errors when the manifest is malformed or
/// outside the supported application schema.
pub fn application_name_from_toml(input: &str) -> Result<String, ValidationErrors> {
    piqueld_core::parse_toml(input).map(|application| application.name().to_owned())
}

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
