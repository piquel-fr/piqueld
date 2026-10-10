//! GitHub push webhooks: their payload URL and secret in the API, and the
//! webhook listener receiving deliveries.
//!
//! The listener is its own router, served on a Unix socket only the gateway
//! reaches, with nothing but `POST /hooks/github/{id}`. It shares no
//! middleware with the API: deliveries carry no credentials, only GitHub's
//! signature, and every other path is a 404.
use super::{ApiError, ApiPath, ApiState, ok, openapi::ApiErrorResponse};
use crate::api::{Delivery, WEBHOOK_BODY_LIMIT, WEBHOOK_PATH, WebhookError};
use crate::auth::Identity;
use axum::{
    Extension, Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::post,
};
use piqueld_core::ApplicationId;
use piqueld_core::api::Envelope;
use piqueld_core::sync::{WebhookSecret, WebhookView};
use std::{sync::Arc, time::Duration};
use tokio::sync::Semaphore;

/// Deliveries handled at once; more get 503, which GitHub reports.
const CONCURRENCY: usize = 16;
/// Longest a delivery may take, including uploading its body.
const TIMEOUT: Duration = Duration::from_secs(10);

/// Builds the webhook router: bodies above `WEBHOOK_BODY_LIMIT` get 413, at
/// most `CONCURRENCY` deliveries run at once, and each gets `TIMEOUT`.
pub fn webhook_router(state: ApiState) -> Router {
    let slots = Arc::new(Semaphore::new(CONCURRENCY));
    Router::new()
        .route(&format!("{WEBHOOK_PATH}{{id}}"), post(receive))
        .layer(DefaultBodyLimit::max(WEBHOOK_BODY_LIMIT))
        .layer(middleware::from_fn(move |request: Request, next: Next| {
            let slots = Arc::clone(&slots);
            async move {
                let Ok(_slot) = slots.try_acquire() else {
                    return StatusCode::SERVICE_UNAVAILABLE.into_response();
                };
                tokio::time::timeout(TIMEOUT, next.run(request))
                    .await
                    .unwrap_or_else(|_| StatusCode::REQUEST_TIMEOUT.into_response())
            }
        }))
        .with_state(state)
}

/// Receives a GitHub delivery for application `id`: 204 once its signature
/// verifies, whatever its event, and 401 otherwise, including for unknown
/// applications.
async fn receive(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let Ok(application) = ApplicationId::parse(id) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let header = |name| headers.get(name).and_then(|value| value.to_str().ok());
    let delivery = state
        .receive_webhook(
            &application,
            header("x-hub-signature-256"),
            header("x-github-event"),
            &body,
        )
        .await;
    match delivery {
        Ok(Delivery::Push) => {
            tracing::info!(%application, "webhook reported a push");
            StatusCode::NO_CONTENT
        }
        Ok(Delivery::Ignored) => StatusCode::NO_CONTENT,
        Err(WebhookError::Unauthorized) => {
            tracing::warn!(%application, "webhook delivery refused: its signature does not verify");
            StatusCode::UNAUTHORIZED
        }
        Err(WebhookError::Unavailable(error)) => {
            tracing::error!(%application, ?error, "webhook delivery could not be verified");
            StatusCode::SERVICE_UNAVAILABLE
        }
    }
    .into_response()
}

/// Gets an application's webhook payload URL.
///
/// The URL to configure in GitHub, absent until the daemon serves
/// `ingress.webhook_hostname`, and when the current secret was generated.
/// The secret itself is only shown when generated.
#[utoipa::path(get,path="/api/v1/applications/{id}/webhook",operation_id="getWebhook",
    params(("id"=String,Path)),
    responses((status=200,description="Success",body=Envelope<WebhookView>),
    (status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),
    (status=500,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn get(
    State(state): State<ApiState>,
    ApiPath(id): ApiPath<String>,
) -> Result<impl IntoResponse, ApiError> {
    Ok(ok(state.webhook(&ApplicationId::parse(id)?).await?))
}

/// Generates an application's webhook secret.
///
/// Replaces the previous secret, which stops verifying at once, and returns
/// the new one. It is never shown again: configure it in GitHub as the
/// webhook's secret, with content type `application/json`.
#[utoipa::path(post,path="/api/v1/applications/{id}/webhook/secret",operation_id="generateWebhookSecret",
    params(("id"=String,Path)),
    responses((status=200,description="Secret generated",body=Envelope<WebhookSecret>),
    (status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),
    (status=409,response=inline(ApiErrorResponse)),(status=500,response=inline(ApiErrorResponse)),
    (status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn generate_secret(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    ApiPath(id): ApiPath<String>,
) -> Result<impl IntoResponse, ApiError> {
    let application = ApplicationId::parse(id)?;
    Ok(ok(state
        .generate_webhook_secret(identity.actor(), &application)
        .await?))
}
