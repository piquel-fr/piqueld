//! Applications: listing, reading, applying and planning whole manifests,
//! renaming, deletion, and manifest download. Mutations of applications and
//! their environments are submitted through [`accept_mutation`].
use super::extract::{IDEMPOTENCY_KEY, ManifestUpload, optional_header};
use super::{
    ApiError, ApiJson, ApiPath, ApiQuery, ApiState, QueryParams, accepted, ok,
    openapi::ApiErrorResponse,
};
use crate::api::{Mutation, MutationResponse};
use crate::auth::Identity;
use axum::{
    Extension,
    extract::State,
    http::{HeaderMap, header},
    response::{IntoResponse, Response},
};
use piqueld_core::api::{
    ApplicationSummary, ApplicationView, ApplyApplicationRequest, DeletedApplication, Envelope,
    Page, PlanView, RenameApplicationRequest, RenamedApplication, SavedApplication,
};
use piqueld_core::{ApplicationId, EnvironmentId, EnvironmentName};
use serde::Deserialize;

/// Cursor pagination for the application list.
#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(super) struct ListQuery {
    /// `next_cursor` from a previous page.
    cursor: Option<String>,
    /// Page size.
    #[param(minimum = 1, maximum = 100, default = 100)]
    limit: Option<u16>,
}

impl QueryParams for ListQuery {
    // The same error as an out-of-range `limit`.
    const INVALID_CODE: &'static str = "pagination_invalid";
    const INVALID_MESSAGE: &'static str = "pagination parameters are invalid";
}

#[utoipa::path(
    get,
    path = "/api/v1/applications",
    operation_id = "listApplications",
    summary = "List applications",
    params(ListQuery),
    responses(
        (status = 200, description = "Success", body = Envelope<Page<ApplicationSummary>>),
        (status = 400, response = inline(ApiErrorResponse)),
        (status = 500, response = inline(ApiErrorResponse)),
        (status = 503, response = inline(ApiErrorResponse)),
    )
)]
pub(super) async fn list(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    ApiQuery(query): ApiQuery<ListQuery>,
) -> Result<impl IntoResponse, ApiError> {
    Ok(ok(state
        .applications(
            &identity
                .grants
                .app_scope(piqueld_core::access::AppPermission::Read),
            query.cursor.as_deref(),
            query.limit,
        )
        .await?))
}

#[utoipa::path(
    get,
    path = "/api/v1/applications/{id}",
    operation_id = "getApplication",
    summary = "Get an application and its environments",
    params(("id" = String, Path, min_length = 8, max_length = 64)),
    responses(
        (status = 200, description = "Success", body = Envelope<ApplicationView>),
        (status = 400, response = inline(ApiErrorResponse)),
        (status = 404, response = inline(ApiErrorResponse)),
        (status = 500, response = inline(ApiErrorResponse)),
        (status = 503, response = inline(ApiErrorResponse)),
    )
)]
pub(super) async fn get(
    State(state): State<ApiState>,
    ApiPath(id): ApiPath<String>,
) -> Result<impl IntoResponse, ApiError> {
    Ok(ok(state.application(&ApplicationId::parse(id)?).await?))
}

// Saves a whole manifest (JSON or TOML, see `ManifestUpload`) by name,
// optionally accepting a deployment of it in the same transaction.
#[utoipa::path(
    post, path = "/api/v1/applications/apply", operation_id = "applyApplication",
    summary = "Apply an application manifest",
    params(ApplyQuery,("X-Expected-Generation"=Option<u64>,Header,description="TOML only: required unless forced; zero requires absence. JSON uses `expected_generation` in the request body."),("X-Expected-Application-Id"=Option<String>,Header,description="TOML only: inspected application identity. JSON uses `expected_application_id` in the request body."),("Idempotency-Key"=Option<String>,Header)),
    request_body(content((ApplyApplicationRequest = "application/json"), (String = "application/toml"), (String = "text/toml"))),
    responses(
        (status = 200, description = "Configuration saved", body = Envelope<SavedApplication>),
        (status = 202, description = "Configuration saved and deployment accepted", body = Envelope<SavedApplication>),
        (status = 400, response = inline(ApiErrorResponse)),
        (status = 409, response = inline(ApiErrorResponse)),
        (status = 413, response = inline(ApiErrorResponse)),
        (status = 415, response = inline(ApiErrorResponse)),
        (status = 422, response = inline(ApiErrorResponse)),
        (status = 500, response = inline(ApiErrorResponse)),
        (status = 502, response = inline(ApiErrorResponse)),
        (status = 503, response = inline(ApiErrorResponse))
    )
)]
pub(super) async fn apply(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    ApiQuery(query): ApiQuery<ApplyQuery>,
    headers: HeaderMap,
    upload: ManifestUpload,
) -> Result<Response, ApiError> {
    accept_mutation(
        &state,
        &identity,
        Mutation::save(
            upload.manifest,
            upload.expected_application_id,
            query.deploy,
        ),
        upload.expected_generation,
        query.force,
        &headers,
    )
    .await
}

