//! Write-only values and metadata-only secret lifecycle endpoints.
use super::{ApiError, ApiPath, ApiState, ok, openapi::ApiErrorResponse};
use axum::{
    body::Bytes,
    extract::{State, rejection::BytesRejection},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use piqueld_core::{
    EnvironmentId,
    api::{Envelope, SecretMetadata},
};

/// `OpenAPI` representation of an opaque octet-stream upload.
struct SecretValue;
impl utoipa::ToSchema for SecretValue {}

impl utoipa::PartialSchema for SecretValue {
    fn schema() -> utoipa::openapi::RefOr<utoipa::openapi::schema::Schema> {
        use utoipa::openapi::schema::{KnownFormat, ObjectBuilder, SchemaFormat, Type};
        ObjectBuilder::new()
            .schema_type(Type::String)
            .format(Some(SchemaFormat::KnownFormat(KnownFormat::Binary)))
            .into()
    }
}

/// Lists an environment's secrets.
///
/// Returns metadata only; secret values are write-only and never returned.
#[utoipa::path(get,path="/api/v1/environments/{id}/secrets",operation_id="environmentSecrets",params(("id"=String,Path)),responses((status=200,description="Secret metadata",body=Envelope<Vec<SecretMetadata>>),(status=404,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn list(
    State(state): State<ApiState>,
    ApiPath(id): ApiPath<String>,
) -> Result<impl IntoResponse, ApiError> {
    Ok(ok(state.secrets(&EnvironmentId::parse(id)?).await?))
}

/// Reads the mandatory non-negative `X-Expected-Generation` header; `0` means
/// the secret must not exist yet.
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
/// Creates or replaces a secret value.
///
/// The body is the raw value (`application/octet-stream`, 1–512000 bytes).
/// `X-Expected-Generation` must be 0 to create a secret, or its current
/// generation to replace it; a mismatch fails with 409. Running services keep
/// their value until the next deployment. The response carries metadata only.
#[utoipa::path(put,path="/api/v1/environments/{id}/secrets/{name}",operation_id="putEnvironmentSecret",params(("id"=String,Path),("name"=String,Path),("X-Expected-Generation"=i64,Header)),request_body(content=inline(SecretValue),content_type="application/octet-stream"),responses((status=200,description="Updated metadata; no secret value",body=Envelope<SecretMetadata>),(status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),(status=409,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn put(
    State(state): State<ApiState>,
    axum::Extension(identity): axum::Extension<crate::auth::Identity>,
    ApiPath((id, name)): ApiPath<(String, String)>,
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
        .put_secret(
            identity.actor(),
            &EnvironmentId::parse(id)?,
            &name,
            generation,
            body.to_vec(),
        )
        .await?))
}

/// Deletes a secret.
///
/// Fails with 409 while the configuration or a deployment still references it.
/// If cleanup is interrupted, retrying the deletion finishes it.
#[utoipa::path(delete,path="/api/v1/environments/{id}/secrets/{name}",operation_id="deleteEnvironmentSecret",params(("id"=String,Path),("name"=String,Path),("X-Expected-Generation"=i64,Header)),responses((status=200,description="Secret deleted",body=Envelope<bool>),(status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),(status=409,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn delete(
    State(state): State<ApiState>,
    axum::Extension(identity): axum::Extension<crate::auth::Identity>,
    ApiPath((id, name)): ApiPath<(String, String)>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    let generation = expected(&headers)?;
    state
        .delete_secret(
            identity.actor(),
            &EnvironmentId::parse(id)?,
            &name,
            generation,
        )
        .await?;
    Ok(ok(true))
}

/// Recovers from a lost secret master key.
///
/// Discards stored secret values for all environments; metadata and running
/// services are kept. Fails with 409 while the current key still works. The next
/// value write generates a new key.
#[utoipa::path(post,path="/api/v1/system/secrets/recover-key",operation_id="recoverSecretKey",
    responses((status=200,description="Values discarded; the next value write generates a new key",body=Envelope<piqueld_core::api::SecretKeyRecovery>),
    (status=409,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn recover_key(
    State(state): State<ApiState>,
    axum::Extension(identity): axum::Extension<crate::auth::Identity>,
) -> Result<impl IntoResponse, ApiError> {
    Ok(ok(state.recover_secret_key(identity.actor()).await?))
}
