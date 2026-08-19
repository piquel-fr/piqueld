//! Static dashboard assets and non-API SPA fallback handling.

use axum::{
    body::Body,
    extract::Request,
    http::{HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use std::{path::Path, path::PathBuf};
use tower::ServiceExt;
use tower_http::services::{ServeDir, ServeFile};

const SECURITY_HEADERS: [(&str, &str); 2] = [
    ("x-content-type-options", "nosniff"),
    ("referrer-policy", "no-referrer"),
];

/// Returns whether a path is reserved for API, health, or documentation errors.
pub(super) fn is_reserved_path(path: &str) -> bool {
    path.contains('%')
        || path == "/api"
        || path.starts_with("/api/")
        || path == "/health"
        || path.starts_with("/health/")
        || path == "/openapi.json"
        || path.starts_with("/openapi/")
}

/// Serves existing assets and falls back to the dashboard shell only for
/// extensionless, non-reserved browser routes.
pub(super) async fn fallback(root: PathBuf, request: Request) -> Response {
    if !matches!(*request.method(), Method::GET | Method::HEAD) {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }

    let extensionless = Path::new(request.uri().path()).extension().is_none();
    let mut response = if extensionless {
        ServeDir::new(&root)
            .fallback(ServeFile::new(root.join("index.html")))
            .oneshot(request)
            .await
            .into_response()
    } else {
        ServeDir::new(&root).oneshot(request).await.into_response()
    };
    if response.status() == StatusCode::NOT_FOUND && extensionless {
        return unavailable().into_response();
    }
    for (name, value) in SECURITY_HEADERS {
        response.headers_mut().insert(
            header::HeaderName::from_static(name),
            HeaderValue::from_static(value),
        );
    }
    response
}

fn unavailable() -> Response<Body> {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        "<!doctype html><html lang=\"en\"><meta charset=\"utf-8\"><title>piqueld dashboard unavailable</title><body><main><h1>Dashboard unavailable</h1><p>The production dashboard bundle is not installed.</p></main></body></html>",
    )
        .into_response()
}
