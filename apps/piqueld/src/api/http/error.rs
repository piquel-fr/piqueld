//! The API's structured error: every failure a handler or middleware
//! reports becomes an [`ApiError`], rendered as a JSON `ErrorBody` with a
//! stable `code`.
use super::JSON;
use crate::api::ApplicationError;
use crate::application::BoundaryError;
use crate::store::{PromotionError, StoreError};
use axum::{
    http::{StatusCode, header},
    response::{IntoResponse, Response},
};
use piqueld_core::api::ErrorBody;
use piqueld_core::{
    ApplicationIdError, EnvironmentIdError, EnvironmentNameError, GitBranchError, PreviewSlotError,
};
use serde_json::{Value, json};

/// Handler failure rendered as a structured JSON `ErrorBody` response.
///
/// Every domain error converts into this type, so handlers can use `?` and
/// still produce a stable machine-readable `code` plus a sanitized message.
#[derive(Debug)]
pub(super) struct ApiError {
    pub(super) status: StatusCode,
    /// Stable machine-readable error code, e.g. `generation_conflict`.
    code: &'static str,
    /// Sanitized, user-facing explanation.
    message: String,
    /// Extra structured context; `null` when there is none.
    details: Value,
    /// `Allow` header value, only set for 405 responses.
    pub(super) allow: Option<String>,
    /// Diagnostic attached as a response extension so `bind_error_request_id`
    /// can record it instead of synthesizing a generic one.
    pub(super) diagnostic: Option<Box<piqueld_core::observability::Diagnostic>>,
    /// Authorization refusal attached as a response extension, so the audit
    /// trail records it even when it reads as a plain 404.
    pub(super) denied: Option<piqueld_core::access::Denied>,
}

impl ApiError {
    /// Creates an error without details, `Allow` header, or diagnostic.
    pub(super) fn new(status: StatusCode, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
            details: Value::Null,
            allow: None,
            diagnostic: None,
            denied: None,
        }
    }
    /// The 404 for an API path no route matches.
    pub(super) fn endpoint_not_found() -> Self {
        Self::new(
            StatusCode::NOT_FOUND,
            "endpoint_not_found",
            "API endpoint was not found",
        )
    }

    /// Attaches structured `details` to the error body.
    pub(super) fn details(mut self, details: Value) -> Self {
        self.details = details;
        self
    }

    /// Logs storage failures that indicate a server-side fault. Expected client
    /// errors (conflicts, not found, validation) are deliberately not logged.
    fn log_storage_error(error: &StoreError) {
        if matches!(
            error,
            StoreError::SecretSource(_)
                | StoreError::Database
                | StoreError::DatabaseSource(_)
                | StoreError::SchemaMismatch
                | StoreError::SchemaMismatchSource(_)
                | StoreError::PathSource(_)
                | StoreError::Corrupt
                | StoreError::CorruptSource(_)
        ) {
            tracing::error!(?error, "storage request failed");
        }
    }
}

