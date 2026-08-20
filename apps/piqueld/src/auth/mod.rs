//! Administrative authentication and HTTP resource-policy boundary.

use crate::config::{CredentialReference, SecurityConfig};
use axum::{
    body::Body,
    extract::{Request, State},
    http::{HeaderValue, Method, StatusCode, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use http_body_util::BodyExt as _;
use rustix::fs::{Mode, OFlags, ResolveFlags};
use serde_json::json;
use std::{
    fs::File,
    io::Read,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use subtle::ConstantTimeEq;
use tokio::sync::Semaphore;
use zeroize::Zeroizing;

const TAILSCALE_LOGIN: &str = "tailscale-user-login";
const TAILSCALE_IDENTITY_HEADERS: [&str; 3] = [
    TAILSCALE_LOGIN,
    "tailscale-user-name",
    "tailscale-user-profile-pic",
];
const MAX_BEARER_BYTES: usize = 4096;

struct BearerSecret {
    padded: Zeroizing<[u8; MAX_BEARER_BYTES]>,
    len: usize,
}

impl BearerSecret {
    fn new(value: &Zeroizing<Vec<u8>>) -> Option<Self> {
        if value.is_empty() || value.len() > MAX_BEARER_BYTES {
            return None;
        }
        let mut padded = Zeroizing::new([0; MAX_BEARER_BYTES]);
        padded[..value.len()].copy_from_slice(value);
        Some(Self {
            padded,
            len: value.len(),
        })
    }

    fn matches(&self, provided: &[u8]) -> bool {
        if provided.len() > MAX_BEARER_BYTES {
            return false;
        }
        let mut padded = Zeroizing::new([0; MAX_BEARER_BYTES]);
        padded[..provided.len()].copy_from_slice(provided);
        let contents = self.padded.as_slice().ct_eq(padded.as_slice());
        let length = self.len.ct_eq(&provided.len());
        bool::from(contents & length)
    }
}

/// Authentication policy for one listener. Unix sockets rely on filesystem
/// ownership; TCP listeners require an explicit bearer or trusted proxy mode.
#[derive(Clone)]
pub struct AccessPolicy {
    local_unix: bool,
    bearer: Option<Arc<BearerSecret>>,
    trust_tailscale: bool,
    allowed_origins: Arc<[String]>,
    max_header_bytes: usize,
    max_headers: usize,
    timeout: Duration,
    concurrency: Arc<Semaphore>,
}

impl AccessPolicy {
    /// Creates a filesystem-authorized Unix listener policy.
    #[must_use]
    pub fn unix(config: &SecurityConfig) -> Self {
        Self::new(
            true,
            None,
            config,
            Arc::new(Semaphore::new(config.max_concurrent_requests)),
        )
    }

    /// Creates a fail-closed loopback TCP policy.
    #[must_use]
    pub fn tcp(token: Option<&Zeroizing<Vec<u8>>>, config: &SecurityConfig) -> Self {
        Self::new(
            false,
            token.and_then(BearerSecret::new).map(Arc::new),
            config,
            Arc::new(Semaphore::new(config.max_concurrent_requests)),
        )
    }

    /// Creates both listener policies with one process-wide concurrency budget.
    #[must_use]
    pub fn listener_pair(
        token: Option<&Zeroizing<Vec<u8>>>,
        config: &SecurityConfig,
    ) -> (Self, Self) {
        let concurrency = Arc::new(Semaphore::new(config.max_concurrent_requests));
        let bearer = token.and_then(BearerSecret::new).map(Arc::new);
        (
            Self::new(false, bearer, config, Arc::clone(&concurrency)),
            Self::new(true, None, config, concurrency),
        )
    }

    fn new(
        local_unix: bool,
        bearer: Option<Arc<BearerSecret>>,
        config: &SecurityConfig,
        concurrency: Arc<Semaphore>,
    ) -> Self {
        Self {
            local_unix,
            bearer,
            trust_tailscale: config.trust_tailscale_headers && config.trusted_loopback_proxy,
            allowed_origins: config.allowed_origins.clone().into(),
            max_header_bytes: config.max_header_bytes,
            max_headers: config.max_headers,
            timeout: Duration::from_secs(config.request_timeout_seconds),
            concurrency,
        }
    }
}

/// Reads a bearer credential without following links or accepting an unsafe
/// regular file. The returned bytes are zeroized on drop.
///
/// # Errors
///
/// Returns an I/O error when the credential cannot be opened, read, or fails
/// the protection checks required for a private bearer-token file.
pub fn load_bearer(reference: &CredentialReference) -> Result<Zeroizing<Vec<u8>>, std::io::Error> {
    let path = credential_path(reference)?;
    if path.starts_with("/nix/store") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "unsafe credential path",
        ));
    }
    let descriptor = rustix::fs::openat2(
        rustix::fs::CWD,
        &path,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
        ResolveFlags::NO_SYMLINKS | ResolveFlags::NO_MAGICLINKS,
    )
    .or_else(|error| {
        if error == rustix::io::Errno::NOSYS
            && matches!(reference, CredentialReference::SystemdCredential { .. })
        {
            rustix::fs::open(
                &path,
                OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
                Mode::empty(),
            )
        } else {
            Err(error)
        }
    })
    .map_err(std::io::Error::from)?;
    let mut file = File::from(descriptor);
    let metadata = file.metadata()?;
    let protected_file = metadata.permissions().mode().trailing_zeros() >= 6
        && (metadata.uid() == rustix::process::geteuid().as_raw() || metadata.uid() == 0);
    if !metadata.is_file()
        || (matches!(reference, CredentialReference::File { .. }) && !protected_file)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "unsafe credential permissions",
        ));
    }
    if std::fs::canonicalize(&path)?.starts_with("/nix/store") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "unsafe credential path",
        ));
    }
    let mut value = Zeroizing::new(Vec::with_capacity(MAX_BEARER_BYTES + 3));
    file.by_ref()
        .take((MAX_BEARER_BYTES + 3) as u64)
        .read_to_end(&mut value)?;
    if value.ends_with(b"\r\n") {
        let length = value.len() - 2;
        value.truncate(length);
    } else if value
        .last()
        .is_some_and(|byte| matches!(byte, b'\n' | b'\r'))
    {
        value.pop();
    }
    if value.is_empty() || value.len() > MAX_BEARER_BYTES || value.iter().any(u8::is_ascii_control)
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "invalid bearer credential",
        ));
    }
    Ok(value)
}

