use super::{ApiError, ApiPath, ApiState, ok, openapi::ApiErrorResponse};
use crate::auth::Identity;
use axum::{
    Extension,
    extract::{Query, State, rejection::QueryRejection},
    response::IntoResponse,
};
use piqueld_core::{
    Event,
    access::{AppPermission, Denied, GlobalPermission, Permission},
    api::{Envelope, Page},
    audit::{AuditEvent, AuditFilter, AuditOutcome, AuditVerification},
    observability::{DaemonStats, DeploymentAnalytics, NotificationDelivery},
};

/// Gets a recorded diagnostic.
///
/// Server errors expose this ID as `details.diagnostic_id`; the response is the
/// event that recorded the failure, with its context.
#[utoipa::path(get,path="/api/v1/diagnostics/{id}",operation_id="getDiagnostic",params(("id"=String,Path)),responses((status=200,body=Envelope<Event>),(status=404,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn diagnostic(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    ApiPath(id): ApiPath<String>,
) -> Result<impl IntoResponse, ApiError> {
    let event = state.diagnostic(&id).await?;
    // Application diagnostics need `events:read` there, like other history of
    // a readable application. Daemon diagnostics need `system:read`; without
    // it they do not exist for the caller.
    match (&event.scope, &event.application_id) {
        (piqueld_core::observability::EventScope::Application, Some(application)) => identity
            .grants
            .require_app(AppPermission::EventsRead, application)?,
        _ if identity.grants.has_global(GlobalPermission::SystemRead) => {}
        _ => return Err(Denied::Hidden.into()),
    }
    Ok(ok(event))
}
/// Gets daemon resource usage.
///
/// Reports process, database, disk, retained history, and queue statistics.
/// Measurements the host cannot provide are omitted rather than reported as zero.
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
    /// Only include deployments of this environment.
    environment_id: Option<String>,
    /// Inclusive Unix millisecond lower bound; defaults to 30 days before `until_ms`.
    since_ms: Option<i64>,
    /// Inclusive Unix millisecond upper bound; defaults to now.
    until_ms: Option<i64>,
}
/// Gets deployment analytics for a time window.
///
/// Aggregates deployment outcomes; the window defaults to the last 30 days.
#[utoipa::path(get,path="/api/v1/analytics/deployments",operation_id="deploymentAnalytics",params(AnalyticsQuery),responses((status=200,body=Envelope<DeploymentAnalytics>),(status=400,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn analytics(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    query: Result<Query<AnalyticsQuery>, QueryRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let Query(query) = query.map_err(ApiError::query)?;
    let until = query.until_ms.unwrap_or_else(crate::store::now_ms);
    Ok(ok(state
        .deployment_analytics(
            query.environment_id.as_deref(),
            &super::access::history(&identity)?,
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
    /// `next_cursor` from a previous page.
    cursor: Option<String>,
    /// Page size; defaults to 50.
    #[param(minimum = 1, maximum = 100)]
    limit: Option<usize>,
}
/// Lists webhook notification deliveries.
///
/// Follow `next_cursor` to load more deliveries.
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
/// Retries a webhook notification delivery.
///
/// Schedules another attempt using the current destination configuration.
#[utoipa::path(post,path="/api/v1/notifications/deliveries/{id}/retry",operation_id="retryNotificationDelivery",params(("id"=String,Path)),responses((status=200,body=Envelope<bool>),(status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn retry_delivery(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    ApiPath(id): ApiPath<String>,
) -> Result<impl IntoResponse, ApiError> {
    state.retry_notification(identity.actor(), &id).await?;
    Ok(ok(true))
}
#[derive(Default, serde::Deserialize, utoipa::IntoParams)]
#[serde(default, deny_unknown_fields)]
#[into_params(parameter_in=Query)]
pub(super) struct AuditQuery {
    /// Only this account's requests; callers without `audit:read` may only
    /// name their own account, which is also the default for them.
    user_id: Option<String>,
    /// Only requests by accounts that had this username when making them,
    /// e.g. a deleted account's.
    username: Option<String>,
    /// Only requests made with this credential; scoped credentials without
    /// `audit:read` may only name themselves, which is also their default.
    credential_id: Option<String>,
    /// Only requests with this outcome.
    outcome: Option<AuditOutcome>,
    /// `next_cursor` from a previous page.
    cursor: Option<String>,
    /// Page size; defaults to 50.
    #[param(minimum = 1, maximum = 100)]
    limit: Option<usize>,
}
/// Lists audited API requests, newest first.
///
/// Refused requests, writes, and sensitive reads are recorded with their
/// account, credential, address, and outcome. With `audit:read`, every
/// account's trail is visible; otherwise only the caller's own account's, or
/// only its own requests for a scoped credential like an API token, which
/// must not see what its account does beyond the token's grants. Follow
/// `next_cursor` to load older requests.
#[utoipa::path(get,path="/api/v1/audit",operation_id="listAudit",params(AuditQuery),responses((status=200,body=Envelope<Page<AuditEvent>>),(status=400,response=inline(ApiErrorResponse)),(status=403,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn audit(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    query: Result<Query<AuditQuery>, QueryRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let Query(query) = query.map_err(ApiError::query)?;
    let mut filter = AuditFilter {
        user_id: query.user_id,
        username: query.username,
        credential_id: query.credential_id,
        outcome: query.outcome,
    };
    if !identity.grants.has_global(GlobalPermission::AuditRead) {
        // Only accounts lack `audit:read`; the host operator holds `admin`.
        // Scoped credentials, like API tokens, see only their own requests.
        let own = identity.user().map(|user| user.id.clone());
        let credential = identity
            .scoped
            .then(|| identity.credential_id().map(str::to_owned))
            .flatten();
        let other = |requested: &Option<String>, own: &Option<String>| {
            requested.is_some() && requested != own
        };
        if other(&filter.user_id, &own)
            || credential.is_some() && other(&filter.credential_id, &credential)
        {
            return Err(Denied::Missing(Permission::Global(GlobalPermission::AuditRead)).into());
        }
        filter.user_id = own;
        if credential.is_some() {
            filter.credential_id = credential;
        }
    }
    Ok(ok(state
        .audit_events(&filter, query.cursor.as_deref(), query.limit.unwrap_or(50))
        .await?))
}
/// Verifies the audit trail's hash chain.
///
/// Recomputes every retained record's link. `broken_at` names the first
/// record that was edited, inserted, or follows a removed one. Removing the
/// newest records leaves a valid chain, so compare `head` with a value kept
/// elsewhere.
#[utoipa::path(get,path="/api/v1/audit/verify",operation_id="verifyAudit",responses((status=200,body=Envelope<AuditVerification>),(status=403,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn verify_audit(
    State(state): State<ApiState>,
) -> Result<impl IntoResponse, ApiError> {
    Ok(ok(state.verify_audit().await?))
}

impl ApiError {
    /// Maps a rejected observability query string to a generic 400.
    fn query(_: QueryRejection) -> Self {
        Self::new(
            axum::http::StatusCode::BAD_REQUEST,
            "invalid_request",
            "Invalid observability query",
        )
    }
}
