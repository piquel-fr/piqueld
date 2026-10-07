//! Environment lifecycle, status, and repair. Deployment, history, logs, and
//! secrets of an environment live with their own handlers.
use super::applications::{ForceQuery, GenerationQuery, accept_mutation, request_body};
use super::{ApiError, ApiPath, ApiState, ok, openapi::ApiErrorResponse};
use crate::api::Mutation;
use axum::{
    body::Bytes,
    extract::{Query, State, rejection::BytesRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use piqueld_core::api::{
    AcceptedOperation, CreateEnvironmentRequest, Envelope, EnvironmentBranchRequest,
    EnvironmentDetailView, EnvironmentRequest, EnvironmentStatusView, EnvironmentView,
};
use piqueld_core::{ApplicationId, EnvironmentId, EnvironmentName, TrackedBranch};

/// Decodes a JSON environment request, requiring the JSON content type.
fn environment_request<T: serde::de::DeserializeOwned>(
    headers: &HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<T, ApiError> {
    if super::content_type(headers) != Some("application/json") {
        return Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "content_type_unsupported",
            "Content-Type must be application/json",
        ));
    }
    super::decode_json(&request_body(body)?)
}

/// Adds an environment to an application.
///
/// The environment starts `not_deployed`. It deploys the application's saved
/// manifest or, when the application is repository-backed, the manifest on
/// `branch` (by default the branch `spec.manifest` names), optionally pinned
/// to `commit`. A branch for an application without a repository fails with
/// `manifest_repository_required`. The inspected application
/// `expected_generation` goes in the JSON body.
#[utoipa::path(post,path="/api/v1/applications/{id}/environments",operation_id="createEnvironment",
    params(("id"=String,Path),ForceQuery,("Idempotency-Key"=Option<String>,Header)),
    request_body=CreateEnvironmentRequest,
    responses((status=200,description="Environment created",body=Envelope<EnvironmentView>),
    (status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),
    (status=409,response=inline(ApiErrorResponse)),(status=415,response=inline(ApiErrorResponse)),
    (status=500,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn create(
    State(state): State<ApiState>,
    query: Result<Query<ForceQuery>, axum::extract::rejection::QueryRejection>,
    ApiPath(id): ApiPath<String>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let request: CreateEnvironmentRequest = environment_request(&headers, body)?;
    let branch = match (request.branch, request.commit) {
        (Some(branch), commit) => Some(TrackedBranch::new(branch, commit)?),
        (None, None) => None,
        (None, Some(_)) => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "revision_invalid",
                "a pinned commit requires a branch",
            ));
        }
    };
    accept_mutation(
        &state,
        Mutation::CreateEnvironment {
            application: ApplicationId::parse(id)?,
            name: EnvironmentName::parse(request.name)?,
            branch,
        },
        request.expected_generation,
        ForceQuery::decode(query)?.force,
        &headers,
    )
    .await
}

#[utoipa::path(
    get,
    path = "/api/v1/environments/{id}",
    operation_id = "getEnvironment",
    summary = "Get an environment",
    params(("id" = String, Path, min_length = 8, max_length = 64)),
    responses(
        (status = 200, description = "Success", body = Envelope<EnvironmentView>),
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
    Ok(ok(state.environment(&EnvironmentId::parse(id)?).await?))
}

// Combines saved intent with a live runtime observation; runtime outages are
// reported as diagnostics rather than failing the request.
#[utoipa::path(
    get,
    path = "/api/v1/environments/{id}/detail",
    operation_id = "getEnvironmentDetail",
    summary = "Get desired and observed environment state",
    params(("id" = String, Path, min_length = 8, max_length = 64)),
    responses(
        (status = 200, description = "Success", body = Envelope<EnvironmentDetailView>),
        (status = 400, response = inline(ApiErrorResponse)),
        (status = 404, response = inline(ApiErrorResponse)),
        (status = 502, response = inline(ApiErrorResponse)),
        (status = 500, response = inline(ApiErrorResponse)),
        (status = 503, response = inline(ApiErrorResponse)),
    )
)]
pub(super) async fn detail(
    State(state): State<ApiState>,
    ApiPath(id): ApiPath<String>,
) -> Result<impl IntoResponse, ApiError> {
    Ok(ok(state
        .environment_detail(&EnvironmentId::parse(id)?)
        .await?))
}

#[utoipa::path(
    get,
    path = "/api/v1/environments/{id}/status",
    operation_id = "environmentStatus",
    summary = "Get environment status",
    params(("id" = String, Path, min_length = 8, max_length = 64)),
    responses(
        (status = 200, description = "Success", body = Envelope<EnvironmentStatusView>),
        (status = 400, response = inline(ApiErrorResponse)),
        (status = 404, response = inline(ApiErrorResponse)),
        (status = 500, response = inline(ApiErrorResponse)),
        (status = 503, response = inline(ApiErrorResponse)),
    )
)]
pub(super) async fn status(
    State(state): State<ApiState>,
    ApiPath(id): ApiPath<String>,
) -> Result<impl IntoResponse, ApiError> {
    Ok(ok(state
        .environment_status(&EnvironmentId::parse(id)?)
        .await?))
}

