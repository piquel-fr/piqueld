//! Explicit deployment acceptance and durable history.
use super::{ApiError, ApiState, ok, openapi::ApiErrorResponse};
use axum::{
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use piqueld_core::{
    ApplicationId, Operation,
    api::{AcceptedOperation, DeploymentView, Envelope, Page},
};

#[derive(Default, serde::Deserialize, utoipa::IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in=Query)]
pub(super) struct HistoryQuery {
    cursor: Option<String>,
}

#[utoipa::path(post,path="/api/v1/applications/{id}/deploy",operation_id="deployApplication",
    params(("id"=String,Path),super::applications::GenerationQuery,("Idempotency-Key"=Option<String>,Header)),
    responses((status=202,description="Deployment accepted",body=Envelope<AcceptedOperation>),
    (status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),
    (status=409,response=inline(ApiErrorResponse)),(status=500,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn deploy(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    query: Result<
        Query<super::applications::GenerationQuery>,
        axum::extract::rejection::QueryRejection,
    >,
) -> Result<Response, ApiError> {
    let query = super::applications::GenerationQuery::decode(query)?;
    super::applications::accept_mutation(
        &state,
        crate::api::Mutation::Deploy {
            id: ApplicationId::parse(id)?,
        },
        query.expected_generation,
        query.force,
        &headers,
    )
    .await
}

#[utoipa::path(get,path="/api/v1/applications/{id}/deployments",operation_id="listDeployments",
    params(("id"=String,Path),HistoryQuery),
    responses((status=200,description="Deployment snapshots, newest first (three per page)",body=Envelope<Page<DeploymentView>>),
    (status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),(status=500,response=inline(ApiErrorResponse))))]
pub(super) async fn list(
    State(state): State<ApiState>,
    Path(id): Path<String>,
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

#[utoipa::path(get,path="/api/v1/applications/{id}/deployments/{deployment}/attempts",operation_id="listDeploymentAttempts",
    params(("id"=String,Path),("deployment"=String,Path),HistoryQuery),
    responses((status=200,description="Retained attempt outcomes, newest first (100 per page)",body=Envelope<Page<Operation>>),
    (status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),(status=500,response=inline(ApiErrorResponse))))]
pub(super) async fn attempts(
    State(state): State<ApiState>,
    Path((id, deployment)): Path<(String, String)>,
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
