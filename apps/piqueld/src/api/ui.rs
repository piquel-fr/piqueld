//! Static dashboard assets and hardened non-API SPA fallback handling.

use axum::{
    body::Body,
    extract::Request,
    http::{HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use sha2::{Digest, Sha256};
use std::{
    io::Read,
    path::{Component, Path, PathBuf},
};

const MAX_ASSET_BYTES: u64 = 16 * 1024 * 1024;
const SECURITY_HEADERS: [(&str, &str); 4] = [
    ("referrer-policy", "no-referrer"),
    ("x-content-type-options", "nosniff"),
    ("x-frame-options", "DENY"),
    (
        "permissions-policy",
        "camera=(), microphone=(), geolocation=()",
    ),
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

/// Serves fingerprinted assets and the SPA shell without shadowing API paths.
pub(super) async fn fallback(root: PathBuf, request: Request) -> Response {
    let path = request.uri().path();
    if is_reserved_path(path) {
        return StatusCode::NOT_FOUND.into_response();
    }
    if !matches!(*request.method(), Method::GET | Method::HEAD) {
        return StatusCode::METHOD_NOT_ALLOWED.into_response();
    }
    // Do not let percent-encoding create a second interpretation of API,
    // traversal, or asset paths between the router, proxies, and filesystem.
    if path.as_bytes().contains(&b'%') {
        return StatusCode::NOT_FOUND.into_response();
    }
    let relative_path = path.trim_start_matches('/');
    let asset_request = !relative_path.is_empty() && Path::new(relative_path).extension().is_some();
    let relative = if asset_request {
        relative_path
    } else {
        "index.html"
    };
    if !safe_relative(relative) {
        return StatusCode::NOT_FOUND.into_response();
    }
    let file = root.join(relative);
    let Ok(bytes) = read_asset(root, relative.to_owned()).await else {
        return if asset_request {
            StatusCode::NOT_FOUND.into_response()
        } else {
            unavailable_shell()
        };
    };
    let Some(policy) = security_policy(relative, &bytes) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Ok(range) = requested_range(request.headers().get(header::RANGE), bytes.len()) else {
        let mut response = StatusCode::RANGE_NOT_SATISFIABLE.into_response();
        response.headers_mut().insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes */{}", bytes.len()))
                .unwrap_or_else(|_| HeaderValue::from_static("bytes */0")),
        );
        response
            .headers_mut()
            .insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
        apply_security_headers(&mut response, &policy);
        return response;
    };
    let (start, end) = range.unwrap_or((0, bytes.len().saturating_sub(1)));
    let content_length = if bytes.is_empty() { 0 } else { end - start + 1 };
    let mut response = if request.method() == Method::HEAD {
        Response::new(Body::empty())
    } else {
        Response::new(Body::from(if bytes.is_empty() {
            Vec::new()
        } else {
            bytes[start..=end].to_vec()
        }))
    };
    *response.status_mut() = if range.is_some() {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(content_type(&file)),
    );
    if let Ok(value) = HeaderValue::from_str(&content_length.to_string()) {
        response.headers_mut().insert(header::CONTENT_LENGTH, value);
    }
    response
        .headers_mut()
        .insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    if range.is_some()
        && let Ok(value) = HeaderValue::from_str(&format!("bytes {start}-{end}/{}", bytes.len()))
    {
        response.headers_mut().insert(header::CONTENT_RANGE, value);
    }
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static(if relative == "index.html" || !fingerprinted(relative) {
            "no-cache"
        } else {
            "public, max-age=31536000, immutable"
        }),
    );
    apply_security_headers(&mut response, &policy);
    response
}

fn fingerprinted(relative: &str) -> bool {
    let Some(stem) = Path::new(relative)
        .file_stem()
        .and_then(|value| value.to_str())
    else {
        return false;
    };
    let stem = stem.strip_suffix("_bg").unwrap_or(stem);
    let Some((_, digest)) = stem.rsplit_once('-') else {
        return false;
    };
    digest.len() >= 16 && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn requested_range(value: Option<&HeaderValue>, len: usize) -> Result<Option<(usize, usize)>, ()> {
    let Some(value) = value else { return Ok(None) };
    let value = value.to_str().map_err(|_| ())?;
    let spec = value.strip_prefix("bytes=").ok_or(())?;
    if spec.contains(',') || len == 0 {
        return Err(());
    }
    let (start, end) = spec.split_once('-').ok_or(())?;
    if start.is_empty() {
        let suffix = end.parse::<usize>().map_err(|_| ())?;
        if suffix == 0 {
            return Err(());
        }
        let start = len.saturating_sub(suffix);
        return Ok(Some((start, len - 1)));
    }
    let start = start.parse::<usize>().map_err(|_| ())?;
    if start >= len {
        return Err(());
    }
    let end = if end.is_empty() {
        len - 1
    } else {
        end.parse::<usize>().map_err(|_| ())?.min(len - 1)
    };
    (start <= end).then_some(Some((start, end))).ok_or(())
}

fn apply_security_headers(response: &mut Response, policy: &str) {
    if let Ok(value) = HeaderValue::from_str(policy) {
        response
            .headers_mut()
            .insert(header::CONTENT_SECURITY_POLICY, value);
    }
    for (name, value) in SECURITY_HEADERS {
        response.headers_mut().insert(
            header::HeaderName::from_static(name),
            HeaderValue::from_static(value),
        );
    }
}

fn security_policy(relative: &str, bytes: &[u8]) -> Option<String> {
    let mut hashes = Vec::new();
    if relative == "index.html" {
        let html = std::str::from_utf8(bytes).ok()?;
        let mut remaining = html;
        while let Some(start) = remaining.find("<script") {
            remaining = &remaining[start..];
            let body_start = remaining.find('>')? + 1;
            let body = &remaining[body_start..];
            let body_end = body.find("</script>")?;
            hashes.push(format!(
                "'sha256-{}'",
                STANDARD.encode(Sha256::digest(&body.as_bytes()[..body_end]))
            ));
            remaining = &body[body_end + "</script>".len()..];
        }
    }
    let script_hashes = if hashes.is_empty() {
        String::new()
    } else {
        format!(" {}", hashes.join(" "))
    };
    Some(format!(
        "default-src 'self'; connect-src 'self'; img-src 'self' data:; style-src 'self'; script-src 'self' 'wasm-unsafe-eval'{script_hashes}; object-src 'none'; base-uri 'none'; frame-ancestors 'none'; form-action 'self'"
    ))
}

async fn read_asset(root: PathBuf, relative: String) -> Result<Vec<u8>, ()> {
    tokio::task::spawn_blocking(move || {
        let root = rustix::fs::open(
            &root,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::CLOEXEC
                | rustix::fs::OFlags::DIRECTORY
                | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
        )
        .map_err(|_| ())?;
        let descriptor = rustix::fs::openat2(
            &root,
            relative,
            rustix::fs::OFlags::RDONLY | rustix::fs::OFlags::CLOEXEC | rustix::fs::OFlags::NOFOLLOW,
            rustix::fs::Mode::empty(),
            rustix::fs::ResolveFlags::BENEATH
                | rustix::fs::ResolveFlags::NO_SYMLINKS
                | rustix::fs::ResolveFlags::NO_MAGICLINKS,
        )
        .map_err(|_| ())?;
        let mut file = std::fs::File::from(descriptor);
        let before = file.metadata().map_err(|_| ())?;
        if !before.is_file() || before.len() > MAX_ASSET_BYTES {
            return Err(());
        }
        let mut bytes = Vec::with_capacity(usize::try_from(before.len()).map_err(|_| ())?);
        file.by_ref()
            .take(MAX_ASSET_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| ())?;
        let after = file.metadata().map_err(|_| ())?;
        if bytes.len() as u64 != before.len()
            || before.len() != after.len()
            || before.modified().ok() != after.modified().ok()
        {
            return Err(());
        }
        Ok(bytes)
    })
    .await
    .map_err(|_| ())?
}

fn safe_relative(value: &str) -> bool {
    !value.is_empty()
        && Path::new(value)
            .components()
            .all(|part| matches!(part, Component::Normal(_)))
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|value| value.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("wasm") => "application/wasm",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        _ => "application/octet-stream",
    }
}

fn unavailable_shell() -> Response {
    let body = "<!doctype html><html lang=en><meta charset=utf-8><title>piqueld UI unavailable</title><body><main><h1>UI bundle unavailable</h1><p>Install the piqueld-ui production bundle in the configured server.ui_dir.</p></main></body></html>";
    let mut response = (
        StatusCode::SERVICE_UNAVAILABLE,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
        .into_response();
    if let Some(policy) = security_policy("index.html", body.as_bytes()) {
        apply_security_headers(&mut response, &policy);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_traversal_and_accepts_versioned_assets() {
        assert!(!safe_relative("../secret"));
        assert!(!safe_relative("assets/../secret"));
        assert!(safe_relative("assets/piqueld-ui-0123456789abcdef.wasm"));
        assert!(fingerprinted("assets/piqueld-ui-0123456789abcdef_bg.wasm"));
    }

    #[test]
    fn has_strict_secret_safe_browser_policy() {
        let csp = security_policy(
            "index.html",
            b"<script type=module>import init from '/ui.js';</script>",
        )
        .unwrap();
        assert!(!csp.contains("unsafe-inline"));
        assert!(!csp.contains(" 'unsafe-eval'"));
        assert!(csp.contains("connect-src 'self'"));
        assert!(csp.contains("'wasm-unsafe-eval'"));
        assert!(csp.contains("'sha256-"));
    }

    #[tokio::test]
    async fn descriptor_relative_reader_rejects_symlinks_and_bounds_files() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("index.html"), b"safe shell").unwrap();
        assert_eq!(
            read_asset(root.path().to_owned(), "index.html".into())
                .await
                .unwrap(),
            b"safe shell"
        );

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("index.html", root.path().join("alias.html")).unwrap();
            assert!(
                read_asset(root.path().to_owned(), "alias.html".into())
                    .await
                    .is_err()
            );
        }
        assert!(
            read_asset(root.path().join("missing"), "index.html".into())
                .await
                .is_err()
        );
    }

    #[test]
    fn missing_bundle_response_is_explicit_uncached_and_hardened() {
        let response = unavailable_shell();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        assert!(
            response
                .headers()
                .contains_key(header::CONTENT_SECURITY_POLICY)
        );
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
    }
}