impl ApiError {
    /// Maps secret lifecycle failures selected by `From<StoreError>`.
    ///
    /// Panics if given a non-secret variant; callers must pre-filter.
    fn from_secret_error(error: StoreError) -> Self {
        match error {
            StoreError::SecretVersionConflict { expected, actual } => Self::new(
                StatusCode::CONFLICT,
                "secret_generation_conflict",
                "Secret changed since inspection; read its metadata and retry",
            )
            .details(json!({"expected_generation": expected, "actual_generation": actual})),
            StoreError::SecretUnavailable { names } => Self::new(
                StatusCode::CONFLICT,
                "secret_unavailable",
                "Supply replacement secret values and start a new deployment",
            )
            .details(json!({"names": names})),
            ref error @ StoreError::SecretMissing { ref names } => {
                Self::new(StatusCode::CONFLICT, "secret_missing", error.to_string())
                    .details(json!({"names": names}))
            }
            StoreError::SecretKeyUsable => Self::new(
                StatusCode::CONFLICT,
                "secret_key_usable",
                "The secret master key still works; recovery would discard values needlessly",
            ),
            error @ StoreError::SecretSource(_) => {
                let diagnostic = crate::operations::OperationError::from(error).diagnostic();
                let mut error = Self::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "secret_storage_unavailable",
                    "Secret storage is unavailable",
                );
                error.diagnostic = Some(Box::new(diagnostic));
                error
            }
            StoreError::SecretDeleting => Self::new(
                StatusCode::CONFLICT,
                "secret_deleting",
                "Secret deletion is in progress; retry deletion to finish cleanup",
            ),
            StoreError::SecretQuota => Self::new(
                StatusCode::CONFLICT,
                "secret_quota_exceeded",
                "Secret storage quota exceeded (100 secrets, 1000 versions or 100 MiB per environment or application store); delete unused secrets to free space",
            ),
            StoreError::SecretReferenced => Self::new(
                StatusCode::CONFLICT,
                "secret_referenced",
                "Secret is still referenced by application configuration or a deployment",
            ),
            ref error @ StoreError::SecretAccessDenied {
                ref environment,
                ref secret,
            } => Self::new(
                StatusCode::CONFLICT,
                "secret_access_denied",
                error.to_string(),
            )
            .details(json!({"environment": environment, "secret": secret})),
            other => unreachable!("non-secret storage error: {other}"),
        }
    }

    /// Maps environment selection and rename failures selected by
    /// `From<StoreError>`, naming the environments in `details`, previews
    /// of applications without a manifest repository, and previews over a
    /// `[previews]` limit, listing the previews it counts in `details`.
    ///
    /// Panics if given another variant; callers must pre-filter.
    fn from_environment_error(error: StoreError) -> Self {
        match error {
            StoreError::EnvironmentRequired { environments } => Self::new(
                StatusCode::CONFLICT,
                "environment_required",
                "The application does not have exactly one environment; name the environment explicitly",
            )
            .details(json!({"environments": environments})),
            StoreError::ConfirmationRequired { environments } => Self::new(
                StatusCode::CONFLICT,
                "environment_confirmation_required",
                "Deleting this application also deletes all its environments; confirm by naming every one",
            )
            .details(json!({"environments": environments})),
            ref error @ StoreError::EnvironmentConfigured { ref environment } => Self::new(
                StatusCode::CONFLICT,
                "environment_configured",
                error.to_string(),
            )
            .details(json!({"environment": environment})),
            ref error @ StoreError::EnvironmentDeclared { ref environment } => Self::new(
                StatusCode::CONFLICT,
                piqueld_core::codes::ENVIRONMENT_DECLARED,
                error.to_string(),
            )
            .details(json!({"environment": environment})),
            ref error @ StoreError::EnvironmentUnknown { ref environment } => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                piqueld_core::codes::ENVIRONMENT_UNKNOWN,
                error.to_string(),
            )
            .details(json!({"environment": environment})),
            ref error @ StoreError::PreviewRequiresRepository => Self::new(
                StatusCode::CONFLICT,
                piqueld_core::codes::PREVIEW_REQUIRES_REPOSITORY,
                error.to_string(),
            ),
            ref error @ StoreError::PreviewLimitReached(ref reached) => Self::new(
                StatusCode::CONFLICT,
                piqueld_core::codes::PREVIEW_LIMIT_REACHED,
                error.to_string(),
            )
            .details(json!(reached)),
            other => unreachable!("non-environment storage error: {other}"),
        }
    }

    /// Maps refused promotions and promotion sources, with what they name in
    /// `details`. Changes the source made since it was inspected are
    /// conflicts; impossible sources are invalid input.
    fn from_promotion_error(error: &PromotionError) -> Self {
        let (status, code, details) = match error {
            PromotionError::Promoted { environment } => (
                StatusCode::CONFLICT,
                "environment_promoted",
                json!({"environment": environment}),
            ),
            PromotionError::NotPromoted { environment } => (
                StatusCode::CONFLICT,
                "environment_not_promoted",
                json!({"environment": environment}),
            ),
            PromotionError::Cycle { environments } => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "promotion_cycle",
                json!({"environments": environments}),
            ),
            PromotionError::SourceInvalid { environment, .. } => (
                StatusCode::UNPROCESSABLE_ENTITY,
                "promotion_source_invalid",
                json!({"environment": environment}),
            ),
            PromotionError::InUse { environments } => (
                StatusCode::CONFLICT,
                "promotion_source_in_use",
                json!({"environments": environments}),
            ),
            PromotionError::SourceNotReady { environment, .. } => (
                StatusCode::CONFLICT,
                "promotion_source_not_ready",
                json!({"environment": environment}),
            ),
            PromotionError::SourceChanged {
                environment,
                deployment,
            } => (
                StatusCode::CONFLICT,
                "promotion_source_changed",
                json!({"environment": environment, "deployment": deployment}),
            ),
            PromotionError::Secrets {
                environment,
                secrets,
            } => (
                StatusCode::CONFLICT,
                "secrets_unavailable",
                json!({"environment": environment, "secrets": secrets}),
            ),
        };
        Self::new(status, code, error.to_string()).details(details)
    }

    /// Maps hostname reservation conflicts selected by `From<StoreError>`,
    /// naming the hostname, and the sibling environment reserving it if any.
    ///
    /// Panics if given another variant; callers must pre-filter.
    fn from_hostname_conflict(error: StoreError) -> Self {
        match error {
            StoreError::HostnameConflict { hostname } => Self::new(
                StatusCode::CONFLICT,
                "hostname_conflict",
                "Hostname is reserved by another environment, another application's wildcard, or this installation",
            )
            .details(json!({"hostname": hostname})),
            ref error @ StoreError::SharedHostnameConflict {
                ref hostname,
                ref environment,
            } => Self::new(StatusCode::CONFLICT, "hostname_conflict", error.to_string())
                .details(json!({"hostname": hostname, "environment": environment})),
            other => unreachable!("non-hostname storage error: {other}"),
        }
    }
}