fn credential_path(reference: &CredentialReference) -> Result<PathBuf, std::io::Error> {
    match reference {
        CredentialReference::File { path } => Ok(path.clone()),
        CredentialReference::SystemdCredential { name } => {
            let directory = std::env::var_os("CREDENTIALS_DIRECTORY").ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "credential directory unavailable",
                )
            })?;
            Ok(PathBuf::from(directory).join(name))
        }
    }
}

/// Applies authentication, exact-origin policy, header limits, timeouts, and
/// process-wide backpressure before a request reaches the API or UI handlers.
pub async fn enforce(
    State(policy): State<AccessPolicy>,
    mut request: Request,
    next: Next,
) -> Response {
    let header_bytes = request.headers().iter().fold(0usize, |sum, (name, value)| {
        sum.saturating_add(name.as_str().len())
            .saturating_add(value.as_bytes().len())
    });
    if request.headers().len() > policy.max_headers || header_bytes > policy.max_header_bytes {
        return error(
            StatusCode::REQUEST_HEADER_FIELDS_TOO_LARGE,
            "request_headers_too_large",
            "request headers exceed a safety limit",
        );
    }

    let origin = match allowed_origin(&request, &policy) {
        OriginPolicy::Absent => None,
        OriginPolicy::Allowed(origin) => Some(origin),
        OriginPolicy::Forbidden => {
            return error(
                StatusCode::FORBIDDEN,
                "origin_forbidden",
                "request origin is not allowed",
            );
        }
    };
    if request.method() == Method::OPTIONS && origin.is_some() {
        let mut response = StatusCode::NO_CONTENT.into_response();
        add_cors(&mut response, origin.as_deref());
        return response;
    }
    if !policy.local_unix && !tcp_authenticated(&request, &policy) {
        return cors_error(
            StatusCode::UNAUTHORIZED,
            "authentication_required",
            "valid administrative credentials are required",
            origin.as_deref(),
        );
    }

    request.headers_mut().remove(header::AUTHORIZATION);
    for name in TAILSCALE_IDENTITY_HEADERS {
        request.headers_mut().remove(name);
    }

    let Ok(permit) = policy.concurrency.clone().try_acquire_owned() else {
        return cors_error(
            StatusCode::SERVICE_UNAVAILABLE,
            "request_capacity_exhausted",
            "request capacity is temporarily exhausted",
            origin.as_deref(),
        );
    };
    let mut response = match tokio::time::timeout(policy.timeout, next.run(request)).await {
        Ok(response) => response,
        Err(_) => cors_error(
            StatusCode::GATEWAY_TIMEOUT,
            "request_timeout",
            "request exceeded its execution deadline",
            origin.as_deref(),
        ),
    };
    add_cors(&mut response, origin.as_deref());
    let (parts, body) = response.into_parts();
    let body = body.map_frame(move |frame| {
        let _hold = &permit;
        frame
    });
    Response::from_parts(parts, Body::new(body))
}

enum OriginPolicy {
    Absent,
    Allowed(String),
    Forbidden,
}

