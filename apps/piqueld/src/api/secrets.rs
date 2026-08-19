use axum::{
    body::Bytes,
    extract::{
        Path, Query, State,
        rejection::{BytesRejection, QueryRejection},
    },
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use piqueld_client::{Envelope, Page, SecretMetadata, SecretReferenceView};
use serde::Deserialize;

use super::{ApiError, ApiState, content_type, header_text, ok, openapi::ApiErrorResponse};
use crate::secrets::{MAX_SECRET_BYTES, PlaintextSecret, SecretService};
use crate::store::DEFAULT_PAGE_SIZE;

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(super) struct ListQuery {
    cursor: Option<String>,
    #[param(minimum = 1, maximum = 100, default = 50)]
    limit: Option<usize>,
}

fn service(state: &ApiState) -> Result<&std::sync::Arc<SecretService>, ApiError> {
    state.secret_service().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "master_key_unavailable",
            "master encryption key is not configured",
        )
    })
}

fn view(value: crate::secrets::SecretMetadata) -> SecretMetadata {
    SecretMetadata {
        name: value.name,
        value_is_set: value.value_is_set,
        generation: value.generation,
        created_at_ms: value.created_at_ms,
        updated_at_ms: value.updated_at_ms,
        references: value
            .references
            .into_iter()
            .map(|reference| SecretReferenceView {
                application_id: reference.application_id,
                application_name: reference.application_name,
                service: reference.service,
                deployed: reference.deployed,
            })
            .collect(),
    }
}

fn request_body(body: Result<Bytes, BytesRejection>) -> Result<Bytes, ApiError> {
    let body = body.map_err(|rejection| {
        if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
            ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request_body_too_large",
                "request body exceeds the maximum allowed size",
            )
        } else {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "request_body_unreadable",
                "request body could not be read",
            )
        }
    })?;
    if body.len() > MAX_SECRET_BYTES {
        return Err(ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "request_body_too_large",
            "request body exceeds the maximum allowed size",
        ));
    }
    Ok(body)
}

fn raw(
    headers: &HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<PlaintextSecret, ApiError> {
    if content_type(headers) != Some("application/octet-stream") {
        return Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "content_type_unsupported",
            "Content-Type must be application/octet-stream",
        ));
    }
    PlaintextSecret::new(request_body(body)?.to_vec()).map_err(Into::into)
}

#[utoipa::path(
    get,
    path = "/api/v1/secrets",
    operation_id = "listSecrets",
    summary = "List logical secret metadata",
    params(ListQuery),
    responses(
        (status = 200, description = "Success", body = Envelope<Page<SecretMetadata>>),
        (status = 400, response = inline(ApiErrorResponse)),
        (status = 500, response = inline(ApiErrorResponse)),
        (status = 503, response = inline(ApiErrorResponse)),
    )
)]
pub(super) async fn list(
    State(state): State<ApiState>,
    query: Result<Query<ListQuery>, QueryRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let Query(query) = query.map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "pagination_invalid",
            "pagination parameters are invalid",
        )
    })?;
    let page = service(&state)?
        .list_page(
            query.cursor.as_deref(),
            query.limit.unwrap_or(DEFAULT_PAGE_SIZE),
        )
        .await?;
    Ok(ok(Page {
        items: page.items.into_iter().map(view).collect(),
        next_cursor: page.next_cursor,
    }))
}

#[utoipa::path(
    post,
    path = "/api/v1/secrets",
    operation_id = "createSecret",
    summary = "Create an encrypted logical secret",
    params(("X-Secret-Name" = String, Header, min_length = 1, max_length = 63)),
    request_body(content = String, content_type = "application/octet-stream"),
    responses(
        (status = 201, description = "Created", body = Envelope<SecretMetadata>),
        (status = 400, response = inline(ApiErrorResponse)),
        (status = 409, response = inline(ApiErrorResponse)),
        (status = 413, response = inline(ApiErrorResponse)),
        (status = 415, response = inline(ApiErrorResponse)),
        (status = 503, response = inline(ApiErrorResponse)),
    )
)]
pub(super) async fn create_header(
    State(state): State<ApiState>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let _maintenance = state.ordinary_lease()?;
    let name = header_text(&headers, "x-secret-name")
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "secret_name_required",
                "X-Secret-Name is required",
            )
        })?
        .to_owned();
    let value = raw(&headers, body)?;
    Ok((
        StatusCode::CREATED,
        axum::Json(Envelope {
            data: view(service(&state)?.create(&name, value).await?),
        }),
    ))
}

