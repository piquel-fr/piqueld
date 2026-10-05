//! Authentication HTTP boundary shared by both daemon transports.
use super::ApiError;
use crate::auth::{Auth, AuthError, Identity};
use crate::store::StoreError;
use axum::{
    Extension, Json, Router,
    extract::{ConnectInfo, Request},
    http::{HeaderMap, Method, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use piqueld_core::auth::{
    AuthStatus, Ceremony, CeremonyFinish, DeviceApprove, DevicePoll, DeviceRequest, DeviceStart,
    DeviceStartRequest, DeviceToken, Directory, Manage, Managed, RecoveryLink, RegistrationStart,
    Session, SetupLink, User,
};
use std::net::SocketAddr;

/// Authentication boundary that every API router requires and applies itself.
/// Daemon listeners use the passkey [`Auth`] service; tests may supply a fake.
pub trait Authenticator: Clone + Send + Sync + 'static {
    /// Wraps every route and fallback already registered on `router`.
    fn guard<S: Clone + Send + Sync + 'static>(self, router: Router<S>) -> Router<S>;
}

impl Authenticator for Auth {
    /// Installs the `authenticate` middleware and exposes `Auth` to handlers.
    fn guard<S: Clone + Send + Sync + 'static>(self, router: Router<S>) -> Router<S> {
        router
            .layer(middleware::from_fn_with_state(self.clone(), authenticate))
            .layer(Extension(self))
    }
}

