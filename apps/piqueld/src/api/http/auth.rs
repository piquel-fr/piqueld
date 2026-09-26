//! Authentication HTTP boundary shared by both daemon transports.
use super::ApiError;
use crate::auth::{Auth, AuthError, Identity};
use axum::{
    Extension, Json, Router,
    extract::Request,
    http::{HeaderMap, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use piqueld_core::auth::{
    AuthStatus, Ceremony, CeremonyFinish, DeviceApprove, DevicePoll, DeviceStart, DeviceToken,
    Directory, Manage, Managed, RegistrationStart, User,
};

/// Wraps an API router with mandatory authentication and the authentication
/// service. Apply this boundary to every listener before serving requests.
pub fn protect(router: Router, auth: Auth) -> Router {
    router
        .layer(middleware::from_fn_with_state(auth.clone(), authenticate))
        .layer(Extension(auth))
}

impl From<AuthError> for ApiError {
    fn from(error: AuthError) -> Self {
        match error {
            AuthError::Unauthorized | AuthError::Webauthn(_) => Self::new(
                StatusCode::UNAUTHORIZED,
                "authentication_required",
                "Sign in with a passkey or run piquelctl login",
            ),
            AuthError::Invalid(message) => {
                Self::new(StatusCode::BAD_REQUEST, "authentication_invalid", message)
            }
            AuthError::Busy => Self::new(
                StatusCode::TOO_MANY_REQUESTS,
                "authentication_busy",
                "Too many authentication requests; try again shortly",
            ),
            AuthError::Database(ref source)
                if source
                    .as_database_error()
                    .is_some_and(sqlx::error::DatabaseError::is_unique_violation) =>
            {
                Self::new(
                    StatusCode::CONFLICT,
                    "account_conflict",
                    "Account name or passkey is already registered",
                )
            }
            error => {
                tracing::error!(error = ?error, "authentication operation failed");
                Self::new(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "authentication_failed",
                    "Authentication operation failed",
                )
            }
        }
    }
}

fn cookie<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .find_map(|entry| {
            let (key, value) = entry.trim().split_once('=')?;
            (key == name).then_some(value)
        })
}
pub(super) fn is_public(path: &str) -> bool {
    matches!(
        path,
        "/api/v1/auth/status"
            | "/api/v1/auth/register/start"
            | "/api/v1/auth/register/finish"
            | "/api/v1/auth/login/start"
            | "/api/v1/auth/login/finish"
            | "/api/v1/auth/device/start"
            | "/api/v1/auth/device/poll"
    )
}
async fn authenticate(
    axum::extract::State(auth): axum::extract::State<Auth>,
    mut request: Request,
    next: Next,
) -> Response {
    if !super::ui::is_api_path(request.uri().path()) {
        return next.run(request).await;
    }
    let public = is_public(request.uri().path());
    if request.method() == Method::POST
        && matches!(
            request.uri().path(),
            "/api/v1/auth/login/start"
                | "/api/v1/auth/register/start"
                | "/api/v1/auth/device/start"
        )
    {
        let peer = request
            .extensions()
            .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
            .map(|peer| peer.0.ip());
        if let Err(error) = auth.admit_start(peer).await {
            let mut response = ApiError::from(error).into_response();
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, header::HeaderValue::from_static("60"));
            response.headers_mut().insert(
                header::CACHE_CONTROL,
                header::HeaderValue::from_static("no-store"),
            );
            return response;
        }
    }
    let bearer = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let session = cookie(request.headers(), "piqueld_session");
    let uses_cookie = bearer.is_none() && session.is_some();
    let origin = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok());
    let mutation = !matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    );
    let ceremony = request.uri().path().contains("/auth/register/")
        || request.uri().path().contains("/auth/login/");
    if mutation
        && ((origin.is_some() && origin != Some(auth.origin()))
            || ((uses_cookie || (ceremony && bearer.is_none())) && origin != Some(auth.origin())))
    {
        return ApiError::new(
            StatusCode::FORBIDDEN,
            "origin_mismatch",
            "Request origin does not match the configured website",
        )
        .into_response();
    }
    if let Some(secret) = bearer.or(session) {
        match auth.authenticate(secret).await {
            Ok(identity) => {
                request.extensions_mut().insert(identity);
            }
            Err(AuthError::Unauthorized) if public => {}
            Err(error) => return ApiError::from(error).into_response(),
        }
    } else if !public {
        return ApiError::from(AuthError::Unauthorized).into_response();
    }
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    response
}
fn session_response(auth: &Auth, user: User, token: Option<String>) -> Response {
    let mut response = Json(user).into_response();
    if let Some(token) = token {
        response.headers_mut().append(
            header::SET_COOKIE,
            auth.cookie("piqueld_session", &token, 7 * 86400)
                .parse()
                .expect("generated cookie is valid"),
        );
    }
    response
}
fn binding(headers: &HeaderMap) -> Result<&str, ApiError> {
    cookie(headers, "piqueld_ceremony").ok_or_else(|| AuthError::Unauthorized.into())
}
fn challenge_response(auth: &Auth, ceremony: Ceremony, binding: &str) -> Response {
    let mut response = Json(ceremony).into_response();
    response.headers_mut().append(
        header::SET_COOKIE,
        auth.cookie("piqueld_ceremony", binding, 300)
            .parse()
            .expect("generated cookie is valid"),
    );
    response
}

