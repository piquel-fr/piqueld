use axum::{extract::State, response::IntoResponse};
use piqueld_client::{Envelope, SystemStatus};

use super::{ApiState, ok, openapi::ApiErrorResponse};

/// Provides the daemon's current status and API version.
///
/// # Returns
///
/// A response containing the running status, API version, and instance identifier.
///
/// # Examples
///
/// ```no_run
/// let response = status(State(ApiState {
///     instance_id: "example".into(),
/// })).await;
/// ```
///
#[utoipa::path(
get,
path = "/api/v1/system/status",
operation_id = "systemStatus",
summary = "Get daemon status",
responses(
(status = 200, description = "Success", body = Envelope<SystemStatus>),
(status = 500, response = inline(ApiErrorResponse)),
(status = 503, response = inline(ApiErrorResponse)),
)
)]
pub(super) async fn status(State(state): State<ApiState>) -> impl IntoResponse {
    ok(SystemStatus {
        status: "running".into(),
        api_version: "v1".into(),
        instance_id: state.instance_id,
    })
}
