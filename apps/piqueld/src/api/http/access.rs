//! Per-route access requirements and their enforcement.
//!
//! Every API route is registered through [`public!`], [`authenticated!`], or
//! [`granted!`], which record the requirement on its `OpenAPI` operation as
//! `x-piqueld-access`. Building the router fails for any operation without
//! one, so routes are denied unless they declare who may call them, and the
//! published contract documents each requirement. [`enforce`] checks every
//! API request against the resulting table after authentication.
//!
//! On `/api/v1/applications/{id}` routes, application permissions are checked
//! on that application, and on `/api/v1/environments/{id}` routes on the
//! environment's application: callers who cannot read it get 404, as if it did
//! not exist. Elsewhere they only require the permission on some application,
//! and handlers narrow results to the applications the caller may see.
use super::{ApiError, ApiState, ui};
use crate::auth::{AuthError, Identity};
use axum::{
    extract::{
        ConnectInfo, MatchedPath, RawPathParams, Request, State, rejection::RawPathParamsRejection,
    },
    http::{Method, StatusCode},
    middleware::Next,
    response::{IntoResponse, Response},
};
use piqueld_core::audit::AuditOutcome;
use piqueld_core::{
    ApplicationId, EnvironmentId,
    access::{AppPermission, Denied, GlobalPermission, Permission, Target},
};
use serde_json::json;
use std::{collections::HashMap, net::SocketAddr, sync::Arc};
use tower_http::request_id::RequestId;
use utoipa::openapi::{OpenApi, PathItem, path::Operation};
use utoipa_axum::router::UtoipaMethodRouter;

/// `OpenAPI` operation extension naming the route's requirement.
const EXTENSION: &str = "x-piqueld-access";
/// What the `{id}` path parameter of a route names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PathOwner {
    /// An application.
    Application,
    /// An environment, checked on its application.
    Environment,
}

impl PathOwner {
    /// Route prefixes whose `{id}` path parameter names an owner.
    const ROUTES: [(&str, Self); 2] = [
        ("/api/v1/applications/{id}", Self::Application),
        ("/api/v1/environments/{id}", Self::Environment),
    ];

    /// The owner a matched route names, with its `{id}`, if any.
    fn find(
        matched: Option<&MatchedPath>,
        params: Result<RawPathParams, RawPathParamsRejection>,
    ) -> Option<(Self, String)> {
        let path = matched?.as_str();
        let (_, owner) = Self::ROUTES
            .iter()
            .find(|(prefix, _)| path.starts_with(prefix))?;
        let params = params.ok()?;
        let id = params.iter().find(|(name, _)| *name == "id")?.1.to_owned();
        Some((*owner, id))
    }

    /// The application `id` names, if it exists. Malformed IDs name none.
    async fn application(
        self,
        state: &ApiState,
        id: &str,
    ) -> Result<Option<ApplicationId>, ApiError> {
        Ok(match self {
            Self::Application => ApplicationId::parse(id).ok(),
            Self::Environment => match EnvironmentId::parse(id) {
                Ok(id) => state.environment_application(&id).await?,
                Err(_) => None,
            },
        })
    }
}

/// Who may call a route.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Access {
    /// Anyone, without credentials.
    Public,
    /// Any signed-in caller; the handler decides what it may see or change.
    Authenticated,
    /// Callers holding this permission.
    Granted(Permission),
}

impl Access {
    /// Wire value of the `x-piqueld-access` extension.
    fn as_str(self) -> &'static str {
        match self {
            Self::Public => "public",
            Self::Authenticated => "authenticated",
            Self::Granted(permission) => permission.as_str(),
        }
    }

    /// Parses an `x-piqueld-access` value.
    fn parse(value: &str) -> Option<Self> {
        match value {
            "public" => Some(Self::Public),
            "authenticated" => Some(Self::Authenticated),
            permission => Permission::parse(permission).map(Self::Granted),
        }
    }

    /// Records this requirement on every operation of a route set. Public
    /// operations also drop the document's default security requirement.
    pub(super) fn declare<S>(
        self,
        (schemas, mut paths, router): UtoipaMethodRouter<S>,
    ) -> UtoipaMethodRouter<S> {
        for item in paths.paths.values_mut() {
            for (_, operation) in operations_mut(item) {
                operation
                    .extensions
                    .get_or_insert_default()
                    .insert(EXTENSION.into(), self.as_str().into());
                if self == Self::Public {
                    operation.security = Some(Vec::new());
                }
            }
        }
        (schemas, paths, router)
    }
}