impl From<AuthError> for ApiError {
    /// Maps authentication failures. Passkey verification errors are logged
    /// and reported as a plain 401 so clients learn nothing about the cause.
    fn from(error: AuthError) -> Self {
        match error {
            AuthError::Webauthn(ref source) => {
                // Includes counter regressions that can indicate a cloned authenticator.
                tracing::warn!(error = %source, "passkey verification failed");
                Self::from(AuthError::Unauthorized)
            }
            AuthError::Unauthorized => Self::new(
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
            AuthError::SetupCompleted => Self::new(
                StatusCode::CONFLICT,
                "setup_completed",
                "First-account setup is already completed; run piquelctl login",
            ),
            AuthError::SetupPending => Self::new(
                StatusCode::CONFLICT,
                "setup_pending",
                "First-account setup is not completed; run piquelctl setup-link instead",
            ),
            AuthError::Store(StoreError::AlreadyExists) => Self::new(
                StatusCode::CONFLICT,
                "account_conflict",
                "Account name or passkey is already registered",
            ),
            AuthError::Store(source) => source.into(),
            AuthError::Denied(denied) => denied.into(),
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

/// Reads one cookie value from the `Cookie` header.
///
/// ```text
/// "a=1; piqueld_session=abc" + "piqueld_session" -> Some("abc")
/// ```
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
/// Authentication middleware for API paths; other paths pass through. Every API
/// response, including rejections, is marked `Cache-Control: no-store`.
async fn authenticate(
    axum::extract::State(auth): axum::extract::State<Auth>,
    request: Request,
    next: Next,
) -> Response {
    if !super::ui::is_api_path(request.uri().path()) {
        return next.run(request).await;
    }
    let mut response = authorize(&auth, request, next).await;
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-store"),
    );
    response
}
/// Authenticates one API request before running the handler:
///
/// 1. Takes the credential from `Authorization: Bearer`, else the session cookie.
/// 2. Throttles ceremony start endpoints per peer IP (429 with `Retry-After`).
/// 3. Blocks cross-site mutations and upgrades: any mismatched `Origin` is
///    rejected, and cookie-authenticated or bearer-less ceremony mutations must
///    send the configured origin.
/// 4. Inserts the resolved `Identity` as an extension, on the request for
///    handlers and on the response for outer layers. Missing or invalid
///    credentials leave it out; the route's access requirement then decides
///    whether the request needs one (see `access::enforce`).
async fn authorize(auth: &Auth, mut request: Request, next: Next) -> Response {
    let bearer = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let session = cookie(request.headers(), &auth.cookie_name("piqueld_session"));
    let secret = bearer.or(session);
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
            .get::<ConnectInfo<SocketAddr>>()
            .map(|peer| peer.0.ip());
        if let Err(error) = auth.admit_start(peer).await {
            let mut response = ApiError::from(error).into_response();
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, header::HeaderValue::from_static("60"));
            return refused(auth, secret, response).await;
        }
    }
    let uses_cookie = bearer.is_none() && session.is_some();
    let origin = request
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok());
    // A WebSocket opens with GET, but browsers send cookies on cross-site
    // handshakes, so it is checked like a mutation.
    let mutation = !matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    ) || request.headers().contains_key(header::UPGRADE);
    let ceremony = request.uri().path().contains("/auth/register/")
        || request.uri().path().contains("/auth/login/");
    if mutation
        && ((origin.is_some() && origin != Some(auth.origin()))
            || ((uses_cookie || (ceremony && bearer.is_none())) && origin != Some(auth.origin())))
    {
        let response = ApiError::new(
            StatusCode::FORBIDDEN,
            "origin_mismatch",
            "Request origin does not match the configured website",
        )
        .into_response();
        return refused(auth, secret, response).await;
    }
    let mut identity = None;
    if let Some(secret) = secret {
        match auth.authenticate(secret).await {
            Ok(resolved) => identity = Some(resolved),
            Err(AuthError::Unauthorized) => {}
            Err(error) => return ApiError::from(error).into_response(),
        }
    }
    if let Some(identity) = &identity {
        request.extensions_mut().insert(identity.clone());
    }
    let mut response = next.run(request).await;
    // Outer layers, like the audit trail, see who made the request.
    if let Some(identity) = identity {
        response.extensions_mut().insert(identity);
    }
    response
}
/// Attaches a valid caller's identity to a refusal made before
/// authentication, so the audit trail names it, without refreshing its
/// credential: a refused request must not keep a session alive.
async fn refused(auth: &Auth, secret: Option<&str>, mut response: Response) -> Response {
    if let Some(secret) = secret
        && let Ok((identity, _)) = auth.identify(secret).await
    {
        response.extensions_mut().insert(identity);
    }
    response
}
/// Returns the user as JSON, setting a seven-day session cookie when a new
/// session `token` was issued. The audit trail then records who signed in;
/// otherwise, e.g. adding a passkey to the caller's account, the caller.
fn session_response(auth: &Auth, user: User, token: Option<String>) -> Response {
    let signed_in = super::access::SignedIn(user.clone());
    let mut response = Json(user).into_response();
    if let Some(token) = token {
        response.extensions_mut().insert(signed_in);
        response.headers_mut().append(
            header::SET_COOKIE,
            auth.cookie("piqueld_session", &token, 7 * 86400)
                .parse()
                .expect("generated cookie is valid"),
        );
    }
    response
}
/// Reads the ceremony binding cookie that ties a passkey finish request to the
/// browser that started it; missing cookies are unauthorized.
fn binding<'a>(auth: &Auth, headers: &'a HeaderMap) -> Result<&'a str, ApiError> {
    cookie(headers, &auth.cookie_name("piqueld_ceremony"))
        .ok_or_else(|| AuthError::Unauthorized.into())
}
/// Returns a passkey challenge and sets its five-minute ceremony binding cookie.
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

/// Gets authentication status.
///
/// Public. Reports whether the first account exists and the website origin.
#[utoipa::path(get,path="/api/v1/auth/status",operation_id="authStatus",responses((status=200,body=AuthStatus)))]
pub(super) async fn status(Extension(auth): Extension<Auth>) -> Result<Json<AuthStatus>, ApiError> {
    Ok(Json(auth.status().await?))
}
/// Gets the first-account setup link.
///
/// Public, but served only over the Unix socket, whose group access is the trust boundary; other transports return 404. Returns 409 once the first account exists.
#[utoipa::path(get,path="/api/v1/auth/setup-link",operation_id="authSetupLink",responses((status=200,body=SetupLink),(status=404,response=inline(super::openapi::ApiErrorResponse)),
    (status=409,response=inline(super::openapi::ApiErrorResponse))))]
