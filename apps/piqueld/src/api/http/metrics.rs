//! Metrics-only listener, optionally protected by a static bearer token.

use axum::{
    Router,
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use sha2::{Digest, Sha256};
use std::path::Path;

use super::ApiState;

/// Bearer token that scrapers must present, loaded from `metrics.token_file`.
///
/// Only its SHA-256 digest is kept; comparing fixed-size digests avoids
/// leaking the token through comparison timing.
#[derive(Clone)]
pub struct MetricsToken([u8; 32]);

impl MetricsToken {
    /// Reads the token from `path`, ignoring surrounding whitespace.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the file is unreadable or holds no token.
    pub fn read(path: &Path) -> std::io::Result<Self> {
        let contents = std::fs::read_to_string(path)?;
        let token = contents.trim();
        if token.is_empty() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "metrics token file is empty",
            ));
        }
        Ok(Self(Self::digest(token)))
    }

    fn digest(token: &str) -> [u8; 32] {
        Sha256::digest(token.as_bytes()).into()
    }

    fn authorizes(&self, headers: &HeaderMap) -> bool {
        headers
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
            .is_some_and(|token| Self::digest(token) == self.0)
    }
}

/// Builds an isolated metrics-only router; no administrative routes are installed.
/// With a token, `GET /metrics` requires `Authorization: Bearer <token>`.
pub fn metrics_router(state: ApiState, token: Option<MetricsToken>) -> Router {
    Router::new()
        .route(
            "/metrics",
            get(
                move |State(state): State<ApiState>, headers: HeaderMap| async move {
                    if token
                        .as_ref()
                        .is_some_and(|token| !token.authorizes(&headers))
                    {
                        return (
                            StatusCode::UNAUTHORIZED,
                            [(header::WWW_AUTHENTICATE, "Bearer")],
                            "Unauthorized\n",
                        )
                            .into_response();
                    }
                    metrics_response(&state).await
                },
            ),
        )
        .with_state(state)
}

async fn metrics_response(state: &ApiState) -> Response {
    match state.prometheus_metrics().await {
        Ok(body) => (
            StatusCode::OK,
            [(
                header::CONTENT_TYPE,
                "text/plain; version=0.0.4; charset=utf-8",
            )],
            body,
        )
            .into_response(),
        Err(_) => (
            StatusCode::SERVICE_UNAVAILABLE,
            [(header::CONTENT_TYPE, "text/plain")],
            "Metrics collection unavailable\n",
        )
            .into_response(),
    }
}