/// Registers routes callable without credentials.
macro_rules! public {
    ($($handler:path),+ $(,)?) => {
        $crate::api::http::access::Access::Public.declare(utoipa_axum::routes!($($handler),+))
    };
}

/// Registers routes callable by any signed-in caller; handlers authorize
/// what the caller may see or change.
macro_rules! authenticated {
    ($($handler:path),+ $(,)?) => {
        $crate::api::http::access::Access::Authenticated.declare(utoipa_axum::routes!($($handler),+))
    };
}

/// Registers routes requiring a permission, e.g.
/// `granted!(App(Deploy) => deployments::deploy)`.
macro_rules! granted {
    ($kind:ident($permission:ident) => $($handler:path),+ $(,)?) => {
        $crate::api::http::access::Access::Granted(piqueld_core::access::Permission::$kind(
            granted!(@permission $kind $permission),
        ))
        .declare(utoipa_axum::routes!($($handler),+))
    };
    (@permission App $permission:ident) => { piqueld_core::access::AppPermission::$permission };
    (@permission Global $permission:ident) => { piqueld_core::access::GlobalPermission::$permission };
}

/// The requirement of every documented operation, by path template and method.
pub(super) struct RouteAccess(HashMap<(String, Method), Access>);

impl RouteAccess {
    /// Indexes the requirement of every documented operation.
    ///
    /// # Panics
    /// Panics when an operation was registered without [`public!`],
    /// [`authenticated!`], or [`granted!`], or names an unknown permission, so
    /// an undeclared route can never be served.
    pub(super) fn build(document: &OpenApi) -> Arc<Self> {
        let mut routes = HashMap::new();
        for (path, item) in &document.paths.paths {
            for (method, operation) in operations(item) {
                let access = operation
                    .extensions
                    .as_ref()
                    .and_then(|extensions| extensions.get(EXTENSION))
                    .and_then(|value| value.as_str())
                    .and_then(Access::parse)
                    .unwrap_or_else(|| {
                        panic!("{method} {path} must be registered with public!, authenticated!, or granted!")
                    });
                routes.insert((path.clone(), method), access);
            }
        }
        Arc::new(Self(routes))
    }

    /// The requirement for a request. `HEAD` follows `GET`; unknown routes and
    /// methods, which only produce 404 or 405, require a signed-in caller.
    fn get(&self, path: Option<&MatchedPath>, method: &Method) -> Access {
        let method = if method == Method::HEAD {
            Method::GET
        } else {
            method.clone()
        };
        path.and_then(|path| self.0.get(&(path.as_str().to_owned(), method)))
            .copied()
            .unwrap_or(Access::Authenticated)
    }
}

/// Authorization middleware for API paths, run after authentication.
///
/// 1. Public routes pass.
/// 2. Requests without an authenticated identity get 401.
/// 3. Permission routes check the caller's grants, on the path's application
///    when there is one.
pub(super) async fn enforce(
    State((routes, state)): State<(Arc<RouteAccess>, ApiState)>,
    matched: Option<MatchedPath>,
    params: Result<RawPathParams, RawPathParamsRejection>,
    request: Request,
    next: Next,
) -> Response {
    if !ui::is_api_path(request.uri().path()) {
        return next.run(request).await;
    }
    let access = routes.get(matched.as_ref(), request.method());
    let owner = PathOwner::find(matched.as_ref(), params);
    let identity = request.extensions().get::<Identity>().cloned();
    match refusal(&state, access, identity.as_ref(), owner).await {
        None => next.run(request).await,
        Some(response) => response,
    }
}

/// Checks the route's requirement for `identity`, returning the refusal
/// response when it is not met.
async fn refusal(
    state: &ApiState,
    access: Access,
    identity: Option<&Identity>,
    owner: Option<(PathOwner, String)>,
) -> Option<Response> {
    if access == Access::Public {
        return None;
    }
    let Some(identity) = identity else {
        return Some(ApiError::from(AuthError::Unauthorized).into_response());
    };
    let Access::Granted(permission) = access else {
        return None;
    };
    let application = match owner {
        Some((owner, id)) => match owner.application(state, &id).await {
            Ok(application) => Some(application),
            Err(error) => return Some(error.into_response()),
        },
        None => None,
    };
    let target = application
        .as_ref()
        .map(|application| application.as_ref().map_or(Target::Unknown, Target::Id));
    check(identity, permission, target)
        .err()
        .map(|denied| ApiError::from(denied).into_response())
}