#[utoipa::path(
    get,
    path = "/api/v1/secrets/{name}",
    operation_id = "getSecret",
    summary = "Get logical secret metadata",
    params(("name" = String, Path, min_length = 1, max_length = 63)),
    responses(
        (status = 200, description = "Success", body = Envelope<SecretMetadata>),
        (status = 400, response = inline(ApiErrorResponse)),
        (status = 404, response = inline(ApiErrorResponse)),
        (status = 503, response = inline(ApiErrorResponse)),
    )
)]
pub(super) async fn get(
    State(state): State<ApiState>,
    Path(name): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    Ok(ok(view(service(&state)?.get(&name).await?)))
}

#[utoipa::path(
    post,
    path = "/api/v1/secrets/{name}",
    operation_id = "createNamedSecret",
    summary = "Create an encrypted logical secret",
    params(("name" = String, Path, min_length = 1, max_length = 63)),
    request_body(content = String, content_type = "application/octet-stream"),
    responses(
        (status = 201, description = "Created", body = Envelope<SecretMetadata>),
        (status = 400, response = inline(ApiErrorResponse)),
        (status = 409, response = inline(ApiErrorResponse)),
        (status = 413, response = inline(ApiErrorResponse)),
        (status = 415, response = inline(ApiErrorResponse)),
        (status = 503, response = inline(ApiErrorResponse)),
    )
)]
pub(super) async fn create(
    State(state): State<ApiState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let _maintenance = state.ordinary_lease()?;
    let value = raw(&headers, body)?;
    Ok((
        StatusCode::CREATED,
        axum::Json(Envelope {
            data: view(service(&state)?.create(&name, value).await?),
        }),
    ))
}

#[utoipa::path(
    put,
    path = "/api/v1/secrets/{name}",
    operation_id = "replaceSecret",
    summary = "Rotate an encrypted logical secret",
    params(("name" = String, Path, min_length = 1, max_length = 63)),
    request_body(content = String, content_type = "application/octet-stream"),
    responses(
        (status = 200, description = "Success", body = Envelope<SecretMetadata>),
        (status = 400, response = inline(ApiErrorResponse)),
        (status = 404, response = inline(ApiErrorResponse)),
        (status = 409, response = inline(ApiErrorResponse)),
        (status = 413, response = inline(ApiErrorResponse)),
        (status = 415, response = inline(ApiErrorResponse)),
        (status = 503, response = inline(ApiErrorResponse)),
    )
)]
pub(super) async fn replace(
    State(state): State<ApiState>,
    Path(name): Path<String>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let _maintenance = state.ordinary_lease()?;
    let value = raw(&headers, body)?;
    let service = service(&state)?;
    let metadata = view(service.replace(&name, value).await?);
    service.schedule_rotation(&name).await?;
    state.runtime.trigger_reconciliation();
    Ok(ok(metadata))
}

#[utoipa::path(
    delete,
    path = "/api/v1/secrets/{name}",
    operation_id = "deleteSecret",
    summary = "Delete an unreferenced logical secret",
    params(("name" = String, Path, min_length = 1, max_length = 63)),
    responses(
        (status = 204, description = "Deleted; no content is returned"),
        (status = 404, response = inline(ApiErrorResponse)),
        (status = 409, response = inline(ApiErrorResponse)),
        (status = 503, response = inline(ApiErrorResponse)),
    )
)]
pub(super) async fn delete(
    State(state): State<ApiState>,
    Path(name): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let _maintenance = state.ordinary_lease()?;
    service(&state)?.delete(&name).await?;
    Ok(StatusCode::NO_CONTENT)
}
