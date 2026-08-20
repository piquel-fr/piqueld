//! Application and control-plane transfer HTTP handlers.

use axum::{
    body::Bytes,
    extract::{
        Path, Query, State,
        rejection::{BytesRejection, QueryRejection},
    },
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use chacha20poly1305::{
    XChaCha20Poly1305,
    aead::{AeadCore, OsRng},
};
use piqueld_client::{
    Envelope, PrepareStateImportRequest, StateExportMode, StateImportConfirmation,
};
use piqueld_core::ApplicationId;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fmt::Write as _;

use super::{ApiError, ApiState, content_type, header_text, ok, openapi::ApiErrorResponse};
use crate::transfer::{ARCHIVE_CONTENT_TYPE, MAX_ARCHIVE_BYTES};

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(super) struct StateExportQuery {
    /// Archive mode; portable is the default and excludes encrypted envelopes.
    mode: Option<StateExportMode>,
}

#[utoipa::path(
    get,
    path = "/api/v1/state/export",
    operation_id = "exportState",
    summary = "Export a deterministic checksummed state archive",
    params(StateExportQuery),
    responses(
        (status = 200, description = "Binary state archive", content_type = "application/vnd.piqueld.state-v1+tar"),
        (status = 500, response = inline(ApiErrorResponse)),
        (status = 503, response = inline(ApiErrorResponse)),
    )
)]
pub(super) async fn export_state(
    State(state): State<ApiState>,
    query: Result<Query<StateExportQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let Query(query) = query.map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "archive_query_invalid",
            "state export parameters are invalid",
        )
    })?;
    let archive = state
        .transfer
        .export(query.mode.unwrap_or_default())
        .await?;
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, ARCHIVE_CONTENT_TYPE),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=piqueld-state-v1.tar",
            ),
        ],
        archive,
    )
        .into_response())
}

#[utoipa::path(
    post,
    path = "/api/v1/state/import/confirm",
    operation_id = "prepareStateImport",
    summary = "Create a short-lived confirmation bound to archive bytes",
    request_body = PrepareStateImportRequest,
    responses(
        (status = 200, description = "Confirmation", body = Envelope<StateImportConfirmation>),
        (status = 400, response = inline(ApiErrorResponse)),
        (status = 503, response = inline(ApiErrorResponse)),
    )
)]
pub(super) async fn prepare_state_import(
    State(state): State<ApiState>,
    axum::Json(request): axum::Json<PrepareStateImportRequest>,
) -> Result<impl IntoResponse, ApiError> {
    if !valid_digest(&request.archive_digest) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "archive_digest_invalid",
            "archive digest must be a SHA-256 digest",
        ));
    }
    let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    let token = format!("replace-{}", hex(&Sha256::digest(nonce.as_slice())));
    let expires_at_ms = current_time_ms().saturating_add(5 * 60 * 1_000);
    let mut confirmations = state.import_confirmations.lock().await;
    confirmations.retain(|_, (_, expiry)| *expiry > current_time_ms());
    if confirmations.len() >= 1_024 {
        return Err(ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "confirmation_capacity",
            "state import confirmation capacity is temporarily exhausted",
        ));
    }
    confirmations.insert(
        token.clone(),
        (request.archive_digest.clone(), expires_at_ms),
    );
    Ok(ok(StateImportConfirmation {
        token,
        archive_digest: request.archive_digest,
        expires_at_ms,
    }))
}