/// Audit middleware for API paths, wrapping authentication so its refusals
/// are recorded too. Refusals, writes, and sensitive reads are recorded with
/// their outcome (see [`Audit`]); the caller's identity comes back on the
/// response from authentication.
pub(super) async fn audit(
    State((routes, state)): State<(Arc<RouteAccess>, super::ApiState)>,
    matched: Option<MatchedPath>,
    params: Result<RawPathParams, RawPathParamsRejection>,
    mut request: Request,
    next: Next,
) -> Response {
    if !ui::is_api_path(request.uri().path()) {
        return next.run(request).await;
    }
    let extensions = request.extensions();
    let audit = Audit {
        access: routes.get(matched.as_ref(), request.method()),
        route: matched.as_ref().map_or_else(
            || request.uri().path().chars().take(256).collect(),
            |path| path.as_str().to_owned(),
        ),
        method: request.method().clone(),
        owner: PathOwner::find(matched.as_ref(), params),
        peer: extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ConnectInfo(peer)| peer.ip().to_string()),
        request_id: extensions
            .get::<RequestId>()
            .and_then(|id| id.header_value().to_str().ok().map(str::to_owned)),
    };
    // Handlers that refuse after responding, like exec, record it themselves.
    request.extensions_mut().insert(audit.clone());
    let response = next.run(request).await;
    let extensions = response.extensions();
    let answer = Answer {
        status: response.status(),
        denied: extensions.get::<Denied>().copied(),
        identity: extensions.get::<Identity>().cloned(),
        signed_in: extensions
            .get::<SignedIn>()
            .map(|SignedIn(user)| user.clone()),
    };
    audit.record(&state, answer);
    response
}

/// Account signed in by a public request, e.g. passkey login or a completed
/// CLI login, attached to its response so the audit trail names who signed in.
#[derive(Clone)]
pub(super) struct SignedIn(pub(super) piqueld_core::auth::User);

/// What the audit trail needs from a response.
struct Answer {
    status: StatusCode,
    /// Authorization refusal behind an error response.
    denied: Option<Denied>,
    /// Caller that authentication resolved, if any.
    identity: Option<Identity>,
    /// Account a sign-in request signed in.
    signed_in: Option<piqueld_core::auth::User>,
}

/// One API request, as the audit trail will record it.
#[derive(Clone)]
pub(super) struct Audit {
    access: Access,
    /// Route template, or the raw path (truncated) when no route matched.
    route: String,
    method: Method,
    /// Application or environment the route names.
    owner: Option<(PathOwner, String)>,
    peer: Option<String>,
    request_id: Option<String>,
}

impl Audit {
    /// Reads that reveal sensitive data and are always recorded; other reads,
    /// often polled by clients, are recorded only when refused.
    const SENSITIVE_READS: &[&str] = &[
        "/api/v1/system/configuration",
        "/api/v1/auth/directory",
        "/api/v1/applications/{id}/manifest",
    ];
    /// Writes recorded only when refused or signing someone in: CLI login
    /// polling, which repeats every few seconds until approval.
    const QUIET_WRITES: &[&str] = &["/api/v1/auth/device/poll"];

    /// The `{id}` the route names, when it names an `owner`.
    fn owner_id(&self, owner: PathOwner) -> Option<String> {
        self.owner
            .as_ref()
            .filter(|(named, _)| *named == owner)
            .map(|(_, id)| id.clone())
    }

    /// Records the request when it was refused, wrote state, signed someone
    /// in, ran a command, or read sensitive data (logs, configuration,
    /// manifests, the account directory), and counts refusals. Recording happens in the background
    /// (see `ApplicationService::record_audit`); the response never waits.
    /// Records a refusal decided after the response was sent, e.g. a
    /// command refused after its WebSocket upgrade was allowed.
    pub(super) fn refused_later(
        self,
        state: &super::ApiState,
        identity: Identity,
        error: &ApiError,
    ) {
        let answer = Answer {
            status: error.status,
            denied: error.denied,
            identity: Some(identity),
            signed_in: None,
        };
        self.record(state, answer);
    }