impl From<StoreError> for ApiError {
    /// Classifies storage failures into HTTP status codes and stable error
    /// codes, logging server-side faults first.
    fn from(value: StoreError) -> Self {
        Self::log_storage_error(&value);
        match value {
            StoreError::Validation(errors) => errors.into(),
            StoreError::Edit(error) => error.into(),
            error @ (StoreError::SecretVersionConflict { .. }
            | StoreError::SecretUnavailable { .. }
            | StoreError::SecretMissing { .. }
            | StoreError::SecretKeyUsable
            | StoreError::SecretSource(_)
            | StoreError::SecretDeleting
            | StoreError::SecretQuota
            | StoreError::SecretReferenced
            | StoreError::SecretAccessDenied { .. }) => Self::from_secret_error(error),
            error @ (StoreError::HostnameConflict { .. }
            | StoreError::SharedHostnameConflict { .. }) => Self::from_hostname_conflict(error),
            error @ (StoreError::EnvironmentRequired { .. }
            | StoreError::ConfirmationRequired { .. }
            | StoreError::EnvironmentConfigured { .. }
            | StoreError::EnvironmentDeclared { .. }
            | StoreError::EnvironmentUnknown { .. }
            | StoreError::PreviewRequiresRepository
            | StoreError::PreviewLimitReached(_)) => Self::from_environment_error(error),
            StoreError::Promotion(error) => Self::from_promotion_error(&error),
            StoreError::GenerationConflict { expected, actual } => Self::new(
                StatusCode::CONFLICT,
                "generation_conflict",
                "Application changed since inspection; run the command again",
            )
            .details(json!({"expected_generation": expected, "actual_generation": actual})),
            StoreError::IdentityConflict => Self::new(
                StatusCode::CONFLICT,
                "identity_conflict",
                "Application changed since inspection; run the command again",
            ),
            StoreError::ReplayConflict => Self::new(
                StatusCode::CONFLICT,
                "request_id_conflict",
                "request ID was already used for different input",
            ),
            StoreError::RepositoryManaged => Self::new(
                StatusCode::CONFLICT,
                "repository_managed",
                "Edit runtime configuration in the repository manifest; only its connection settings can be changed directly",
            ),
            StoreError::Busy => Self::new(
                StatusCode::CONFLICT,
                "application_busy",
                "application is busy; wait for its current operation to finish",
            ),
            StoreError::HistoryExpired => Self::new(
                StatusCode::GONE,
                "history_expired",
                "Requested event history was pruned; reload history before resuming",
            ),
            StoreError::NotFound => {
                Self::new(StatusCode::NOT_FOUND, "not_found", "resource was not found")
            }
            StoreError::Lockout(lockout) => {
                Self::new(StatusCode::CONFLICT, "account_lockout", lockout.message())
            }
            StoreError::Destination(error) => error.into(),
            StoreError::Retry(error) => error.into(),
            StoreError::Denied(denied) => denied.into(),
            StoreError::CredentialRevoked => crate::auth::AuthError::Unauthorized.into(),
            StoreError::AlreadyExists => Self::new(
                StatusCode::CONFLICT,
                "application_name_collision",
                "application, environment, or preview identity or name already exists",
            ),
            StoreError::IllegalTransition => Self::new(
                StatusCode::CONFLICT,
                "application_state_conflict",
                "the requested application transition is not allowed",
            ),
            StoreError::InvalidInput | StoreError::InvalidInputSource(_) => Self::new(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "the request is invalid",
            ),
            StoreError::SchemaMismatch | StoreError::SchemaMismatchSource(_) => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "schema_mismatch",
                "database schema is incompatible",
            ),
            StoreError::Corrupt | StoreError::CorruptSource(_) => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "stored_state_corrupt",
                "stored application state is corrupt",
            ),
            StoreError::Database | StoreError::DatabaseSource(_) | StoreError::PathSource(_) => {
                Self::new(
                    StatusCode::SERVICE_UNAVAILABLE,
                    "storage_unavailable",
                    "control-plane storage is unavailable",
                )
            }
        }
    }
}

