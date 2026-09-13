//! Write-only values and metadata-only secret lifecycle endpoints.
use super::{ApiError, ApiState, ok, openapi::ApiErrorResponse};
use axum::{
    body::Bytes,
    extract::{Path, State, rejection::BytesRejection},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use piqueld_core::{
    ApplicationId,
    api::{Envelope, SecretMetadata},
};

#[utoipa::path(get,path="/api/v1/applications/{id}/secrets",operation_id="applicationSecrets",params(("id"=String,Path)),responses((status=200,description="Secret metadata",body=Envelope<Vec<SecretMetadata>>),(status=404,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn list(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    Ok(ok(state.secrets(&ApplicationId::parse(id)?).await?))
}

fn expected(headers: &HeaderMap) -> Result<i64, ApiError> {
    headers
        .get("x-expected-generation")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse().ok())
        .filter(|v| *v >= 0)
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "secret_generation_required",
                "Supply X-Expected-Generation (0 creates a new secret)",
            )
        })
}

#[utoipa::path(put,path="/api/v1/applications/{id}/secrets/{name}",operation_id="putApplicationSecret",params(("id"=String,Path),("name"=String,Path),("X-Expected-Generation"=i64,Header)),request_body(content=String,content_type="application/octet-stream"),responses((status=200,description="Updated metadata; no secret value",body=Envelope<SecretMetadata>),(status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),(status=409,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn put(
    State(state): State<ApiState>,
    Path((id, name)): Path<(String, String)>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let generation = expected(&headers)?;
    let body = body.map_err(|e| {
        ApiError::new(
            e.status(),
            "secret_body_invalid",
            "Secret body could not be read within the request limit",
        )
    })?;
    if body.is_empty() || body.len() > 500 * 1024 {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "secret_value_invalid",
            "Secret values must contain 1–512000 bytes",
        ));
    }
    if headers
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_none_or(|v| v != "application/octet-stream")
    {
        return Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "secret_content_type",
            "Secret values use application/octet-stream",
        ));
    }
    Ok(ok(state
        .put_secret(&ApplicationId::parse(id)?, &name, generation, body.to_vec())
        .await?))
}

#[utoipa::path(delete,path="/api/v1/applications/{id}/secrets/{name}",operation_id="deleteApplicationSecret",params(("id"=String,Path),("name"=String,Path),("X-Expected-Generation"=i64,Header)),responses((status=200,description="Secret deleted",body=Envelope<bool>),(status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),(status=409,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn delete(
    State(state): State<ApiState>,
    Path((id, name)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    let generation = expected(&headers)?;
    state
        .delete_secret(&ApplicationId::parse(id)?, &name, generation)
        .await?;
    Ok(ok(true))
}