    fn record(self, state: &super::ApiState, answer: Answer) {
        let Answer {
            status,
            denied,
            identity,
            signed_in,
        } = answer;
        let refused = denied.is_some()
            || matches!(
                status,
                StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::TOO_MANY_REQUESTS
            );
        let write = !matches!(self.method, Method::GET | Method::HEAD | Method::OPTIONS)
            && (signed_in.is_some() || !Self::QUIET_WRITES.contains(&self.route.as_str()));
        let sensitive = matches!(
            self.access,
            Access::Granted(Permission::App(
                AppPermission::LogsRead | AppPermission::Exec
            ))
        ) || Self::SENSITIVE_READS.contains(&self.route.as_str());
        if !(refused || write || sensitive) {
            return;
        }
        let outcome = if refused {
            state.count_denial();
            AuditOutcome::Denied
        } else if status.is_client_error() || status.is_server_error() {
            AuditOutcome::Failed
        } else {
            AuditOutcome::Allowed
        };
        // A sign-in names the account it signed in; any credential the request
        // also carried belongs to whoever was signed in before.
        let credential = identity.as_ref().filter(|_| signed_in.is_none());
        let user = signed_in.or_else(|| identity.as_ref().map(|identity| identity.user.clone()));
        let permission = match denied {
            Some(Denied::Missing(permission)) => Some(permission.as_str()),
            _ => None,
        };
        let application_id = self.owner_id(PathOwner::Application);
        let environment_id = self.owner_id(PathOwner::Environment);
        state.record_audit(crate::store::NewAuditEvent {
            action: format!("{} {}", self.method, self.route),
            outcome,
            status: status.as_u16(),
            user_id: user.as_ref().map(|user| user.id.clone()),
            username: user.map(|user| user.username),
            credential_id: credential.map(|identity| identity.credential_id.clone()),
            credential_kind: credential.map(|identity| identity.kind.as_str()),
            scoped: credential.map(|identity| identity.scoped),
            peer: self.peer,
            request_id: self.request_id,
            application_id,
            environment_id,
            permission,
        });
    }
}

/// Checks `permission`, on the route's `target` application when it names
/// one. An unknown one, including a malformed ID, passes only for callers who
/// hold the permission on every application, so the handler reports it
/// without revealing anything to others.
fn check(
    identity: &Identity,
    permission: Permission,
    target: Option<Target<'_>>,
) -> Result<(), Denied> {
    let grants = &identity.grants;
    match (permission, target) {
        (Permission::App(permission), Some(target)) => grants.require_change(&[permission], target),
        (permission, _) => grants.require(permission),
    }
}

/// History `identity` may read: events of applications it holds
/// `events:read` on, and daemon events with `system:read`.
///
/// # Errors
/// Returns [`Denied::Missing`] when it may read no history at all.
pub(super) fn history(identity: &Identity) -> Result<crate::store::Visibility, Denied> {
    let visible = crate::store::Visibility {
        applications: identity.grants.app_scope(AppPermission::EventsRead),
        daemon: identity.grants.has_global(GlobalPermission::SystemRead),
    };
    if visible.applications.is_empty() && !visible.daemon {
        return Err(Denied::Missing(Permission::App(AppPermission::EventsRead)));
    }
    Ok(visible)
}

/// Every operation of a path item with its method.
fn operations(item: &PathItem) -> impl Iterator<Item = (Method, &Operation)> {
    [
        (Method::GET, &item.get),
        (Method::PUT, &item.put),
        (Method::POST, &item.post),
        (Method::DELETE, &item.delete),
        (Method::OPTIONS, &item.options),
        (Method::HEAD, &item.head),
        (Method::PATCH, &item.patch),
        (Method::TRACE, &item.trace),
    ]
    .into_iter()
    .filter_map(|(method, operation)| operation.as_ref().map(|operation| (method, operation)))
}

/// Every operation of a path item with its method, mutably.
fn operations_mut(item: &mut PathItem) -> impl Iterator<Item = (Method, &mut Operation)> {
    [
        (Method::GET, &mut item.get),
        (Method::PUT, &mut item.put),
        (Method::POST, &mut item.post),
        (Method::DELETE, &mut item.delete),
        (Method::OPTIONS, &mut item.options),
        (Method::HEAD, &mut item.head),
        (Method::PATCH, &mut item.patch),
        (Method::TRACE, &mut item.trace),
    ]
    .into_iter()
    .filter_map(|(method, operation)| operation.as_mut().map(|operation| (method, operation)))
}

impl From<Denied> for ApiError {
    /// Hidden targets look exactly like missing ones; other refusals name what
    /// the caller lacks.
    fn from(denied: Denied) -> Self {
        let mut error = match denied {
            Denied::Hidden => {
                Self::new(StatusCode::NOT_FOUND, "not_found", "resource was not found")
            }
            Denied::Missing(permission) => Self::new(
                StatusCode::FORBIDDEN,
                "permission_denied",
                format!("This action requires the {permission} permission"),
            )
            .details(json!({"permission": permission})),
            Denied::Exceeds => Self::new(
                StatusCode::FORBIDDEN,
                "permission_exceeded",
                "The account or grants include access you do not hold",
            ),
            Denied::Scoped => Self::new(
                StatusCode::FORBIDDEN,
                "credential_scoped",
                "Credentials with limited access, like API tokens, cannot create credentials or change their account",
            ),
        };
        error.denied = Some(denied);
        error
    }
}