fn allowed_origin(request: &Request, policy: &AccessPolicy) -> OriginPolicy {
    let mut origins = request.headers().get_all(header::ORIGIN).iter();
    let origin = match (origins.next(), origins.next()) {
        (None, None) => return OriginPolicy::Absent,
        (Some(value), None) => value.to_str().ok(),
        _ => None,
    };
    origin
        .filter(|origin| {
            policy
                .allowed_origins
                .iter()
                .any(|allowed| allowed == origin)
        })
        .map_or(OriginPolicy::Forbidden, |origin| {
            OriginPolicy::Allowed(origin.to_owned())
        })
}

fn tcp_authenticated(request: &Request, policy: &AccessPolicy) -> bool {
    let mut authorizations = request.headers().get_all(header::AUTHORIZATION).iter();
    let authorization = match (authorizations.next(), authorizations.next()) {
        (Some(value), None) => Some(value),
        _ => None,
    };
    let bearer_ok = authorization
        .and_then(|value| value.as_bytes().strip_prefix(b"Bearer "))
        .zip(policy.bearer.as_deref())
        .is_some_and(|(provided, expected)| expected.matches(provided));
    let mut identities = request.headers().get_all(TAILSCALE_LOGIN).iter();
    let identity = match (identities.next(), identities.next()) {
        (Some(value), None) => value.to_str().ok(),
        _ => None,
    };
    bearer_ok || (policy.trust_tailscale && identity.is_some_and(valid_identity))
}

fn valid_identity(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 254
        && value
            .bytes()
            .all(|byte| byte.is_ascii_graphic() && byte != b',' && byte != b';')
}

fn add_cors(response: &mut Response, origin: Option<&str>) {
    let Some(origin) = origin.and_then(|value| HeaderValue::from_str(value).ok()) else {
        return;
    };
    response
        .headers_mut()
        .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin);
    response
        .headers_mut()
        .append(header::VARY, HeaderValue::from_static("Origin"));
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, PUT, DELETE, OPTIONS"),
    );
    response.headers_mut().insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static(
            "Authorization, Content-Type, Idempotency-Key, X-Expected-Generation, X-Replace-Confirmation",
        ),
    );
}

fn error(status: StatusCode, code: &'static str, message: &'static str) -> Response {
    let request_id = uuid::Uuid::now_v7().simple().to_string();
    let mut response = (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        Body::from(json!({"code": code, "message": message, "request_id": request_id}).to_string()),
    )
        .into_response();
    response.headers_mut().insert(
        "x-request-id",
        HeaderValue::from_str(&request_id).expect("generated request IDs are valid headers"),
    );
    response
}

fn cors_error(
    status: StatusCode,
    code: &'static str,
    message: &'static str,
    origin: Option<&str>,
) -> Response {
    let mut response = error(status, code, message);
    add_cors(&mut response, origin);
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Router, middleware, routing::get};
    use std::{fs, os::unix::fs::PermissionsExt};
    use tower::ServiceExt;

    fn secured(token: Option<&[u8]>, trusted: bool) -> Router {
        let config = SecurityConfig {
            trusted_loopback_proxy: trusted,
            trust_tailscale_headers: trusted,
            allowed_origins: vec!["https://admin.example".into()],
            ..SecurityConfig::default()
        };
        let token = token.map(|value| Zeroizing::new(value.to_vec()));
        Router::new()
            .route("/", get(|| async { "ok" }))
            .layer(middleware::from_fn_with_state(
                AccessPolicy::tcp(token.as_ref(), &config),
                enforce,
            ))
    }

    #[tokio::test]
    async fn tcp_authentication_and_spoofed_identity_fail_closed() {
        let app = secured(Some(b"correct"), false);
        assert_eq!(
            app.clone()
                .oneshot(Request::builder().uri("/").body(Body::empty()).unwrap())
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            app.clone()
                .oneshot(
                    Request::builder()
                        .uri("/")
                        .header(TAILSCALE_LOGIN, "spoofed@example")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap()
                .status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            app.oneshot(
                Request::builder()
                    .uri("/")
                    .header(header::AUTHORIZATION, "Bearer correct")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap()
            .status(),
            StatusCode::OK
        );
    }

    #[tokio::test]
    async fn origins_and_duplicate_security_headers_fail_closed() {
        let app = secured(Some(b"correct"), false);
        let duplicate = Request::builder()
            .uri("/")
            .header(header::AUTHORIZATION, "Bearer correct")
            .header(header::AUTHORIZATION, "Bearer wrong")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(duplicate).await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
        let forbidden = Request::builder()
            .uri("/")
            .header(header::ORIGIN, "https://evil.example")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.oneshot(forbidden).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
    }

    #[test]
    fn bearer_files_require_private_regular_files() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("bearer");
        let reference = CredentialReference::File { path: path.clone() };
        fs::write(&path, b"correct\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(load_bearer(&reference).unwrap().as_slice(), b"correct");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        assert_eq!(
            load_bearer(&reference).unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
    }
}