#[derive(Default, Deserialize, utoipa::IntoParams)]
#[serde(default, deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(super) struct ApplyQuery {
    /// Explicitly bypass revision and identity preconditions.
    pub(super) force: bool,
    /// Deploy the saved configuration to the application's only environment or,
    /// when its manifest declares environments, every declared one not promoted
    /// from another; omission saves only.
    deploy: bool,
}

impl QueryParams for ApplyQuery {
    const INVALID_CODE: &'static str = "query_invalid";
    const INVALID_MESSAGE: &'static str = "force and deploy must be true or false";
}

/// Application deletion preconditions and confirmation.
#[derive(Default, Deserialize, utoipa::IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(super) struct DeleteApplicationQuery {
    /// Current application revision; required unless forced.
    expected_generation: Option<u64>,
    /// Explicitly bypass the revision precondition. Never skips confirmation.
    #[serde(default)]
    force: bool,
    /// Comma-separated names of every environment; required when there are several.
    environments: Option<String>,
}

impl QueryParams for DeleteApplicationQuery {
    const INVALID_CODE: &'static str = "query_invalid";
    const INVALID_MESSAGE: &'static str =
        "expected_generation must be an integer and force must be true or false";
}

// Accepts a deletion operation for every environment, whose named volumes are
// retained, and every preview, whose volumes are removed.
#[utoipa::path(
    delete, path = "/api/v1/applications/{id}", operation_id = "deleteApplication",
    summary = "Delete an application with its environments, retaining their volumes, and its previews",
    params(("id" = String, Path, min_length = 8, max_length = 64), DeleteApplicationQuery,("Idempotency-Key"=Option<String>,Header)),
    responses(
        (status = 409, response = inline(ApiErrorResponse)),
        (status = 202, description = "Deletion operations", body = Envelope<DeletedApplication>),
        (status = 400, response = inline(ApiErrorResponse)),
        (status = 404, response = inline(ApiErrorResponse)),
        (status = 500, response = inline(ApiErrorResponse)),
        (status = 503, response = inline(ApiErrorResponse))
    )
)]
pub(super) async fn delete(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    ApiPath(id): ApiPath<String>,
    headers: HeaderMap,
    ApiQuery(query): ApiQuery<DeleteApplicationQuery>,
) -> Result<Response, ApiError> {
    let environments = query
        .environments
        .iter()
        .flat_map(|names| names.split(','))
        .filter(|name| !name.is_empty())
        .map(EnvironmentName::parse)
        .collect::<Result<_, _>>()?;
    accept_mutation(
        &state,
        &identity,
        Mutation::DeleteApplication {
            id: ApplicationId::parse(id)?,
            environments,
        },
        query.expected_generation,
        query.force,
        &headers,
    )
    .await
}

// Dry-runs `apply` with the same body; supplied preconditions are checked, but
// none are required and nothing is saved.
#[utoipa::path(
    post, path = "/api/v1/applications/plan", operation_id = "planApplication",
    summary = "Preview an application manifest",
    params(PlanQuery,("X-Expected-Generation"=Option<u64>,Header,description="TOML only: inspected intent revision; zero requires absence. JSON uses `expected_generation` in the request body."),("X-Expected-Application-Id"=Option<String>,Header,description="TOML only: inspected application identity. JSON uses `expected_application_id` in the request body.")),
    request_body(content((ApplyApplicationRequest = "application/json"), (String = "application/toml"), (String = "text/toml"))),
    responses(
        (status = 200, description = "Preview", body = Envelope<PlanView>),
        (status = 409, response = inline(ApiErrorResponse)),
        (status = 400, response = inline(ApiErrorResponse)),
        (status = 413, response = inline(ApiErrorResponse)),
        (status = 415, response = inline(ApiErrorResponse)),
        (status = 422, response = inline(ApiErrorResponse)),
        (status = 500, response = inline(ApiErrorResponse)),
        (status = 502, response = inline(ApiErrorResponse)),
        (status = 503, response = inline(ApiErrorResponse))
    )
)]
pub(super) async fn plan(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    ApiQuery(query): ApiQuery<PlanQuery>,
    upload: ManifestUpload,
) -> Result<impl IntoResponse, ApiError> {
    let environment = query.environment.map(EnvironmentId::parse).transpose()?;
    Ok(ok(state
        .plan(
            &identity.grants,
            upload.manifest,
            upload.expected_generation,
            upload.expected_application_id,
            environment.as_ref(),
        )
        .await?))
}

/// Environment a preview compares against.
#[derive(Default, Deserialize, utoipa::IntoParams)]
#[serde(default, deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(super) struct PlanQuery {
    /// Environment of the application whose latest deployment and runtime the
    /// preview compares against; defaults to its only environment.
    environment: Option<String>,
}