pub(super) async fn setup_link(
    Extension(auth): Extension<Auth>,
    unix: Option<Extension<super::UnixSocket>>,
) -> Result<Json<SetupLink>, ApiError> {
    if unix.is_none() {
        // Looks absent over TCP, but the audit trail records the refusal.
        let mut error = ApiError::endpoint_not_found();
        error.denied = Some(piqueld_core::access::Denied::Hidden);
        return Err(error);
    }
    Ok(Json(auth.setup_link().await?))
}
/// Issues a one-time admin recovery link.
///
/// Public, but only over the Unix socket and only to root or the daemon's own
/// user, identified by the kernel (`SO_PEERCRED`); anyone else gets 404. The
/// link is valid for 24 hours, replaces any earlier one, and registers a new
/// account with `admin` on every application. Issuing it raises a `security`
/// notification.
#[utoipa::path(post,path="/api/v1/auth/recovery",operation_id="authRecoverAdmin",responses((status=200,body=RecoveryLink),(status=404,response=inline(super::openapi::ApiErrorResponse)),
    (status=409,response=inline(super::openapi::ApiErrorResponse))))]
pub(super) async fn recover_admin(
    Extension(auth): Extension<Auth>,
    unix: Option<Extension<super::UnixSocket>>,
    peer: Option<Extension<ConnectInfo<super::UnixPeer>>>,
) -> Result<Json<RecoveryLink>, ApiError> {
    let operator = rustix::process::geteuid().as_raw();
    let uid = peer
        .and_then(|Extension(ConnectInfo(peer))| peer.uid)
        .filter(|uid| unix.is_some() && (*uid == 0 || *uid == operator));
    let Some(uid) = uid else {
        let mut error = ApiError::endpoint_not_found();
        error.denied = Some(piqueld_core::access::Denied::Hidden);
        return Err(error);
    };
    Ok(Json(auth.recover_admin(&format!("uid {uid}")).await?))
}
/// Gets the signed-in user and what the current credential may do.
#[utoipa::path(get,path="/api/v1/auth/me",operation_id="authMe",responses((status=200,body=Session)))]
pub(super) async fn me(Extension(identity): Extension<Identity>) -> Json<Session> {
    Json(Session {
        user: identity.user,
        grants: identity.grants,
        scoped: identity.scoped,
    })
}
/// Starts passkey registration.
///
/// Public. Registers a new account by redeeming an invitation or setup secret,
/// adds a passkey to an existing account by redeeming an enrollment link, or,
/// when signed in, adds a passkey to the caller's own account. Sets a
/// short-lived ceremony cookie that the finish request must present. Rate
/// limited per client address (429 with `Retry-After`).
#[utoipa::path(post,path="/api/v1/auth/register/start",operation_id="authRegistrationStart",request_body=RegistrationStart,responses((status=200,body=Ceremony)))]
pub(super) async fn register_start(
    Extension(auth): Extension<Auth>,
    identity: Option<Extension<Identity>>,
    Json(input): Json<RegistrationStart>,
) -> Result<Response, ApiError> {
    let binding = Auth::secret()?;
    let ceremony = auth
        .registration_start(
            input,
            &binding,
            identity.as_ref().map(|Extension(identity)| identity),
        )
        .await?;
    Ok(challenge_response(&auth, ceremony, &binding))
}
/// Finishes passkey registration.
///
/// Public. Verifies the passkey against the ceremony started in the same
/// browser. Redeeming an invitation, setup, or enrollment secret signs the
/// account in with a session cookie; a signed-in caller adding a passkey to
/// itself gets none.
#[utoipa::path(post,path="/api/v1/auth/register/finish",operation_id="authRegistrationFinish",request_body=CeremonyFinish,responses((status=200,body=User)))]
pub(super) async fn register_finish(
    Extension(auth): Extension<Auth>,
    identity: Option<Extension<Identity>>,
    headers: HeaderMap,
    Json(input): Json<CeremonyFinish>,
) -> Result<Response, ApiError> {
    let (user, token) = auth
        .registration_finish(
            input,
            binding(&auth, &headers)?,
            identity.as_ref().map(|Extension(identity)| identity),
        )
        .await?;
    Ok(session_response(&auth, user, token))
}
/// Starts a passkey sign-in.
///
/// Public. Usernameless: any discoverable passkey for this site may answer.
/// Sets a short-lived ceremony cookie that the finish request must present.
/// Rate limited per client address (429 with `Retry-After`).
#[utoipa::path(post,path="/api/v1/auth/login/start",operation_id="authLoginStart",responses((status=200,body=Ceremony)))]
pub(super) async fn login_start(Extension(auth): Extension<Auth>) -> Result<Response, ApiError> {
    let binding = Auth::secret()?;
    let ceremony = auth.login_start(&binding).await?;
    Ok(challenge_response(&auth, ceremony, &binding))
}
/// Finishes a passkey sign-in.
///
/// Public. Verifies the passkey and sets a seven-day session cookie.
#[utoipa::path(post,path="/api/v1/auth/login/finish",operation_id="authLoginFinish",request_body=CeremonyFinish,responses((status=200,body=User)))]
pub(super) async fn login_finish(
    Extension(auth): Extension<Auth>,
    headers: HeaderMap,
    Json(input): Json<CeremonyFinish>,
) -> Result<Response, ApiError> {
    let (user, token) = auth.login_finish(input, binding(&auth, &headers)?).await?;
    Ok(session_response(&auth, user, Some(token)))
}
/// Signs out.
///
/// Revokes the credential used for this request and clears the session cookie.
#[utoipa::path(post,path="/api/v1/auth/logout",operation_id="authLogout",responses((status=200,body=Managed)))]
pub(super) async fn logout(
    Extension(auth): Extension<Auth>,
    Extension(identity): Extension<Identity>,
) -> Result<Response, ApiError> {
    auth.logout(&identity).await?;
    Ok((
        [(header::SET_COOKIE, auth.cookie("piqueld_session", "", 0))],
        Json(Managed::default()),
    )
        .into_response())
}
/// Lists accounts, passkeys, credentials, and open invitations.
///
/// With `accounts:manage`, lists every account and invitation; otherwise only
/// the caller's own account.
#[utoipa::path(get,path="/api/v1/auth/directory",operation_id="authDirectory",responses((status=200,body=Directory)))]
pub(super) async fn directory(
    Extension(auth): Extension<Auth>,
    Extension(identity): Extension<Identity>,
) -> Result<Json<Directory>, ApiError> {
    Ok(Json(auth.directory(&identity).await?))
}
/// Applies one account management action as the signed-in user.
///
/// Anyone may manage their own account. Changing other accounts requires
/// `accounts:manage` and every grant the account holds; handing out grants
/// requires holding them (403 `permission_denied` or `permission_exceeded`).
#[utoipa::path(post,path="/api/v1/auth/manage",operation_id="authManage",request_body=Manage,responses((status=200,body=Managed)))]
pub(super) async fn manage(
    Extension(auth): Extension<Auth>,
    Extension(identity): Extension<Identity>,
    Json(input): Json<Manage>,
) -> Result<Json<Managed>, ApiError> {
    Ok(Json(auth.manage(&identity, input).await?))
}
/// Starts a device sign-in for a command-line client.
///
/// Public. Returns a device code to poll with and a user code to approve in a
/// signed-in browser. A JSON body, which may be omitted, limits the issued session to
/// `grants`, which the approver must hold, and is echoed back so clients can
/// tell the limit applies. Pending logins are kept in memory, so bodies over
/// 16 KiB answer 413. Rate limited per client address (429 with `Retry-After`).
#[utoipa::path(post,path="/api/v1/auth/device/start",operation_id="authDeviceStart",request_body=DeviceStartRequest,responses((status=200,body=DeviceStart),(status=413,response=inline(super::openapi::ApiErrorResponse))))]
pub(super) async fn device_start(
    Extension(auth): Extension<Auth>,
    peer: Option<Extension<ConnectInfo<SocketAddr>>>,
    body: Result<axum::body::Bytes, axum::extract::rejection::BytesRejection>,
) -> Result<Json<DeviceStart>, ApiError> {
    let body = super::applications::request_body(body)?;
    let requester = peer.map(|Extension(ConnectInfo(peer))| peer.ip());
    let request = if body.is_empty() {
        DeviceStartRequest::default()
    } else {
        super::decode_json::<DeviceStartRequest>(&body)?
    };
    Ok(Json(auth.device_start(requester, request.grants).await?))
}
/// Bodies [`device_start`] accepts; larger ones answer 413 while they are
/// read, before anything is buffered beyond it.
const DEVICE_START_LIMIT_BYTES: usize = 16 * 1024;
/// Registers [`device_start`] with its [`DEVICE_START_LIMIT_BYTES`] body
/// limit, marking the body optional in the `OpenAPI` document: older clients
/// send none.
pub(super) fn device_start_route<S: Clone + Send + Sync + 'static>(
    (schemas, mut paths, router): utoipa_axum::router::UtoipaMethodRouter<S>,
) -> utoipa_axum::router::UtoipaMethodRouter<S> {
    let router = router.layer(axum::extract::DefaultBodyLimit::max(
        DEVICE_START_LIMIT_BYTES,
    ));
    for item in paths.paths.values_mut() {
        if let Some(body) = item
            .post
            .as_mut()
            .and_then(|operation| operation.request_body.as_mut())
        {
            body.required = Some(utoipa::openapi::Required::False);
        }
    }
    (schemas, paths, router)
}
/// Polls a device sign-in.
///
/// Public. Poll at most every five seconds: the status is
/// `authorization_pending` until approval, `slow_down` when polled too fast, and
/// `complete` with a 30-day token exactly once after approval. Expired or unknown
/// codes fail with 401.
#[utoipa::path(post,path="/api/v1/auth/device/poll",operation_id="authDevicePoll",request_body=DevicePoll,responses((status=200,body=DeviceToken)))]
pub(super) async fn device_poll(
    Extension(auth): Extension<Auth>,
    Json(input): Json<DevicePoll>,
) -> Result<Response, ApiError> {
    let token = auth.device_poll(&input.device_code).await?;
    let signed_in = token.user.clone().map(super::access::SignedIn);
    let mut response = Json(token).into_response();
    if let Some(signed_in) = signed_in {
        response.extensions_mut().insert(signed_in);
    }
    Ok(response)
}
/// Inspects a pending device sign-in.
///
/// Shows the requesting address and remaining lifetime so the user can confirm
/// the request before approving it.
#[utoipa::path(post,path="/api/v1/auth/device/inspect",operation_id="authDeviceInspect",request_body=DeviceApprove,responses((status=200,body=DeviceRequest)))]
pub(super) async fn device_inspect(
    Extension(auth): Extension<Auth>,
    Json(input): Json<DeviceApprove>,
) -> Result<Json<DeviceRequest>, ApiError> {
    Ok(Json(auth.device_inspect(&input.user_code).await?))
}
/// Approves a pending device sign-in.
///
/// The device's next poll receives a token for the signed-in user.
#[utoipa::path(post,path="/api/v1/auth/device/approve",operation_id="authDeviceApprove",request_body=DeviceApprove,responses((status=200,body=Managed)))]
pub(super) async fn device_approve(
    Extension(auth): Extension<Auth>,
    Extension(identity): Extension<Identity>,
    Json(input): Json<DeviceApprove>,
) -> Result<Json<Managed>, ApiError> {
    auth.device_approve(&input.user_code, &identity).await?;
    Ok(Json(Managed::default()))
}