impl From<crate::store::DestinationError> for ApiError {
    /// Rejects an invalid destination change with 400.
    fn from(error: crate::store::DestinationError) -> Self {
        use crate::store::DestinationError;
        let code = match error {
            DestinationError::UrlRequired => "destination_url_required",
            DestinationError::UrlInvalid => "destination_url_invalid",
            DestinationError::UnknownApplication(_) => "destination_application_unknown",
        };
        Self::new(StatusCode::BAD_REQUEST, code, error.to_string())
    }
}

impl From<crate::store::RetryError> for ApiError {
    /// Refuses a delivery that cannot be retried with 409, saying why.
    fn from(error: crate::store::RetryError) -> Self {
        Self::new(
            StatusCode::CONFLICT,
            "delivery_not_retryable",
            error.to_string(),
        )
    }
}

impl From<piqueld_core::edit::EditError> for ApiError {
    /// Maps field-edit failures, exposing the edit reason in `details.reason`.
    fn from(error: piqueld_core::edit::EditError) -> Self {
        use piqueld_core::edit::EditError;
        let (status, code) = match &error {
            EditError::NotFound { .. } => (StatusCode::NOT_FOUND, "field_resource_not_found"),
            EditError::AlreadyExists { .. } => (StatusCode::CONFLICT, "field_resource_exists"),
            EditError::Incompatible(_) => (StatusCode::CONFLICT, "field_incompatible"),
        };
        Self::new(status, code, "The field edit could not be applied")
            .details(json!({"reason": error.to_string()}))
    }
}