impl QueryParams for PlanQuery {
    const INVALID_CODE: &'static str = "query_invalid";
    const INVALID_MESSAGE: &'static str = "environment must appear at most once";
}

#[derive(Default, Deserialize, utoipa::IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(super) struct GenerationQuery {
    /// Current intent revision; optional for reconcile, required for deletion unless forced.
    pub(super) expected_generation: Option<u64>,
    /// Explicitly bypass intent preconditions.
    #[serde(default)]
    pub(super) force: bool,
}

impl QueryParams for GenerationQuery {
    const INVALID_CODE: &'static str = "generation_invalid";
    const INVALID_MESSAGE: &'static str = "invalid expected generation";
}

/// Submits a mutation with the request's `Idempotency-Key` and renders the
/// acceptance: 202 when a durable operation was started (including a save that
/// also deploys, a created preview, or a deletion), 200 for a plain save,
/// rename, environment change, or an existing preview.
pub(super) async fn accept_mutation(
    state: &ApiState,
    identity: &Identity,
    mutation: Mutation,
    expected: Option<u64>,
    force: bool,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    let request_id = optional_header(headers, IDEMPOTENCY_KEY)?;
    let response = state
        .accept(
            identity.actor(),
            mutation,
            expected,
            force,
            request_id.as_deref(),
        )
        .await?;
    Ok(match response {
        MutationResponse::Saved(saved) if saved.operation_id.is_some() => accepted(saved),
        MutationResponse::Saved(saved) => ok(saved),
        MutationResponse::Preview(preview) if preview.created => accepted(preview),
        MutationResponse::Preview(preview) => ok(preview),
        MutationResponse::Rename(renamed) => ok(renamed),
        MutationResponse::Environment(environment) => ok(environment),
        MutationResponse::Operation(operation) => accepted(operation),
        MutationResponse::Promotion(promotion) => accepted(promotion),
        MutationResponse::Deleted(deleted) => accepted(deleted),
        // Only sync submits the mutations answered with this.
        MutationResponse::Synced(_) => return Err(crate::store::StoreError::Corrupt.into()),
    })
}

/// Renames an application.
///
/// Changes only the user-facing name, without redeploying. The inspected
/// `expected_generation` goes in the JSON body; the new name must not already be
/// in use, even with `force=true`.
#[utoipa::path(post,path="/api/v1/applications/{id}/rename",operation_id="renameApplication",
    params(("id"=String,Path),ForceQuery,("Idempotency-Key"=Option<String>,Header)),
    request_body=RenameApplicationRequest,
    responses((status=200,description="Application renamed without redeployment",body=Envelope<RenamedApplication>),
    (status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),
    (status=409,response=inline(ApiErrorResponse)),(status=500,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn rename(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    ApiQuery(query): ApiQuery<ForceQuery>,
    ApiPath(id): ApiPath<String>,
    headers: HeaderMap,
    ApiJson(request): ApiJson<RenameApplicationRequest>,
) -> Result<Response, ApiError> {
    accept_mutation(
        &state,
        &identity,
        Mutation::Rename {
            id: ApplicationId::parse(id)?,
            name: request.name,
        },
        request.expected_generation,
        query.force,
        &headers,
    )
    .await
}

#[derive(Default, Deserialize, utoipa::IntoParams)]
#[serde(default, deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(super) struct ForceQuery {
    /// Explicitly bypass the intent revision precondition. Name availability is always enforced.
    pub(super) force: bool,
}

impl QueryParams for ForceQuery {
    const INVALID_CODE: &'static str = "force_invalid";
    const INVALID_MESSAGE: &'static str = "force must be true or false";
}

/// Downloads the saved configuration as a TOML manifest.
///
/// Reads only saved configuration, so it works while the runtime is unavailable.
#[utoipa::path(get,path="/api/v1/applications/{id}/manifest",operation_id="downloadApplicationManifest",
    params(("id"=String,Path)),
    responses((status=200,description="Saved application configuration",body=String,content_type="application/toml",
        headers(("Content-Disposition"=String,description="Attachment filename for the saved TOML manifest"),
                ("Cache-Control"=String,description="no-store"))),
    (status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),
    (status=500,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn manifest_download(
    State(state): State<ApiState>,
    ApiPath(id): ApiPath<String>,
) -> Result<impl IntoResponse, ApiError> {
    let manifest = state.manifest(&ApplicationId::parse(id)?).await?;
    let filename = format!("attachment; filename=\"{}\"", manifest.filename);
    Ok((
        [
            (header::CONTENT_TYPE, "application/toml".to_owned()),
            (header::CONTENT_DISPOSITION, filename),
            (header::CACHE_CONTROL, "no-store".to_owned()),
        ],
        manifest.contents,
    ))
}
