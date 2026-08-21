//! Optional static dashboard assets and client-side route fallback handling.

use axum::{
    body::Body,
    extract::Request,
    http::{HeaderValue, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use std::{path::Path, path::PathBuf};
use tower::ServiceExt;
use tower_http::services::{ServeDir, ServeFile};

const SECURITY_HEADERS: [(&str, &str); 2] = [
    ("x-content-type-options", "nosniff"),
    ("referrer-policy", "no-referrer"),
];

/// Describes whether the optional dashboard bundle is available to the daemon.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UiAssets {
    /// Do not register dashboard routes.
    Disabled,
    /// Serve the bundle from this directory.
    Directory(PathBuf),
}

impl UiAssets {
    /// Resolves UI availability once from configuration.
    ///
    /// A configured directory enables the UI even when it does not exist yet;
    /// anything else disables the dashboard entirely.
    #[must_use]
    pub fn resolve(configured: Option<&Path>) -> Self {
        configured.map_or(Self::Disabled, |path| Self::Directory(path.to_owned()))
    }
}

/// Returns whether a path belongs to the versioned API namespace.
pub(super) fn is_api_path(path: &str) -> bool {
    path == "/api" || path.starts_with("/api/")
}

/// Returns a permanent redirect to the canonical dashboard root.
pub(super) async fn redirect() -> impl IntoResponse {
    axum::response::Redirect::permanent("/dashboard/")
}

/// Serves files below `/dashboard/` and falls back to the dashboard shell for
/// extensionless Leptos routes.
pub(super) async fn fallback(root: PathBuf, mut request: Request) -> Response {
    if !matches!(*request.method(), Method::GET | Method::HEAD) {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }

    let index = root.join("index.html");
    if !root.is_dir() || !index.is_file() {
        return unavailable().into_response();
    }

    let original = request.uri().clone();
    let Some(relative) = original.path().strip_prefix("/dashboard") else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let relative = if relative.is_empty() { "/" } else { relative };
    let Ok(uri) = Uri::builder()
        .path_and_query(match original.query() {
            Some(query) => format!("{relative}?{query}"),
            None => relative.to_owned(),
        })
        .build()
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    *request.uri_mut() = uri;

    let extensionless = Path::new(&relative).extension().is_none();
    let mut response = if extensionless {
        ServeDir::new(&root)
            .fallback(ServeFile::new(&index))
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

pub(super) fn not_found() -> Response<Body> {
    StatusCode::NOT_FOUND.into_response()
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
