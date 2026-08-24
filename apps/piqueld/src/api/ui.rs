//! Embedded dashboard assets and client-side route fallback handling.

use axum::{
    body::Body,
    extract::Request,
    http::{Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use std::path::Path;

/// One embedded dashboard file: its bundle name relative to `/dashboard/`,
/// plus its bytes.
pub type EmbeddedFile = (&'static str, &'static [u8]);

/// The complete set of dashboard files compiled into the daemon binary.
pub type EmbeddedBundle = [EmbeddedFile];

/// Headers applied to every dashboard response, including redirects.
///
/// The Content-Security-Policy is written for Trunk's release bundle: its
/// generated shell carries one inline module loader (`'unsafe-inline'`) and
/// instantiates WebAssembly (`'wasm-unsafe-eval'`); everything else stays
/// same-origin.
const SECURITY_HEADERS: [(&str, &str); 3] = [
    ("x-content-type-options", "nosniff"),
    ("referrer-policy", "no-referrer"),
    (
        "content-security-policy",
        "default-src 'self'; \
         script-src 'self' 'unsafe-inline' 'wasm-unsafe-eval'; \
         style-src 'self'; img-src 'self' data:; font-src 'self'; \
         connect-src 'self'; object-src 'none'; base-uri 'none'; \
         form-action 'none'; frame-ancestors 'none'",
    ),
];

/// Inserts the shared security headers without clobbering existing values.
///
/// Invariant: applied exactly once per dashboard response, at the serving
/// boundary ([`serve`]'s single exit, [`redirect`], [`not_found`]); response
/// constructors stay raw so no path can double-apply or skip the headers.
fn harden(mut response: Response) -> Response {
    let headers = response.headers_mut();
    for (name, value) in SECURITY_HEADERS {
        if !headers.contains_key(name)
            && let Ok(name) = header::HeaderName::from_lowercase(name.as_bytes())
            && let Ok(header_value) = header::HeaderValue::from_str(value)
        {
            headers.insert(name, header_value);
        }
    }
    response
}

/// Describes whether the dashboard is available to the daemon.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UiAssets {
    /// The binary was built without `embedded-ui`; no dashboard routes are
    /// registered and the process serves the API only.
    Disabled,
    /// Serve this compile-time bundle below `/dashboard/`.
    Embedded(&'static EmbeddedBundle),
}

impl UiAssets {
    /// Resolves dashboard availability for the current binary.
    ///
    /// The dashboard exists exactly when the daemon was built with the
    /// `embedded-ui` feature; there is no runtime configuration anymore.
    #[must_use]
    pub fn resolve() -> Self {
        #[cfg(feature = "embedded-ui")]
        {
            Self::Embedded(crate::ui_bundle::BUNDLE)
        }

        #[cfg(not(feature = "embedded-ui"))]
        {
            Self::Disabled
        }
    }
}

/// Returns whether a path belongs to the versioned API namespace.
pub(super) fn is_api_path(path: &str) -> bool {
    path == "/api" || path.starts_with("/api/")
}

/// Returns a permanent redirect to the canonical dashboard root.
pub(super) async fn redirect() -> impl IntoResponse {
    harden(axum::response::Redirect::permanent("/dashboard/").into_response())
}

/// Serves embedded bundle files below `/dashboard/` and falls back to the
/// dashboard shell for extensionless Leptos routes.
///
/// Every lookup is an exact match against the compile-time bundle, so request
/// paths can never traverse anywhere outside it.
pub(super) fn serve(bundle: &'static EmbeddedBundle, request: &Request) -> Response {
    let response = if !matches!(*request.method(), Method::GET | Method::HEAD) {
        StatusCode::METHOD_NOT_ALLOWED.into_response()
    } else if let Some(relative) = request.uri().path().strip_prefix("/dashboard/") {
        match lookup(bundle, relative) {
            Some((name, body)) => asset_response(name, body),
            None if Path::new(relative).extension().is_none() => shell_response(bundle),
            None => StatusCode::NOT_FOUND.into_response(),
        }
    } else {
        return not_found();
    };
    harden(response)
}

pub(super) fn not_found() -> Response<Body> {
    harden(StatusCode::NOT_FOUND.into_response())
}

fn lookup(bundle: &'static EmbeddedBundle, name: &str) -> Option<EmbeddedFile> {
    bundle
        .iter()
        .copied()
        .find(|(candidate, _)| *candidate == name)
}

fn asset_response(name: &str, body: &'static [u8]) -> Response {
    let mut response = Response::new(Body::from(body));
    let headers = response.headers_mut();
    // Trunk content-hashes asset filenames, so those entries may be cached
    // forever; everything else must revalidate because a new daemon binary
    // can replace the bytes under an unchanged URL.
    headers.insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static(if is_content_hashed(name) {
            "public, max-age=31536000, immutable"
        } else {
            "no-cache"
        }),
    );
    headers.insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static(content_type(name)),
    );
    response
}

/// Serves the dashboard shell for extensionless client-side routes such as
/// `/dashboard/applications/notes`.
fn shell_response(bundle: &'static EmbeddedBundle) -> Response {
    match lookup(bundle, "index.html") {
        Some((_, body)) => {
            let mut response = Response::new(Body::from(body));
            let headers = response.headers_mut();
            // The shell references hashed assets by URL, so cached copies of
            // the shell itself must never outlive the daemon that served it.
            headers.insert(
                header::CACHE_CONTROL,
                header::HeaderValue::from_static("no-store"),
            );
            headers.insert(
                header::CONTENT_TYPE,
                header::HeaderValue::from_static("text/html; charset=utf-8"),
            );
            response
        }
        None => not_found(),
    }
}

/// Maps a bundle filename to a conservative static `Content-Type`.
fn content_type(name: &str) -> &'static str {
    let extension = name.rsplit_once('.').map_or("", |(_, tail)| tail);
    match extension {
        "css" => "text/css; charset=utf-8",
        "gif" => "image/gif",
        "htm" | "html" => "text/html; charset=utf-8",
        "ico" => "image/x-icon",
        "jpeg" | "jpg" => "image/jpeg",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" | "map" => "application/json",
        "otf" => "font/otf",
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "ttf" => "font/ttf",
        "txt" => "text/plain; charset=utf-8",
        "wasm" => "application/wasm",
        "webp" => "image/webp",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

/// Returns whether Trunk content-hashed this filename, making it immutable.
///
/// Trunk inserts its digest as the last `-` separated segment of the stem;
/// wasm-bindgen tooling may append further underscore suffixes such as `_bg`
/// behind that digest. The minimum length keeps ordinary short words like
/// `added.css` or `cafe.js` from being mistaken for digests.
fn is_content_hashed(name: &str) -> bool {
    let stem = name.rsplit_once('.').map_or(name, |(stem, _)| stem);
    let digest = stem.rsplit('-').next().unwrap_or(stem);
    let digest = digest.split('_').next().unwrap_or(digest);
    digest.len() >= 8
        && digest.len() <= 64
        && digest
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}