#[utoipa::path(
    post,
    path = "/api/v1/state/import",
    operation_id = "importState",
    summary = "Validate and transactionally replace control-plane state",
    params(("X-Replace-Confirmation" = String, Header, min_length = 72, max_length = 72)),
    request_body(content = String, content_type = "application/vnd.piqueld.state-v1+tar"),
    responses(
        (status = 200, description = "Import result", body = Envelope<piqueld_client::StateImportResult>),
        (status = 400, response = inline(ApiErrorResponse)),
        (status = 412, response = inline(ApiErrorResponse)),
        (status = 413, response = inline(ApiErrorResponse)),
        (status = 415, response = inline(ApiErrorResponse)),
        (status = 422, response = inline(ApiErrorResponse)),
        (status = 503, response = inline(ApiErrorResponse)),
    )
)]
pub(super) async fn import_state(
    State(state): State<ApiState>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<impl IntoResponse, ApiError> {
    if content_type(&headers) != Some(ARCHIVE_CONTENT_TYPE) {
        return Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "content_type_unsupported",
            "Content-Type must identify the piqueld state archive v1 format",
        ));
    }
    let body = body.map_err(|rejection| {
        if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
            ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "archive_limit",
                "state archive exceeds a safety limit",
            )
        } else {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "archive_malformed",
                "state archive could not be read",
            )
        }
    })?;
    if body.is_empty() || body.len() > MAX_ARCHIVE_BYTES {
        return Err(crate::transfer::TransferError::Limit.into());
    }
    let _import = state
        .import_lock
        .clone()
        .try_lock_owned()
        .map_err(|_| crate::transfer::TransferError::Maintenance)?;
    let token = header_text(&headers, "x-replace-confirmation")
        .filter(|value| valid_token(value))
        .ok_or(crate::transfer::TransferError::Confirmation)?;
    let archive_digest = digest(&body);
    {
        let mut confirmations = state.import_confirmations.lock().await;
        let valid = confirmations.get(token).is_some_and(|(expected, expiry)| {
            expected == &archive_digest && *expiry > current_time_ms()
        });
        if !valid {
            return Err(crate::transfer::TransferError::Confirmation.into());
        }
        confirmations.remove(token);
    }
    let staged = state.transfer.stage_import(&body).await?;
    let result = state.transfer.replace(staged).await?;
    Ok(ok(result))
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(super) struct ApplicationExportQuery {
    /// Include resolved immutable metadata as a comment without secret values.
    #[serde(default)]
    include_resolved: bool,
}

#[utoipa::path(
    get,
    path = "/api/v1/applications/{id}/export",
    operation_id = "exportApplication",
    summary = "Export one canonical application manifest",
    params(("id" = String, Path, min_length = 8, max_length = 64), ApplicationExportQuery),
    responses(
        (status = 200, description = "Canonical TOML manifest", content_type = "application/toml"),
        (status = 400, response = inline(ApiErrorResponse)),
        (status = 404, response = inline(ApiErrorResponse)),
        (status = 500, response = inline(ApiErrorResponse)),
        (status = 503, response = inline(ApiErrorResponse)),
    )
)]
pub(super) async fn export_application(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    query: Result<Query<ApplicationExportQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let Query(query) = query.map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "application_export_query_invalid",
            "application export parameters are invalid",
        )
    })?;
    let id = ApplicationId::parse(&id)?;
    let application = state.store.get(&id).await?;
    let mut document = application.application.export_toml().map_err(|_| {
        ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "application_export_failed",
            "application export failed",
        )
    })?;
    if query.include_resolved {
        let resolved = serde_json::to_string(&application.resolved).map_err(|_| {
            ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "application_export_failed",
                "application export failed",
            )
        })?;
        document = format!("# piqueld-resolved-metadata-json: {resolved}\n{document}");
    }
    Ok((
        StatusCode::OK,
        [
            (header::CONTENT_TYPE, "application/toml"),
            (
                header::CONTENT_DISPOSITION,
                "attachment; filename=application.toml",
            ),
        ],
        document,
    )
        .into_response())
}

fn valid_digest(value: &str) -> bool {
    value.len() == 71
        && value.starts_with("sha256:")
        && value[7..].bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_token(value: &str) -> bool {
    value.len() == 72
        && value.starts_with("replace-")
        && value[8..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn digest(value: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(value))
}

fn hex(value: &[u8]) -> String {
    let mut output = String::with_capacity(value.len() * 2);
    for byte in value {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn current_time_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX)
}