/// Renames an environment.
///
/// Changes only the name, without redeploying. The inspected application
/// `expected_generation` goes in the JSON body; the new name must not already
/// be used by another environment of the application, even with `force=true`.
#[utoipa::path(post,path="/api/v1/environments/{id}/rename",operation_id="renameEnvironment",
    params(("id"=String,Path),ForceQuery,("Idempotency-Key"=Option<String>,Header)),
    request_body=EnvironmentRequest,
    responses((status=200,description="Environment renamed without redeployment",body=Envelope<EnvironmentView>),
    (status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),
    (status=409,response=inline(ApiErrorResponse)),(status=415,response=inline(ApiErrorResponse)),
    (status=500,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn rename(
    State(state): State<ApiState>,
    query: Result<Query<ForceQuery>, axum::extract::rejection::QueryRejection>,
    ApiPath(id): ApiPath<String>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let request: EnvironmentRequest = environment_request(&headers, body)?;
    accept_mutation(
        &state,
        Mutation::RenameEnvironment {
            id: EnvironmentId::parse(id)?,
            name: EnvironmentName::parse(request.name)?,
        },
        request.expected_generation,
        ForceQuery::decode(query)?.force,
        &headers,
    )
    .await
}

/// Changes the branch an environment follows.
///
/// Points an environment of a repository-backed application at another branch
/// of its manifest repository, or pins or unpins a commit. Nothing is fetched
/// or redeployed: the next deployment fetches the new branch. Environments
/// deploying the saved manifest fail with `manifest_repository_required`. The
/// inspected application `expected_generation` goes in the JSON body.
#[utoipa::path(put,path="/api/v1/environments/{id}/branch",operation_id="setEnvironmentBranch",
    params(("id"=String,Path),ForceQuery,("Idempotency-Key"=Option<String>,Header)),
    request_body=EnvironmentBranchRequest,
    responses((status=200,description="Environment follows the branch from its next deployment",body=Envelope<EnvironmentView>),
    (status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),
    (status=409,response=inline(ApiErrorResponse)),(status=415,response=inline(ApiErrorResponse)),
    (status=500,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn branch(
    State(state): State<ApiState>,
    query: Result<Query<ForceQuery>, axum::extract::rejection::QueryRejection>,
    ApiPath(id): ApiPath<String>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let request: EnvironmentBranchRequest = environment_request(&headers, body)?;
    accept_mutation(
        &state,
        Mutation::SetBranch {
            id: EnvironmentId::parse(id)?,
            branch: TrackedBranch::new(request.branch, request.commit)?,
        },
        request.expected_generation,
        ForceQuery::decode(query)?.force,
        &headers,
    )
    .await
}

// Accepts a deletion operation; named volumes are retained.
#[utoipa::path(
    delete, path = "/api/v1/environments/{id}", operation_id = "deleteEnvironment",
    summary = "Delete an environment's services and networks, retaining volumes",
    params(("id" = String, Path, min_length = 8, max_length = 64), GenerationQuery,("Idempotency-Key"=Option<String>,Header)),
    responses(
        (status = 409, response = inline(ApiErrorResponse)),
        (status = 202, description = "Deletion operation", body = Envelope<AcceptedOperation>),
        (status = 400, response = inline(ApiErrorResponse)),
        (status = 404, response = inline(ApiErrorResponse)),
        (status = 500, response = inline(ApiErrorResponse)),
        (status = 503, response = inline(ApiErrorResponse))
    )
)]
pub(super) async fn delete(
    State(state): State<ApiState>,
    ApiPath(id): ApiPath<String>,
    headers: HeaderMap,
    query: Result<Query<GenerationQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Response, ApiError> {
    let query = GenerationQuery::decode(query)?;
    accept_mutation(
        &state,
        Mutation::Delete {
            id: EnvironmentId::parse(id)?,
        },
        query.expected_generation,
        query.force,
        &headers,
    )
    .await
}

/// Reconciles an environment.
///
/// Returns 202 with a durable operation that repairs the runtime to match the
/// accepted configuration. Unlike other mutations, `expected_generation` is
/// optional. Repeating a request with the same `Idempotency-Key` returns the
/// original response.
#[utoipa::path(post,path="/api/v1/environments/{id}/reconcile",operation_id="reconcileEnvironment",
    params(("id"=String,Path),GenerationQuery,("Idempotency-Key"=Option<String>,Header)),
    responses((status=202,description="Reconciliation accepted",body=Envelope<AcceptedOperation>),
    (status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),
    (status=409,response=inline(ApiErrorResponse)),(status=500,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn reconcile(
    State(state): State<ApiState>,
    ApiPath(id): ApiPath<String>,
    headers: HeaderMap,
    query: Result<Query<GenerationQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Response, ApiError> {
    let query = GenerationQuery::decode(query)?;
    accept_mutation(
        &state,
        Mutation::Reconcile {
            id: EnvironmentId::parse(id)?,
        },
        query.expected_generation,
        query.force,
        &headers,
    )
    .await
}
