//! Write-only values and metadata-only secret lifecycle endpoints: generated
//! secrets per environment, and each application's store of manually set secrets.
use super::{ApiError, ApiPath, ApiState, ok, openapi::ApiErrorResponse};
use axum::{
    body::Bytes,
    extract::{
        Query, State,
        rejection::{BytesRejection, QueryRejection},
    },
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
};
use piqueld_core::{
    ApplicationId, EnvironmentId,
    api::{Envelope, EnvironmentAccess, SecretAccess, SecretMetadata, StoredSecret},
};
use serde::Deserialize;

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

/// Lists an environment's generated secrets.
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

/// Deletes a generated secret, so a later deployment generates a new value.
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

/// Generates a new version of a generated secret from its declaration.
///
/// `X-Expected-Generation` must be the current generation; a mismatch fails
/// with 409. Rotates the value, or replaces one discarded by key recovery.
/// Running services keep their version until the next deployment. The
/// response carries metadata only.
#[utoipa::path(post,path="/api/v1/environments/{id}/secrets/{name}/regenerate",operation_id="regenerateEnvironmentSecret",params(("id"=String,Path),("name"=String,Path),("X-Expected-Generation"=i64,Header)),responses((status=200,description="Updated metadata; no secret value",body=Envelope<SecretMetadata>),(status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),(status=409,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn regenerate(
    State(state): State<ApiState>,
    axum::Extension(identity): axum::Extension<crate::auth::Identity>,
    ApiPath((id, name)): ApiPath<(String, String)>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    let generation = expected(&headers)?;
    Ok(ok(state
        .regenerate_secret(
            identity.actor(),
            &EnvironmentId::parse(id)?,
            &name,
            generation,
        )
        .await?))
}

/// Lists an application's stored secrets and who may mount them.
///
/// Returns metadata only; secret values are write-only and never returned.
#[utoipa::path(get,path="/api/v1/applications/{id}/secrets",operation_id="applicationSecrets",params(("id"=String,Path)),responses((status=200,description="Stored secret metadata and access",body=Envelope<Vec<StoredSecret>>),(status=404,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn list_stored(
    State(state): State<ApiState>,
    ApiPath(id): ApiPath<String>,
) -> Result<impl IntoResponse, ApiError> {
    Ok(ok(state.stored_secrets(&ApplicationId::parse(id)?).await?))
}

/// Access list supplied with a secret value; see `put_stored`.
#[derive(Default, Deserialize, utoipa::IntoParams)]
#[serde(default, deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(super) struct AccessQuery {
    /// Comma-separated IDs of the environments that may mount the secret;
    /// every environment when omitted.
    environments: Option<String>,
    /// Whether previews may mount the secret.
    previews: Option<bool>,
}

impl AccessQuery {
    /// The access list, when the query sets one.
    fn access(self) -> Result<Option<SecretAccess>, ApiError> {
        if self.environments.is_none() && self.previews.is_none() {
            return Ok(None);
        }
        let environments = match self.environments {
            None => EnvironmentAccess::All,
            Some(ids) => EnvironmentAccess::Only(
                ids.split(',')
                    .filter(|id| !id.is_empty())
                    .map(|id| EnvironmentId::parse(id).map_err(ApiError::from))
                    .collect::<Result<_, _>>()?,
            ),
        };
        Ok(Some(SecretAccess {
            environments,
            previews: self.previews.unwrap_or_default(),
        }))
    }
}

/// Creates or replaces a value in an application's secret store.
///
/// The body is the raw value (`application/octet-stream`, 1–512000 bytes).
/// `X-Expected-Generation` must be 0 to create a secret, or its current
/// generation to replace it; a mismatch fails with 409. Running services keep
/// their value until the next deployment. Supplying `environments` or
/// `previews` replaces the access list; a new secret otherwise allows every
/// environment and no previews, and an existing one keeps its list. Names the
/// saved manifest or an environment's last fetched one declares as generated
/// secrets fail with 422 `manifest_validation_failed` and `secret_name_conflict`
/// in `details.errors`. The response carries metadata only.
#[utoipa::path(put,path="/api/v1/applications/{id}/secrets/{name}",operation_id="putApplicationSecret",params(("id"=String,Path),("name"=String,Path),("X-Expected-Generation"=i64,Header),AccessQuery),request_body(content=inline(SecretValue),content_type="application/octet-stream"),responses((status=200,description="Updated metadata; no secret value",body=Envelope<StoredSecret>),(status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),(status=409,response=inline(ApiErrorResponse)),(status=422,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn put_stored(
    State(state): State<ApiState>,
    axum::Extension(identity): axum::Extension<crate::auth::Identity>,
    ApiPath((id, name)): ApiPath<(String, String)>,
    query: Result<Query<AccessQuery>, QueryRejection>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let generation = expected(&headers)?;
    let access = query
        .map_err(|_| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "query_invalid",
                "environments must list environment IDs; previews must be true or false",
            )
        })?
        .0
        .access()?;
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
        .put_stored_secret(
            identity.actor(),
            &ApplicationId::parse(id)?,
            &name,
            generation,
            body.to_vec(),
            access.as_ref(),
        )
        .await?))
}

/// Replaces which environments, and whether previews, may mount a stored secret.
///
/// Environments are listed by ID and must belong to the application. A
/// narrower list fails later deployments that mount the secret with
/// `secret_access_denied`; running deployments keep their pinned versions.
#[utoipa::path(put,path="/api/v1/applications/{id}/secrets/{name}/access",operation_id="setSecretAccess",params(("id"=String,Path),("name"=String,Path)),request_body=SecretAccess,responses((status=200,description="Updated metadata; no secret value",body=Envelope<StoredSecret>),(status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),(status=415,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn set_access(
    State(state): State<ApiState>,
    axum::Extension(identity): axum::Extension<crate::auth::Identity>,
    ApiPath((id, name)): ApiPath<(String, String)>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<impl IntoResponse, ApiError> {
    if super::content_type(&headers) != Some("application/json") {
        return Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "content_type_unsupported",
            "Content-Type must be application/json",
        ));
    }
    let access: SecretAccess = super::decode_json(&super::applications::request_body(body)?)?;
    Ok(ok(state
        .set_secret_access(identity.actor(), &ApplicationId::parse(id)?, &name, &access)
        .await?))
}

/// Deletes a stored secret and every environment's Docker secrets for it.
///
/// Fails with 409 while any environment's configuration or deployment still
/// references it. If cleanup is interrupted, retrying the deletion finishes it.
#[utoipa::path(delete,path="/api/v1/applications/{id}/secrets/{name}",operation_id="deleteApplicationSecret",params(("id"=String,Path),("name"=String,Path),("X-Expected-Generation"=i64,Header)),responses((status=200,description="Secret deleted",body=Envelope<bool>),(status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),(status=409,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn delete_stored(
    State(state): State<ApiState>,
    axum::Extension(identity): axum::Extension<crate::auth::Identity>,
    ApiPath((id, name)): ApiPath<(String, String)>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, ApiError> {
    let generation = expected(&headers)?;
    state
        .delete_stored_secret(
            identity.actor(),
            &ApplicationId::parse(id)?,
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