#[utoipa::path(get,path="/api/v1/auth/status",operation_id="authStatus",responses((status=200,body=AuthStatus)))]
pub(super) async fn status(Extension(auth): Extension<Auth>) -> Result<Json<AuthStatus>, ApiError> {
    Ok(Json(auth.status().await?))
}
#[utoipa::path(get,path="/api/v1/auth/me",operation_id="authMe",responses((status=200,body=User)))]
pub(super) async fn me(Extension(identity): Extension<Identity>) -> Json<User> {
    Json(identity.user)
}
#[utoipa::path(post,path="/api/v1/auth/register/start",operation_id="authRegistrationStart",request_body=RegistrationStart,responses((status=200,body=Ceremony)))]
pub(super) async fn register_start(
    Extension(auth): Extension<Auth>,
    identity: Option<Extension<Identity>>,
    Json(input): Json<RegistrationStart>,
) -> Result<Response, ApiError> {
    let binding = Auth::secret()?;
    let ceremony = auth
        .registration_start(input, &binding, identity.is_some())
        .await?;
    Ok(challenge_response(&auth, ceremony, &binding))
}
#[utoipa::path(post,path="/api/v1/auth/register/finish",operation_id="authRegistrationFinish",request_body=CeremonyFinish,responses((status=200,body=User)))]
pub(super) async fn register_finish(
    Extension(auth): Extension<Auth>,
    identity: Option<Extension<Identity>>,
    headers: HeaderMap,
    Json(input): Json<CeremonyFinish>,
) -> Result<Response, ApiError> {
    let (user, token) = auth
        .registration_finish(input, binding(&headers)?, identity.is_some())
        .await?;
    Ok(session_response(&auth, user, token))
}
#[utoipa::path(post,path="/api/v1/auth/login/start",operation_id="authLoginStart",responses((status=200,body=Ceremony)))]
pub(super) async fn login_start(Extension(auth): Extension<Auth>) -> Result<Response, ApiError> {
    let binding = Auth::secret()?;
    let ceremony = auth.login_start(&binding).await?;
    Ok(challenge_response(&auth, ceremony, &binding))
}
#[utoipa::path(post,path="/api/v1/auth/login/finish",operation_id="authLoginFinish",request_body=CeremonyFinish,responses((status=200,body=User)))]
pub(super) async fn login_finish(
    Extension(auth): Extension<Auth>,
    headers: HeaderMap,
    Json(input): Json<CeremonyFinish>,
) -> Result<Response, ApiError> {
    let (user, token) = auth.login_finish(input, binding(&headers)?).await?;
    Ok(session_response(&auth, user, Some(token)))
}
#[utoipa::path(post,path="/api/v1/auth/logout",operation_id="authLogout",responses((status=200,body=Managed)))]
pub(super) async fn logout(
    Extension(auth): Extension<Auth>,
    Extension(identity): Extension<Identity>,
) -> Result<Response, ApiError> {
    auth.logout(&identity.credential_id).await?;
    Ok((
        [(header::SET_COOKIE, auth.cookie("piqueld_session", "", 0))],
        Json(Managed::default()),
    )
        .into_response())
}
#[utoipa::path(get,path="/api/v1/auth/directory",operation_id="authDirectory",responses((status=200,body=Directory)))]
pub(super) async fn directory(
    Extension(auth): Extension<Auth>,
) -> Result<Json<Directory>, ApiError> {
    Ok(Json(auth.directory().await?))
}
#[utoipa::path(post,path="/api/v1/auth/manage",operation_id="authManage",request_body=Manage,responses((status=200,body=Managed)))]
pub(super) async fn manage(
    Extension(auth): Extension<Auth>,
    Extension(identity): Extension<Identity>,
    Json(input): Json<Manage>,
) -> Result<Json<Managed>, ApiError> {
    Ok(Json(auth.manage(&identity.user.id, input).await?))
}
#[utoipa::path(post,path="/api/v1/auth/device/start",operation_id="authDeviceStart",responses((status=200,body=DeviceStart)))]
pub(super) async fn device_start(
    Extension(auth): Extension<Auth>,
) -> Result<Json<DeviceStart>, ApiError> {
    Ok(Json(auth.device_start().await?))
}
#[utoipa::path(post,path="/api/v1/auth/device/poll",operation_id="authDevicePoll",request_body=DevicePoll,responses((status=200,body=DeviceToken)))]
pub(super) async fn device_poll(
    Extension(auth): Extension<Auth>,
    Json(input): Json<DevicePoll>,
) -> Result<Json<DeviceToken>, ApiError> {
    Ok(Json(auth.device_poll(&input.device_code).await?))
}
#[utoipa::path(post,path="/api/v1/auth/device/approve",operation_id="authDeviceApprove",request_body=DeviceApprove,responses((status=200,body=Managed)))]
pub(super) async fn device_approve(
    Extension(auth): Extension<Auth>,
    Extension(identity): Extension<Identity>,
    Json(input): Json<DeviceApprove>,
) -> Result<Json<Managed>, ApiError> {
    auth.device_approve(&input.user_code, &identity).await?;
    Ok(Json(Managed::default()))
}
