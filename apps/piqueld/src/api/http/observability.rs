use super::{ApiError, ApiState, ok, openapi::ApiErrorResponse};
use axum::{
    extract::{Path, Query, State, rejection::QueryRejection},
    response::IntoResponse,
};
use piqueld_core::{
    Event,
    api::{Envelope, Page},
    observability::{DaemonStats, DeploymentAnalytics, NotificationDelivery},
};

#[utoipa::path(get,path="/api/v1/diagnostics/{id}",operation_id="getDiagnostic",params(("id"=String,Path)),responses((status=200,body=Envelope<Event>),(status=404,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn diagnostic(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    Ok(ok(state.diagnostic(&id).await?))
}
#[utoipa::path(get,path="/api/v1/system/resources",operation_id="systemResources",responses((status=200,body=Envelope<DaemonStats>),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn resources(
    State(state): State<ApiState>,
) -> Result<impl IntoResponse, ApiError> {
    Ok(ok(state.daemon_stats().await?))
}
#[derive(Default, serde::Deserialize, utoipa::IntoParams)]
#[serde(default, deny_unknown_fields)]
#[into_params(parameter_in=Query)]
pub(super) struct AnalyticsQuery {
    application_id: Option<String>,
    since_ms: Option<i64>,
    until_ms: Option<i64>,
}
#[utoipa::path(get,path="/api/v1/analytics/deployments",operation_id="deploymentAnalytics",params(AnalyticsQuery),responses((status=200,body=Envelope<DeploymentAnalytics>),(status=400,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn analytics(
    State(state): State<ApiState>,
    query: Result<Query<AnalyticsQuery>, QueryRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let Query(query) = query.map_err(ApiError::query)?;
    let until = query.until_ms.unwrap_or_else(crate::store::now_ms);
    Ok(ok(state
        .deployment_analytics(
            query.application_id.as_deref(),
            query
                .since_ms
                .unwrap_or_else(|| until.saturating_sub(30 * 86_400_000)),
            until,
        )
        .await?))
}
#[derive(Default, serde::Deserialize, utoipa::IntoParams)]
#[serde(default, deny_unknown_fields)]
#[into_params(parameter_in=Query)]
pub(super) struct DeliveryQuery {
    cursor: Option<String>,
    #[param(minimum = 1, maximum = 100)]
    limit: Option<usize>,
}
#[utoipa::path(get,path="/api/v1/notifications/deliveries",operation_id="notificationDeliveries",params(DeliveryQuery),responses((status=200,body=Envelope<Page<NotificationDelivery>>),(status=400,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn deliveries(
    State(state): State<ApiState>,
    query: Result<Query<DeliveryQuery>, QueryRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let Query(query) = query.map_err(ApiError::query)?;
    Ok(ok(state
        .notification_deliveries(query.cursor.as_deref(), query.limit.unwrap_or(50))
        .await?))
}
#[utoipa::path(post,path="/api/v1/notifications/deliveries/{id}/retry",operation_id="retryNotificationDelivery",params(("id"=String,Path)),responses((status=200,body=Envelope<bool>),(status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn retry_delivery(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    state.retry_notification(&id).await?;
    Ok(ok(true))
}

impl ApiError {
    fn query(_: QueryRejection) -> Self {
        Self::new(
            axum::http::StatusCode::BAD_REQUEST,
            "invalid_request",
            "Invalid observability query",
        )
    }
}
