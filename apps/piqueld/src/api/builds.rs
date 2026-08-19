//! Read-only source-build status and log endpoints.

use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
};
use piqueld_client::{BuildLogView, BuildView, Envelope, Page};
use serde::Deserialize;
use utoipa::IntoParams;

use super::{ApiError, ApiState, ok};
use crate::store::{
    Build, BuildArtifact, BuildArtifactRepository, BuildLog, BuildRepository, StoreError,
};

#[utoipa::path(
    get,
    path = "/api/v1/builds/{id}",
    operation_id = "getBuild",
    summary = "Get source build status",
    params(("id" = String, Path)),
    responses(
        (status = 200, description = "Success", body = Envelope<BuildView>),
        (status = 404, response = super::openapi::ApiErrorResponse),
        (status = 500, response = super::openapi::ApiErrorResponse),
        (status = 503, response = super::openapi::ApiErrorResponse),
    )
)]
pub(super) async fn get(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    let build = state.store.build(&id).await?;
    let artifact = match state.store.build_artifact(&id).await {
        Ok(artifact) => Some(artifact),
        Err(StoreError::NotFound) => None,
        Err(error) => return Err(error.into()),
    };
    Ok(ok(build_view(build, artifact)))
}

#[utoipa::path(
    get,
    path = "/api/v1/operations/{id}/builds",
    operation_id = "getOperationBuilds",
    summary = "List builds for an operation",
    params(("id" = String, Path)),
    responses(
        (status = 200, description = "Success", body = Envelope<Page<BuildView>>),
        (status = 404, response = super::openapi::ApiErrorResponse),
        (status = 500, response = super::openapi::ApiErrorResponse),
        (status = 503, response = super::openapi::ApiErrorResponse),
    )
)]
pub(super) async fn operation_builds(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    state.store.operation(&id).await?;
    let mut items = Vec::new();
    for build in state.store.builds_for_operation(&id).await? {
        let artifact = match state.store.build_artifact(&build.id).await {
            Ok(artifact) => Some(artifact),
            Err(StoreError::NotFound) => None,
            Err(error) => return Err(error.into()),
        };
        items.push(build_view(build, artifact));
    }
    Ok(ok(Page {
        items,
        next_cursor: None,
    }))
}

#[derive(Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub(super) struct BuildLogQuery {
    /// Return entries after this sequence number.
    #[param(minimum = 0, default = 0)]
    pub after: Option<u64>,
    /// Maximum number of entries.
    #[param(minimum = 1, maximum = 1000, default = 100)]
    pub limit: Option<u32>,
}

#[utoipa::path(
    get,
    path = "/api/v1/builds/{id}/logs",
    operation_id = "getBuildLogs",
    summary = "Read source build logs",
    params(("id" = String, Path), BuildLogQuery),
    responses(
        (status = 200, description = "Success", body = Envelope<Page<BuildLogView>>),
        (status = 400, response = super::openapi::ApiErrorResponse),
        (status = 404, response = super::openapi::ApiErrorResponse),
        (status = 500, response = super::openapi::ApiErrorResponse),
        (status = 503, response = super::openapi::ApiErrorResponse),
    )
)]
pub(super) async fn logs(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    query: Result<Query<BuildLogQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<impl axum::response::IntoResponse, ApiError> {
    let Query(query) = query.map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "pagination_invalid",
            "build log pagination is invalid",
        )
    })?;
    let logs = state
        .store
        .build_logs(&id, query.after.unwrap_or(0), query.limit.unwrap_or(100))
        .await?;
    let next_cursor = logs.last().map(|log| log.sequence.to_string());
    Ok(ok(Page {
        items: logs.into_iter().map(build_log_view).collect(),
        next_cursor,
    }))
}

fn build_view(build: Build, artifact: Option<BuildArtifact>) -> BuildView {
    BuildView {
        id: build.id,
        operation_id: build.operation_id,
        application_id: build.application_id.to_string(),
        service_name: build.service_name,
        state: build.state.as_str().into(),
        source_commit: build.source_commit,
        image_reference: build.image_reference,
        image_digest: build.image_digest,
        build_key: artifact.as_ref().map(|value| value.build_key.clone()),
        context_hash: artifact.as_ref().map(|value| value.context_hash.clone()),
        verified: artifact.is_some_and(|value| value.verified),
        created_at_ms: build.created_at_ms,
        updated_at_ms: build.updated_at_ms,
        finished_at_ms: build.finished_at_ms,
    }
}

fn build_log_view(log: BuildLog) -> BuildLogView {
    BuildLogView {
        build_id: log.build_id,
        sequence: log.sequence,
        timestamp_ms: log.timestamp_ms,
        message: log.message,
    }
}
