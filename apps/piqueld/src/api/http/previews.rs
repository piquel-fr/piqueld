//! Previews: disposable deployments of a branch. Creating, deploying, and
//! deleting one need no application revision. A preview's status, logs,
//! deployments, and secrets use the environment endpoints with its ID.
use super::applications::accept_mutation;
use super::environments::environment_request;
use super::{ApiError, ApiPath, ApiState, ok, openapi::ApiErrorResponse};
use crate::api::{Mutation, MutationResponse, PreviewMutation};
use crate::auth::Identity;
use axum::{
    Extension,
    body::{Body, Bytes},
    extract::{Query, State, rejection::BytesRejection, rejection::QueryRejection},
    http::{HeaderMap, StatusCode, header},
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

/// Which preview a local snapshot deploys to.
#[derive(serde::Deserialize, utoipa::IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in=Query)]
pub(super) struct LocalPreviewQuery {
    /// The worktree's branch of the manifest repository.
    branch: String,
    /// Distinguishes several previews of one branch.
    slot: Option<String>,
}

/// A Git bundle, sent as raw bytes; it only documents the request body.
#[derive(utoipa::ToSchema)]
#[schema(value_type = String, format = Binary)]
pub(super) struct GitBundle(std::marker::PhantomData<()>);

/// Deploys a snapshot of a local worktree to a preview.
///
/// The body is a Git bundle of one snapshot commit, at most `[previews]
/// max_upload_bytes`: either thin, needing one base commit the
/// application's repository has, which the daemon fetches, or complete.
/// It is verified and unbundled into a repository of its own. The preview
/// of `branch` and `slot` is created first if needed, as `createPreview`
/// does, and deploys the snapshot's manifest, building `"self"` sources from
/// it. Fails with 413 `snapshot_too_large`, 422 `snapshot_invalid`, or 422
/// `snapshot_base_unavailable`, whose `details.base` names the base; upload
/// a complete bundle then. Needs no application revision.
#[utoipa::path(post,path="/api/v1/applications/{id}/previews/local",operation_id="deployLocalPreview",
    params(("id"=String,Path),LocalPreviewQuery,("Idempotency-Key"=Option<String>,Header)),
    request_body(content=inline(GitBundle),content_type="application/octet-stream",description="Git bundle of the snapshot commit"),
    responses((status=202,description="Snapshot received and its deployment accepted",body=Envelope<CreatedPreview>),
    (status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),
    (status=409,response=inline(ApiErrorResponse)),(status=413,response=inline(ApiErrorResponse)),
    (status=415,response=inline(ApiErrorResponse)),(status=422,response=inline(ApiErrorResponse)),
    (status=500,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn deploy_local(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    ApiPath(id): ApiPath<String>,
    query: Result<Query<LocalPreviewQuery>, QueryRejection>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    if super::content_type(&headers) != Some("application/octet-stream") {
        return Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "content_type_unsupported",
            "Content-Type must be application/octet-stream",
        ));
    }
    let Query(query) = query.map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "query_invalid",
            "branch is required, and branch and slot may appear once",
        )
    })?;
    let preview = (
        ApplicationId::parse(id)?,
        GitBranch::parse(query.branch)?,
        query.slot.map(PreviewSlot::parse).transpose()?,
    );
    let length = headers
        .get(header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok()?.parse().ok());
    let request_id = super::optional_header(&headers, "idempotency-key")?;
    let deployed = state.deploy_snapshot(
        identity.actor(),
        preview,
        (body.into_data_stream(), length),
        request_id.as_deref(),
    );
    match Box::pin(deployed).await? {
        MutationResponse::Preview(preview) => Ok(super::accepted(preview)),
        _ => Err(crate::store::StoreError::Corrupt.into()),
    }
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
