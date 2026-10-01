//! Explicit deployment acceptance and durable history.
use super::{ApiError, ApiPath, ApiState, ok, openapi::ApiErrorResponse};
use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use piqueld_core::{
    ApplicationId, Operation,
    api::{AcceptedOperation, DeploymentView, Envelope, Page},
    manifest::ManifestRevision,
};

#[derive(Default, serde::Deserialize, utoipa::IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in=Query)]
pub(super) struct HistoryQuery {
    /// `next_cursor` from a previous page.
    cursor: Option<String>,
}

/// Deployment preconditions plus an optional one-time manifest revision.
#[derive(Default, serde::Deserialize, utoipa::IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in=Query)]
pub(super) struct DeployQuery {
    /// Current intent revision; required unless forced.
    expected_generation: Option<u64>,
    /// Explicitly bypass intent preconditions.
    #[serde(default)]
    force: bool,
    /// Fetch the repository manifest from this branch head, without saving it.
    branch: Option<String>,
    /// Fetch the repository manifest from this full commit, without saving it.
    commit: Option<String>,
}

impl DeployQuery {
    fn revision(&mut self) -> Result<Option<ManifestRevision>, ApiError> {
        match (self.branch.take(), self.commit.take()) {
            (None, None) => Ok(None),
            (Some(branch), None) => Ok(Some(ManifestRevision::Branch(branch))),
            (None, Some(commit)) => Ok(Some(ManifestRevision::Commit(commit))),
            (Some(_), Some(_)) => Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "revision_invalid",
                "select either a branch or a commit",
            )),
        }
    }
}

/// Deploys the saved configuration.
///
/// Returns 202 with the accepted durable operation. Requires
/// `expected_generation` unless `force=true`; a stale generation fails with 409.
/// Repeating a request with the same `Idempotency-Key` returns the original
/// response.
#[utoipa::path(post,path="/api/v1/applications/{id}/deploy",operation_id="deployApplication",
    params(("id"=String,Path),DeployQuery,("Idempotency-Key"=Option<String>,Header)),
    responses((status=202,description="Deployment accepted",body=Envelope<AcceptedOperation>),
    (status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),
    (status=409,response=inline(ApiErrorResponse)),(status=500,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn deploy(
    State(state): State<ApiState>,
    ApiPath(id): ApiPath<String>,
    headers: HeaderMap,
    query: Result<Query<DeployQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Response, ApiError> {
    let mut query = super::applications::GenerationQuery::decode(query)?;
    super::applications::accept_mutation(
        &state,
        crate::api::Mutation::Deploy {
            id: ApplicationId::parse(id)?,
            revision: query.revision()?,
        },
        query.expected_generation,
        query.force,
        &headers,
    )
    .await
}

/// Lists deployments of an application, newest first.
///
/// Returns three deployment snapshots per page; follow `next_cursor` for older ones.
#[utoipa::path(get,path="/api/v1/applications/{id}/deployments",operation_id="listDeployments",
    params(("id"=String,Path),HistoryQuery),
    responses((status=200,description="Deployment snapshots, newest first (three per page)",body=Envelope<Page<DeploymentView>>),
    (status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),(status=500,response=inline(ApiErrorResponse))))]
pub(super) async fn list(
    State(state): State<ApiState>,
    ApiPath(id): ApiPath<String>,
    query: Result<Query<HistoryQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let Query(query) = query.map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "pagination_invalid",
            "invalid pagination parameters",
        )
    })?;
    Ok(ok(state
        .deployments(&ApplicationId::parse(id)?, query.cursor.as_deref())
        .await?))
}

/// Lists attempts of one deployment, newest first.
///
/// Returns up to 100 retained attempt outcomes per page. Deployments owned by
/// another application are reported as not found.
#[utoipa::path(get,path="/api/v1/applications/{id}/deployments/{deployment}/attempts",operation_id="listDeploymentAttempts",
    params(("id"=String,Path),("deployment"=String,Path),HistoryQuery),
    responses((status=200,description="Retained attempt outcomes, newest first (100 per page)",body=Envelope<Page<Operation>>),
    (status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),(status=500,response=inline(ApiErrorResponse))))]
pub(super) async fn attempts(
    State(state): State<ApiState>,
    ApiPath((id, deployment)): ApiPath<(String, String)>,
    query: Result<Query<HistoryQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let Query(query) = query.map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "pagination_invalid",
            "invalid pagination parameters",
        )
    })?;
    Ok(ok(state
        .deployment_attempts(
            &ApplicationId::parse(id)?,
            &deployment,
            query.cursor.as_deref(),
        )
        .await?))
}
