//! Previews: disposable deployments of a branch. Creating, deploying, and
//! deleting one need no application revision. A preview's status, logs,
//! deployments, and secrets use the environment endpoints with its ID.
use super::applications::accept_mutation;
use super::environments::environment_request;
use super::{ApiError, ApiPath, ApiState, ok, openapi::ApiErrorResponse};
use crate::api::{Mutation, PreviewMutation};
use crate::auth::Identity;
use axum::{
    Extension,
    body::Bytes,
    extract::{State, rejection::BytesRejection},
    http::HeaderMap,
    response::{IntoResponse, Response},
};
use piqueld_core::api::{
    AcceptedOperation, CreatePreviewRequest, CreatedPreview, DeletedPreview, Envelope, PreviewView,
    PrunePreviewsRequest,
};
use piqueld_core::{ApplicationId, EnvironmentId, GitBranch, PreviewSlot};

/// Creates a preview of a branch.
///
/// Creates and deploys a preview of `branch` of the application's manifest
/// repository, distinguished by `slot` when several previews share a branch.
/// Repeating the request returns the existing preview of that branch and
/// slot with its latest operation, without redeploying it. Applications
/// without a manifest repository fail with `preview_requires_repository`.
/// A new preview beyond a `[previews]` limit fails with 409
/// `preview_limit_reached`, whose `details` are a `PreviewLimitReached`
/// listing the previews it counts. Needs no application revision.
#[utoipa::path(post,path="/api/v1/applications/{id}/previews",operation_id="createPreview",
    params(("id"=String,Path),("Idempotency-Key"=Option<String>,Header)),
    request_body=CreatePreviewRequest,
    responses((status=202,description="Preview created and its first deployment accepted",body=Envelope<CreatedPreview>),
    (status=200,description="The preview of this branch and slot already exists",body=Envelope<CreatedPreview>),
    (status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),
    (status=409,response=inline(ApiErrorResponse)),(status=415,response=inline(ApiErrorResponse)),
    (status=500,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn create(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    ApiPath(id): ApiPath<String>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let request: CreatePreviewRequest = environment_request(&headers, body)?;
    let mutation = Mutation::Preview(PreviewMutation::Create {
        application: ApplicationId::parse(id)?,
        branch: GitBranch::parse(request.branch)?,
        slot: request.slot.map(PreviewSlot::parse).transpose()?,
    });
    accept_mutation(&state, &identity, mutation, None, false, &headers).await
}

/// Lists an application's previews.
///
/// Each preview comes with its status, latest operation, hostnames, and
/// branch state, read with one `git ls-remote` of the manifest repository.
/// Repository failures make every branch state `unknown`.
#[utoipa::path(get,path="/api/v1/applications/{id}/previews",operation_id="listPreviews",
    params(("id"=String,Path)),
    responses((status=200,description="Success",body=Envelope<Vec<PreviewView>>),
    (status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),
    (status=500,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn list(
    State(state): State<ApiState>,
    ApiPath(id): ApiPath<String>,
) -> Result<impl IntoResponse, ApiError> {
    Ok(ok(state.previews(&ApplicationId::parse(id)?).await?))
}

/// Gets a preview with its branch state.
#[utoipa::path(get,path="/api/v1/previews/{id}",operation_id="getPreview",
    params(("id"=String,Path,min_length=8,max_length=64)),
    responses((status=200,description="Success",body=Envelope<PreviewView>),
    (status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),
    (status=500,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn get(
    State(state): State<ApiState>,
    ApiPath(id): ApiPath<String>,
) -> Result<impl IntoResponse, ApiError> {
    Ok(ok(state.preview(&EnvironmentId::parse(id)?).await?))
}

/// Deploys the head of a preview's branch.
#[utoipa::path(post,path="/api/v1/previews/{id}/deploy",operation_id="deployPreview",
    params(("id"=String,Path),("Idempotency-Key"=Option<String>,Header)),
    responses((status=202,description="Deployment accepted",body=Envelope<AcceptedOperation>),
    (status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),
    (status=409,response=inline(ApiErrorResponse)),(status=500,response=inline(ApiErrorResponse)),
    (status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn deploy(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    ApiPath(id): ApiPath<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let mutation = Mutation::Preview(PreviewMutation::Deploy {
        id: EnvironmentId::parse(id)?,
    });
    accept_mutation(&state, &identity, mutation, None, false, &headers).await
}

/// Deletes a preview with its services, network, secrets, and every volume
/// it ever created; the deletion verifies the volumes are gone.
#[utoipa::path(delete,path="/api/v1/previews/{id}",operation_id="deletePreview",
    params(("id"=String,Path,min_length=8,max_length=64),("Idempotency-Key"=Option<String>,Header)),
    responses((status=202,description="Deletion operation",body=Envelope<AcceptedOperation>),
    (status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),
    (status=409,response=inline(ApiErrorResponse)),(status=500,response=inline(ApiErrorResponse)),
    (status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn delete(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    ApiPath(id): ApiPath<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let mutation = Mutation::Preview(PreviewMutation::Delete {
        id: EnvironmentId::parse(id)?,
    });
    accept_mutation(&state, &identity, mutation, None, false, &headers).await
}

/// Deletes previews whose branch is gone.
///
/// Deletes each listed preview whose branch the manifest repository confirms
/// is gone when the request runs, and keeps the others. When the repository
/// cannot be read, fails with `repository_unavailable` and deletes nothing.
#[utoipa::path(post,path="/api/v1/applications/{id}/previews/prune",operation_id="prunePreviews",
    params(("id"=String,Path)),
    request_body=PrunePreviewsRequest,
    responses((status=200,description="Previews whose deletion was accepted",body=Envelope<Vec<DeletedPreview>>),
    (status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),
    (status=409,response=inline(ApiErrorResponse)),(status=415,response=inline(ApiErrorResponse)),
    (status=500,response=inline(ApiErrorResponse)),(status=502,response=inline(ApiErrorResponse)),
    (status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn prune(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    ApiPath(id): ApiPath<String>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let request: PrunePreviewsRequest = environment_request(&headers, body)?;
    let application = ApplicationId::parse(id)?;
    Ok(ok(state
        .prune_previews(identity.actor(), &application, &request.previews)
        .await?))
}