impl From<BoundaryError> for ApiError {
    /// Maps runtime, build, and compilation failures, always attaching the
    /// boundary diagnostic so it is recorded with the request.
    fn from(value: BoundaryError) -> Self {
        let diagnostic = value.diagnostic();
        // Storage conversion handles its own logging, including expected client errors.
        if !matches!(&value, BoundaryError::Store(_)) {
            tracing::error!(error = ?value, "runtime boundary request failed");
        }
        let mut error = match value {
            BoundaryError::Store(error) => error.into(),
            BoundaryError::Runtime(
                crate::docker::DockerError::Unavailable(_)
                | crate::docker::DockerError::UnavailableSource { .. },
            ) => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "docker_unavailable",
                "Docker Engine is unavailable",
            ),
            BoundaryError::Runtime(_) => Self::new(
                StatusCode::BAD_GATEWAY,
                "runtime_request_failed",
                "runtime request failed",
            ),
            BoundaryError::GitBuild(_) => Self::new(
                StatusCode::BAD_GATEWAY,
                "git_build_failed",
                "Git source build failed",
            ),
            BoundaryError::Compilation(_) => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "application_compilation_failed",
                "application compilation failed",
            ),
            BoundaryError::ImageUnavailable(error) => {
                Self::new(StatusCode::CONFLICT, "image_unavailable", error.to_string())
            }
        };
        error.diagnostic = Some(Box::new(diagnostic));
        error
    }
}

/// Implements `From` for errors rejecting a malformed identifier or name in
/// a request, answering 400 with `$code` and the message `$message` builds
/// from the error.
macro_rules! invalid_input {
    ($($error:ty => $code:literal, |$value:pat_param| $message:expr;)+) => {$(
        impl From<$error> for ApiError {
            fn from(error: $error) -> Self {
                let $value = error;
                Self::new(StatusCode::BAD_REQUEST, $code, $message)
            }
        }
    )+};
}

invalid_input! {
    ApplicationIdError => "application_id_invalid", |_| "application ID is invalid";
    EnvironmentIdError => "environment_id_invalid", |_| "environment ID is invalid";
    EnvironmentNameError => "environment_name_invalid", |_| "environment names must be 1-63 lowercase letters, digits, or hyphens, start with a letter, and end with a letter or digit";
    GitBranchError => "git_branch_invalid", |error| error.to_string();
    PreviewSlotError => "preview_slot_invalid", |error| error.to_string();
    piqueld_core::observability::DestinationNameError => "destination_name_invalid", |error| error.to_string();
}

impl From<piqueld_core::ValidationErrors> for ApiError {
    /// Reports pure decode failures as `toml_malformed` (400) and semantic
    /// validation failures as `manifest_validation_failed` (422). Both list
    /// the individual errors in `details.errors`.
    fn from(piqueld_core::ValidationErrors(errors): piqueld_core::ValidationErrors) -> Self {
        let error = if errors
            .iter()
            .all(|error| error.code == piqueld_core::codes::MANIFEST_DECODE_FAILED)
        {
            Self::new(
                StatusCode::BAD_REQUEST,
                "toml_malformed",
                "request TOML is malformed or does not match the application schema",
            )
        } else {
            Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "manifest_validation_failed",
                "application manifest failed validation",
            )
        };
        error.details(json!({"errors": errors}))
    }
}

impl ApiError {
    /// Builds the public JSON error body. Its placeholder `request_id` is
    /// replaced with the real one by `bind_error_request_id`, or by the exec
    /// stream before it writes a terminal failure frame.
    pub(super) fn body(&self) -> ErrorBody {
        ErrorBody {
            code: self.code.into(),
            message: self.message.clone(),
            details: self.details.clone(),
            request_id: uuid::Uuid::now_v7().simple().to_string(),
        }
    }
}

impl IntoResponse for ApiError {
    /// Renders the JSON error body with the error's status, diagnostic
    /// extension, and optional `Allow` header.
    fn into_response(self) -> Response {
        let mut response = (
            self.status,
            [(header::CONTENT_TYPE, JSON)],
            axum::Json(self.body()),
        )
            .into_response();
        if let Some(diagnostic) = self.diagnostic {
            response.extensions_mut().insert(*diagnostic);
        }
        if let Some(denied) = self.denied {
            response.extensions_mut().insert(denied);
        }
        if let Some(allow) = self.allow
            && let Ok(value) = header::HeaderValue::from_str(&allow)
        {
            response.headers_mut().insert(header::ALLOW, value);
        }
        response
    }
}

