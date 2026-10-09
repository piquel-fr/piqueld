//! Explicit deployment acceptance and durable history.
use super::{ApiError, ApiPath, ApiState, ok, openapi::ApiErrorResponse};
use axum::{
    extract::{Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use piqueld_core::{
    ApplicationId, EnvironmentId, Operation,
    api::{AcceptedOperation, DeploymentView, Envelope, Page, ReleaseView},
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
    /// Unwraps the query, mapping rejections to 400 `query_invalid`.
    fn decode(
        query: Result<Query<Self>, axum::extract::rejection::QueryRejection>,
    ) -> Result<Self, ApiError> {
        query.map(|Query(value)| value).map_err(|_| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "query_invalid",
                "expected_generation must be an integer, force must be true or false, and branch or commit may appear once",
            )
        })
    }

    /// The one-time manifest revision, if `branch` or `commit` was given.
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

/// Deploys the application's saved configuration to an environment.
///
/// Returns 202 with the accepted durable operation. Requires
/// `expected_generation` unless `force=true`; a stale generation fails with 409.
/// Repeating a request with the same `Idempotency-Key` returns the original
/// response.
#[utoipa::path(post,path="/api/v1/environments/{id}/deploy",operation_id="deployEnvironment",
    params(("id"=String,Path),DeployQuery,("Idempotency-Key"=Option<String>,Header)),
    responses((status=202,description="Deployment accepted",body=Envelope<AcceptedOperation>),
    (status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),
    (status=409,response=inline(ApiErrorResponse)),(status=500,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn deploy(
    State(state): State<ApiState>,
    axum::Extension(identity): axum::Extension<crate::auth::Identity>,
    ApiPath(id): ApiPath<String>,
    headers: HeaderMap,
    query: Result<Query<DeployQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Response, ApiError> {
    let mut query = DeployQuery::decode(query)?;
    super::applications::accept_mutation(
        &state,
        &identity,
        crate::api::Mutation::Deploy {
            id: EnvironmentId::parse(id)?,
            revision: query.revision()?,
        },
        query.expected_generation,
        query.force,
        &headers,
    )
    .await
}

/// Lists deployments of an environment, newest first.
///
/// Returns three deployment snapshots per page; follow `next_cursor` for older ones.
#[utoipa::path(get,path="/api/v1/environments/{id}/deployments",operation_id="listDeployments",
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
        .deployments(&EnvironmentId::parse(id)?, query.cursor.as_deref())
        .await?))
}

/// Lists an application's releases, newest first.
///
/// A release is recorded by each successful preparation in a tracking
/// environment, and shared by preparations with the same content. Releases
/// outlive the environments that recorded them. Returns twenty per page;
/// follow `next_cursor` for older ones.
#[utoipa::path(get,path="/api/v1/applications/{id}/releases",operation_id="listReleases",
    params(("id"=String,Path),HistoryQuery),
    responses((status=200,description="Releases, newest first (twenty per page)",body=Envelope<Page<ReleaseView>>),
    (status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),(status=500,response=inline(ApiErrorResponse))))]
pub(super) async fn releases(
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
        .releases(&ApplicationId::parse(id)?, query.cursor.as_deref())
        .await?))
}

/// Lists attempts of one deployment, newest first.
///
/// Returns up to 100 retained attempt outcomes per page. Deployments owned by
/// another environment are reported as not found.
#[utoipa::path(get,path="/api/v1/environments/{id}/deployments/{deployment}/attempts",operation_id="listDeploymentAttempts",
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
            &EnvironmentId::parse(id)?,
            &deployment,
            query.cursor.as_deref(),
        )
        .await?))
}