impl From<ApplicationError> for ApiError {
    /// Maps application service failures, delegating storage and runtime
    /// errors to their dedicated conversions.
    fn from(error: ApplicationError) -> Self {
        match error {
            ApplicationError::PreconditionRequired => Self::new(
                StatusCode::BAD_REQUEST,
                "precondition_required",
                "Supply the inspected revision and application identity, or explicitly set force=true",
            ),
            ApplicationError::InvalidPagination => Self::new(
                StatusCode::BAD_REQUEST,
                "pagination_invalid",
                "pagination parameters are invalid",
            ),
            ApplicationError::InvalidLogQuery => Self::new(
                StatusCode::BAD_REQUEST,
                "logs_query_invalid",
                "Tail must be 1–1000 and time window 1–86400 seconds",
            ),
            ApplicationError::ServiceNotRunning => Self::new(
                StatusCode::CONFLICT,
                "service_not_running",
                "Service has no running task; deploy it or check its health",
            ),
            ApplicationError::ConfigurationUnavailable => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "configuration_unavailable",
                "Effective host configuration is unavailable",
            ),
            ApplicationError::ManifestSerialization(error) => {
                tracing::error!(?error, "serialize saved manifest");
                Self::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "manifest_serialization_failed",
                    "Could not render saved configuration",
                )
            }
            ApplicationError::Store(error) => error.into(),
            ApplicationError::Runtime(error) => error.into(),
            ApplicationError::PlanBlocked(diagnostics) => Self::new(
                StatusCode::CONFLICT,
                "plan_blocked",
                "runtime plan contains blocking conflicts",
            )
            .details(json!({"diagnostics":diagnostics})),
            ApplicationError::RepositoryUnavailable(error) => {
                tracing::warn!(?error, "manifest repository unavailable");
                Self::new(
                    StatusCode::BAD_GATEWAY,
                    "repository_unavailable",
                    format!(
                        "The manifest repository could not be read ({error:#}); no preview was deleted"
                    ),
                )
            }
            ApplicationError::Snapshot(error) => error.into(),
            ApplicationError::Busy(_) => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "server_busy",
                "Too many requests like this one are running; try again shortly",
            ),
        }
    }
}

impl From<crate::git::snapshot::SnapshotError> for ApiError {
    /// Maps refused snapshot uploads; Git's output stays in the logs.
    fn from(error: crate::git::snapshot::SnapshotError) -> Self {
        use crate::git::snapshot::SnapshotError;
        let message = error.to_string();
        match error {
            SnapshotError::TooLarge { limit } => Self::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                piqueld_core::codes::SNAPSHOT_TOO_LARGE,
                format!("{message}; raise [snapshots] max_upload_bytes, or upload against a base the repository has"),
            )
            .details(json!({ "limit": limit })),
            SnapshotError::TreeTooLarge { limit } => Self::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                piqueld_core::codes::SNAPSHOT_TOO_LARGE,
                format!("{message}; raise [checkouts] max_bytes, or ignore the files the preview doesn't need"),
            )
            .details(json!({ "limit": limit })),
            SnapshotError::Invalid(_) => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                piqueld_core::codes::SNAPSHOT_INVALID,
                message,
            ),
            SnapshotError::BaseUnavailable { prerequisites } => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                piqueld_core::codes::SNAPSHOT_BASE_UNAVAILABLE,
                format!("{message}; push the base, or upload a complete bundle"),
            )
            .details(json!({ "prerequisites": prerequisites })),
            SnapshotError::TimedOut => Self::new(
                StatusCode::REQUEST_TIMEOUT,
                piqueld_core::codes::SNAPSHOT_TIMED_OUT,
                message,
            ),
            SnapshotError::Internal(error) => {
                tracing::error!(?error, "snapshot upload failed");
                Self::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "snapshot_unavailable",
                    message,
                )
            }
        }
    }
}
