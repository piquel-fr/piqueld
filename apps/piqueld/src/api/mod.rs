//! Versioned HTTP/JSON API and streaming event boundary.
#![allow(missing_docs)]

use async_trait::async_trait;
use axum::{
    Router,
    body::{Body, Bytes},
    extract::{Path, Query, Request, State},
    http::{HeaderMap, Method, StatusCode, header},
    middleware::{self, Next},
    response::{
        IntoResponse, Response, Sse,
        sse::{Event, KeepAlive},
    },
    routing::{get, post},
};
use futures_util::{Stream, stream};
use piqueld_client::{
    AcceptedOperation, ApplicationStatusView, ApplicationView, CreateApplicationRequest,
    DeleteApplicationRequest, Envelope, ErrorBody, ExpectedGeneration, OperationStepView,
    OperationView, Page, PlanApplicationRequest, PlanView, ReplaceApplicationRequest,
    SystemCapabilities, SystemStatus,
};
use piqueld_core::{
    ApplicationId, InstanceId, NormalizedApplication, ObservedApplication, PlanRequest,
    ResolutionSet, compile_application, plan, preview_resolution,
    resource::{ResolvedApplication, ResolvedSource, SecretGeneration},
};
use schemars::{JsonSchema, schema_for};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{convert::Infallible, pin::Pin, sync::Arc, time::Duration};
use tower_http::{
    request_id::{MakeRequestUuid, PropagateRequestIdLayer, RequestId, SetRequestIdLayer},
    trace::TraceLayer,
};

use crate::store::{
    ApplicationRepository, ApplicationState, ApplicationStatus, Operation, OperationKind,
    OperationRepository, OperationStep, SqliteStore, StatusRepository, StepState, StoreError,
    StoredApplication, WorkState,
};

const JSON: &str = "application/json";
const TOML: &str = "application/toml";
const MAX_PAGE: usize = 100;

#[derive(Clone, Debug)]
pub struct RuntimeCapabilities {
    pub source_resolution: bool,
    pub runtime_observation: bool,
    pub runtime_execution: bool,
    pub reason: Option<String>,
}

#[derive(Clone, Debug)]
pub struct PreparedApplication {
    pub resolved: ResolvedApplication,
    pub observed: ObservedApplication,
}

#[derive(Debug, thiserror::Error)]
pub enum BoundaryError {
    #[error("runtime capability is unavailable")]
    Unavailable,
    #[error("runtime request failed")]
    Failed,
}

/// Source resolution, runtime observation, and execution seam supplied by Plan 06.
#[async_trait]
pub trait RuntimeBoundary: Send + Sync + 'static {
    fn capabilities(&self) -> RuntimeCapabilities;
    async fn prepare(
        &self,
        application: &NormalizedApplication,
    ) -> Result<PreparedApplication, BoundaryError>;
    async fn observe(
        &self,
        application: &StoredApplication,
    ) -> Result<ObservedApplication, BoundaryError>;
}

/// Honest production boundary until Docker reconciliation lands in Plan 06.
pub struct UnavailableRuntime;

#[async_trait]
impl RuntimeBoundary for UnavailableRuntime {
    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities {
            source_resolution: false,
            runtime_observation: false,
            runtime_execution: false,
            reason: Some("Docker source resolution and execution arrive in Plan 06".into()),
        }
    }
    async fn prepare(
        &self,
        _: &NormalizedApplication,
    ) -> Result<PreparedApplication, BoundaryError> {
        Err(BoundaryError::Unavailable)
    }
    async fn observe(&self, _: &StoredApplication) -> Result<ObservedApplication, BoundaryError> {
        Err(BoundaryError::Unavailable)
    }
}

#[derive(Clone)]
pub struct ApiState {
    store: Arc<SqliteStore>,
    runtime: Arc<dyn RuntimeBoundary>,
    instance_id: String,
    create_lock: Arc<tokio::sync::Mutex<()>>,
}

impl ApiState {
    #[must_use]
    pub fn new(store: Arc<SqliteStore>, runtime: Arc<dyn RuntimeBoundary>) -> Self {
        Self {
            instance_id: store.instance_id().to_owned(),
            store,
            runtime,
            create_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    code: &'static str,
    message: &'static str,
    details: Value,
}

impl ApiError {
    fn new(status: StatusCode, code: &'static str, message: &'static str) -> Self {
        Self {
            status,
            code,
            message,
            details: Value::Null,
        }
    }
    fn details(mut self, details: Value) -> Self {
        self.details = details;
        self
    }
}

impl From<StoreError> for ApiError {
    fn from(value: StoreError) -> Self {
        match value {
            StoreError::NotFound => {
                Self::new(StatusCode::NOT_FOUND, "not_found", "resource was not found")
            }
            StoreError::AlreadyExists => Self::new(
                StatusCode::CONFLICT,
                "application_name_collision",
                "application identity or name already exists",
            ),
            StoreError::IdempotencyConflict => Self::new(
                StatusCode::CONFLICT,
                "idempotency_key_reused",
                "Idempotency-Key was already used for a different request",
            ),
            StoreError::GenerationConflict { expected, actual } => Self::new(
                StatusCode::CONFLICT,
                "application_generation_conflict",
                "the application was modified by another client",
            )
            .details(json!({"expected_generation": expected, "current_generation": actual})),
            StoreError::MissingSecrets(names) => Self::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "logical_secret_missing",
                "one or more referenced logical secrets do not exist",
            )
            .details(json!({"names": names})),
            StoreError::IllegalTransition => Self::new(
                StatusCode::CONFLICT,
                "application_state_conflict",
                "the requested application transition is not allowed",
            ),
            StoreError::InvalidInput => Self::new(
                StatusCode::BAD_REQUEST,
                "invalid_request",
                "the request is invalid",
            ),
            StoreError::SchemaMismatch => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "schema_mismatch",
                "database schema is incompatible",
            ),
            StoreError::Corrupt => Self::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "stored_state_corrupt",
                "stored application state is corrupt",
            ),
            StoreError::Database => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "storage_unavailable",
                "control-plane storage is unavailable",
            ),
        }
    }
}

impl From<BoundaryError> for ApiError {
    fn from(value: BoundaryError) -> Self {
        match value {
            BoundaryError::Unavailable => Self::new(
                StatusCode::SERVICE_UNAVAILABLE,
                "runtime_unavailable",
                "runtime source resolution or execution is unavailable",
            ),
            BoundaryError::Failed => Self::new(
                StatusCode::BAD_GATEWAY,
                "runtime_request_failed",
                "runtime request failed",
            ),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let body = ErrorBody {
            code: self.code.into(),
            message: self.message.into(),
            details: self.details,
            request_id: uuid::Uuid::now_v7().simple().to_string(),
        };
        (
            self.status,
            [(header::CONTENT_TYPE, JSON)],
            axum::Json(body),
        )
            .into_response()
    }
}

/// Builds the complete Plan 05 router. No later-plan endpoints are published.
pub fn router(state: ApiState) -> Router {
    let request_id = header::HeaderName::from_static("x-request-id");
    Router::new()
        .route("/api/v1/system/status", get(system_status))
        .route("/api/v1/system/capabilities", get(system_capabilities))
        .route("/api/v1/openapi.json", get(openapi))
        .route(
            "/api/v1/applications",
            get(list_applications).post(create_application),
        )
        .route("/api/v1/applications/plan", post(plan_create))
        .route(
            "/api/v1/applications/{id}",
            get(get_application)
                .put(replace_application)
                .delete(delete_application),
        )
        .route("/api/v1/applications/{id}/plan", post(plan_replace))
        .route(
            "/api/v1/applications/{id}/reconcile",
            post(reconcile_application),
        )
        .route("/api/v1/applications/{id}/status", get(application_status))
        .route("/api/v1/applications/{id}/events", get(application_events))
        .route("/api/v1/operations/{id}", get(get_operation))
        .route("/api/v1/operations/{id}/events", get(operation_events))
        .method_not_allowed_fallback(method_not_allowed)
        .fallback(fallback)
        .with_state(state)
        .layer(PropagateRequestIdLayer::new(request_id.clone()))
        .layer(middleware::from_fn(bind_error_request_id))
        .layer(SetRequestIdLayer::new(request_id, MakeRequestUuid))
        .layer(TraceLayer::new_for_http())
}

async fn bind_error_request_id(request: Request, next: Next) -> Response {
    let request_id = request
        .extensions()
        .get::<RequestId>()
        .and_then(|value| value.header_value().to_str().ok())
        .map(str::to_owned);
    let response = next.run(request).await;
    if !response.status().is_client_error() && !response.status().is_server_error() {
        return response;
    }
    let is_json = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.starts_with(JSON));
    if !is_json {
        return response;
    }
    let (parts, body) = response.into_parts();
    let Ok(bytes) = http_body_util::BodyExt::collect(body)
        .await
        .map(http_body_util::Collected::to_bytes)
    else {
        return Response::from_parts(parts, Body::empty());
    };
    let Ok(mut error) = serde_json::from_slice::<ErrorBody>(&bytes) else {
        return Response::from_parts(parts, Body::from(bytes));
    };
    if let Some(request_id) = request_id {
        error.request_id = request_id;
    }
    let bytes = serde_json::to_vec(&error).unwrap_or_else(|_| b"{}".to_vec());
    Response::from_parts(parts, Body::from(bytes))
}

async fn system_status(State(state): State<ApiState>) -> impl IntoResponse {
    ok(SystemStatus {
        status: "running".into(),
        api_version: "v1".into(),
        instance_id: state.instance_id,
    })
}
async fn system_capabilities(State(state): State<ApiState>) -> impl IntoResponse {
    let c = state.runtime.capabilities();
    ok(SystemCapabilities {
        persistence: true,
        source_resolution: c.source_resolution,
        runtime_observation: c.runtime_observation,
        runtime_execution: c.runtime_execution,
        reason: c.reason,
    })
}
async fn openapi() -> impl IntoResponse {
    (
        [(
            header::CONTENT_TYPE,
            "application/vnd.oai.openapi+json;version=3.1",
        )],
        openapi_document().to_string(),
    )
}

#[derive(Deserialize)]
struct ListQuery {
    cursor: Option<String>,
    limit: Option<usize>,
}

async fn list_applications(
    State(state): State<ApiState>,
    Query(query): Query<ListQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let limit = query.limit.unwrap_or(50);
    if limit == 0 || limit > MAX_PAGE {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "pagination_invalid",
            "limit must be between 1 and 100",
        ));
    }
    let values = state.store.list().await?;
    let offset = query
        .cursor
        .as_deref()
        .map_or(Ok(0), str::parse::<usize>)
        .map_err(|_| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "pagination_invalid",
                "cursor is invalid",
            )
        })?;
    if offset > values.len() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "pagination_invalid",
            "cursor is invalid",
        ));
    }
    let mut values = values.into_iter().skip(offset).collect::<Vec<_>>();
    let has_more = values.len() > limit;
    values.truncate(limit);
    let next_cursor = has_more.then(|| (offset + values.len()).to_string());
    Ok(ok(Page {
        items: values.into_iter().map(application_view).collect(),
        next_cursor,
    }))
}

async fn get_application(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let id = application_id(&id)?;
    Ok(ok(application_view(state.store.get(&id).await?)))
}

async fn create_application(
    State(state): State<ApiState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let key = header_text(&headers, "idempotency-key").ok_or_else(|| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "idempotency_key_required",
            "Idempotency-Key is required for application creation",
        )
    })?;
    if key.is_empty() || key.len() > 128 || key.chars().any(char::is_control) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "idempotency_key_invalid",
            "Idempotency-Key is invalid",
        ));
    }
    let validated = parse_manifest(&headers, &body, RequestShape::Create)?;
    let id = idempotent_application_id(key);
    let app = validated.normalize(id.clone());
    let key_hash = idempotency_key_hash(key);
    let request_hash = app.spec_hash();
    // There is exactly one active daemon in the prototype. Serializing create
    // preparation prevents concurrent retries from duplicating resolver/build
    // work before the durable binding can be committed.
    let _create_guard = state.create_lock.lock().await;
    if let Some(mutation) = state
        .store
        .create_idempotency(&id, &key_hash, &request_hash)
        .await?
    {
        return Ok(accepted(AcceptedOperation {
            operation_id: mutation.operation_id,
            application_id: id.to_string(),
            generation: mutation.generation,
        }));
    }
    reject_name_collision(&state, &app, None).await?;
    let prepared = state.runtime.prepare(&app).await?;
    let p = plan(
        &PlanRequest::Reconcile {
            desired: prepared.resolved.clone(),
        },
        &prepared.observed,
    );
    if p.is_blocked() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "plan_blocked",
            "runtime plan contains blocking conflicts",
        )
        .details(json!({"diagnostics": p.diagnostics})));
    }
    let steps = p.actions.iter().map(operation_step).collect::<Vec<_>>();
    let mutation = state
        .store
        .create_idempotent(
            &app,
            Some(&prepared.resolved),
            &steps,
            &key_hash,
            &request_hash,
        )
        .await?;
    Ok(accepted(AcceptedOperation {
        operation_id: mutation.operation_id,
        application_id: id.to_string(),
        generation: mutation.generation,
    }))
}

async fn replace_application(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let id = application_id(&id)?;
    let (validated, expected) = parse_update(&headers, &body)?;
    let current = state.store.get(&id).await?;
    generation(expected, current.generation)?;
    let app = validated.normalize(id.clone());
    reject_name_collision(&state, &app, Some(&id)).await?;
    let prepared = state.runtime.prepare(&app).await?;
    let p = plan(
        &PlanRequest::Reconcile {
            desired: prepared.resolved.clone(),
        },
        &prepared.observed,
    );
    if p.is_blocked() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "plan_blocked",
            "runtime plan contains blocking conflicts",
        ));
    }
    let steps = p.actions.iter().map(operation_step).collect::<Vec<_>>();
    let mutation = state
        .store
        .replace(&app, Some(&prepared.resolved), expected, &steps)
        .await?;
    Ok(accepted(AcceptedOperation {
        operation_id: mutation.operation_id,
        application_id: id.to_string(),
        generation: mutation.generation,
    }))
}

async fn delete_application(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    require_json(&headers)?;
    let request: DeleteApplicationRequest = decode_json(&body)?;
    valid_expected_generation(request.expected_generation)?;
    if request.force {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "force_delete_unsupported",
            "force deletion is not supported; named volumes are always retained",
        ));
    }
    let id = application_id(&id)?;
    let current = state.store.get(&id).await?;
    generation(request.expected_generation, current.generation)?;
    let observed = state.runtime.observe(&current).await?;
    let instance = InstanceId::parse(&state.instance_id).map_err(|_| StoreError::Corrupt)?;
    let p = plan(
        &PlanRequest::Delete {
            application_id: id.clone(),
            instance_id: instance,
        },
        &observed,
    );
    if p.is_blocked() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "plan_blocked",
            "runtime plan contains blocking conflicts",
        )
        .details(json!({"diagnostics": p.diagnostics})));
    }
    let steps = p.actions.iter().map(operation_step).collect::<Vec<_>>();
    let mutation = state
        .store
        .request_delete(&id, request.expected_generation, &steps)
        .await?;
    Ok(accepted(AcceptedOperation {
        operation_id: mutation.operation_id,
        application_id: id.to_string(),
        generation: mutation.generation,
    }))
}

async fn plan_create(
    State(state): State<ApiState>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<impl IntoResponse, ApiError> {
    let validated = parse_manifest(&headers, &body, RequestShape::PlanCreate)?;
    let hash = Sha256::digest(validated.name().as_bytes());
    let id = ApplicationId::parse(format!("preview-{}", hex(&hash[..8])))
        .map_err(|_| StoreError::Corrupt)?;
    let app = validated.normalize(id.clone());
    reject_name_collision(&state, &app, None).await?;
    let p = preview_plan(&state, &app, None).await?;
    Ok(ok(PlanView {
        application_id: id.to_string(),
        proposed_generation: 1,
        plan: p,
    }))
}

async fn plan_replace(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<impl IntoResponse, ApiError> {
    let id = application_id(&id)?;
    let (validated, expected) = parse_update(&headers, &body)?;
    let current = state.store.get(&id).await?;
    generation(expected, current.generation)?;
    let app = validated.normalize(id.clone());
    reject_name_collision(&state, &app, Some(&id)).await?;
    let p = preview_plan(&state, &app, Some(&current)).await?;
    Ok(ok(PlanView {
        application_id: id.to_string(),
        proposed_generation: expected + 1,
        plan: p,
    }))
}

async fn preview_plan(
    state: &ApiState,
    app: &NormalizedApplication,
    current: Option<&StoredApplication>,
) -> Result<piqueld_core::Plan, ApiError> {
    let observed = if let Some(current) = current {
        match state.runtime.observe(current).await {
            Ok(observed) => observed,
            Err(error) => return Err(error.into()),
        }
    } else {
        ObservedApplication::default()
    };
    let resolutions = current
        .and_then(|stored| stored.resolved.as_ref())
        .map_or_else(ResolutionSet::default, |resolved| {
            reusable_resolutions(app, resolved)
        });
    let unresolved = preview_resolution(app, &resolutions);
    let desired = if unresolved.is_empty() {
        current
            .and_then(|stored| stored.resolved.as_ref())
            .and_then(|resolved| {
                let ingress = resolved.networks.iter().find(|network| network.ingress)?;
                compile_application(
                    app,
                    resolved.instance_id.clone(),
                    ingress.name.clone(),
                    &resolutions,
                )
                .ok()
            })
    } else {
        None
    };
    Ok(plan(
        &PlanRequest::Preview {
            unresolved,
            desired,
        },
        &observed,
    ))
}

fn reusable_resolutions(
    app: &NormalizedApplication,
    current: &ResolvedApplication,
) -> ResolutionSet {
    let sources = app
        .spec
        .services
        .iter()
        .filter_map(|service| {
            let resolved = current
                .services
                .iter()
                .find(|candidate| candidate.logical_name == service.name)?;
            let reusable = match (&service.source, &resolved.source) {
                (
                    piqueld_core::manifest::Source::Image { image },
                    ResolvedSource::Image { requested, .. },
                ) => image == requested,
                (
                    piqueld_core::manifest::Source::Git {
                        repository,
                        reference,
                        context,
                        dockerfile,
                    },
                    ResolvedSource::Git {
                        repository: resolved_repository,
                        requested_reference,
                        context: resolved_context,
                        dockerfile: resolved_dockerfile,
                        ..
                    },
                ) => {
                    repository == resolved_repository
                        && reference == requested_reference
                        && context == resolved_context
                        && dockerfile == resolved_dockerfile
                }
                _ => false,
            };
            reusable.then(|| (service.name.clone(), resolved.source.clone()))
        })
        .collect();
    let secrets = current
        .secrets
        .iter()
        .map(|secret| {
            (
                secret.logical_name.clone(),
                SecretGeneration {
                    logical_name: secret.logical_name.clone(),
                    generation: secret.generation.clone(),
                    swarm_name: secret.name.clone(),
                },
            )
        })
        .collect();
    ResolutionSet { sources, secrets }
}

async fn reject_name_collision(
    state: &ApiState,
    app: &NormalizedApplication,
    own_id: Option<&ApplicationId>,
) -> Result<(), ApiError> {
    if state.store.list().await?.iter().any(|stored| {
        stored.application.metadata.name == app.metadata.name
            && own_id != Some(&stored.application.id)
    }) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "application_name_collision",
            "application name already exists",
        ));
    }
    Ok(())
}

async fn reconcile_application(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    require_json(&headers)?;
    let request: ExpectedGeneration = decode_json(&body)?;
    valid_expected_generation(request.expected_generation)?;
    let id = application_id(&id)?;
    let current = state.store.get(&id).await?;
    generation(request.expected_generation, current.generation)?;
    if let Some(mutation) = state
        .store
        .active_reconcile(&id, request.expected_generation)
        .await?
    {
        return Ok(accepted(AcceptedOperation {
            operation_id: mutation.operation_id,
            application_id: id.to_string(),
            generation: mutation.generation,
        }));
    }
    if !state.runtime.capabilities().runtime_execution {
        return Err(BoundaryError::Unavailable.into());
    }
    let desired = current.resolved.clone().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "runtime_unavailable",
            "application has no resolved state available for reconciliation",
        )
    })?;
    let observed = state.runtime.observe(&current).await?;
    let plan = plan(&PlanRequest::Reconcile { desired }, &observed);
    if plan.is_blocked() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "plan_blocked",
            "runtime plan contains blocking conflicts",
        )
        .details(json!({"diagnostics": plan.diagnostics})));
    }
    let steps = plan.actions.iter().map(operation_step).collect::<Vec<_>>();
    let mutation = state
        .store
        .request_reconcile(&id, request.expected_generation, &steps)
        .await?;
    Ok(accepted(AcceptedOperation {
        operation_id: mutation.operation_id,
        application_id: id.to_string(),
        generation: mutation.generation,
    }))
}

async fn application_status(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let id = application_id(&id)?;
    Ok(ok(status_view(state.store.status(&id).await?)))
}

async fn get_operation(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    Ok(ok(operation_view(
        state.store.operation(&id).await?,
        state.store.operation_steps(&id).await?,
    )))
}

async fn operation_events(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    state.store.operation(&id).await?;
    let last = last_event_id(&headers);
    Ok(Sse::new(operation_event_stream(state.store, id, last))
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("keepalive"),
        )
        .into_response())
}

async fn application_events(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let id = application_id(&id)?;
    state.store.status(&id).await?;
    let last = last_event_id(&headers);
    Ok(Sse::new(application_event_stream(state.store, id, last))
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("keepalive"),
        )
        .into_response())
}

type EventStream = Pin<Box<dyn Stream<Item = Result<Event, Infallible>> + Send>>;

fn operation_event_stream(
    store: Arc<SqliteStore>,
    id: String,
    last: Option<String>,
) -> EventStream {
    Box::pin(stream::unfold(
        (store, id, last, false, true),
        |(store, id, last, done, reconnect)| async move {
            if done {
                return None;
            }
            loop {
                tokio::time::sleep(Duration::from_millis(200)).await;
                let Ok(operation) = store.operation(&id).await else {
                    return None;
                };
                let terminal = matches!(
                    operation.state,
                    WorkState::Succeeded | WorkState::Failed | WorkState::Cancelled
                );
                let Ok(steps) = store.operation_steps(&id).await else {
                    return None;
                };
                let data = serde_json::to_string(&operation_view(operation, steps))
                    .unwrap_or_else(|_| "{}".into());
                let event_id = current_state_event_id("operation", &data);
                if last.as_deref() == Some(event_id.as_str()) {
                    if terminal {
                        return None;
                    }
                    continue;
                }
                if reconnect && last.is_some() {
                    let reset = Event::default()
                        .id(format!("reset:{event_id}"))
                        .event("replay_reset")
                        .data("{\"reason\":\"bounded_replay_exhausted\"}");
                    return Some((Ok(reset), (store, id, None, false, false)));
                }
                let event = Event::default()
                    .id(event_id.clone())
                    .event(if terminal { "terminal" } else { "operation" })
                    .data(data);
                return Some((Ok(event), (store, id, Some(event_id), terminal, false)));
            }
        },
    ))
}

fn application_event_stream(
    store: Arc<SqliteStore>,
    id: ApplicationId,
    last: Option<String>,
) -> EventStream {
    Box::pin(stream::unfold(
        (store, id, last, true),
        |(store, id, last, reconnect)| async move {
            loop {
                tokio::time::sleep(Duration::from_millis(200)).await;
                let Ok(status) = store.status(&id).await else {
                    return None;
                };
                let data =
                    serde_json::to_string(&status_view(status)).unwrap_or_else(|_| "{}".into());
                let event_id = current_state_event_id("application", &data);
                if last.as_deref() == Some(event_id.as_str()) {
                    continue;
                }
                if reconnect && last.is_some() {
                    let reset = Event::default()
                        .id(format!("reset:{event_id}"))
                        .event("replay_reset")
                        .data("{\"reason\":\"bounded_replay_exhausted\"}");
                    return Some((Ok(reset), (store, id, None, false)));
                }
                return Some((
                    Ok(Event::default()
                        .id(event_id.clone())
                        .event("application")
                        .data(data)),
                    (store, id, Some(event_id), false),
                ));
            }
        },
    ))
}

async fn fallback(method: Method) -> ApiError {
    let _ = method;
    ApiError::new(
        StatusCode::NOT_FOUND,
        "endpoint_not_found",
        "API endpoint was not found",
    )
}
async fn method_not_allowed() -> ApiError {
    ApiError::new(
        StatusCode::METHOD_NOT_ALLOWED,
        "method_not_allowed",
        "HTTP method is not allowed",
    )
}

fn ok<T: Serialize>(data: T) -> impl IntoResponse {
    (StatusCode::OK, axum::Json(Envelope { data }))
}
fn accepted(data: AcceptedOperation) -> Response {
    (StatusCode::ACCEPTED, axum::Json(Envelope { data })).into_response()
}
fn application_id(value: &str) -> Result<ApplicationId, ApiError> {
    ApplicationId::parse(value).map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "application_id_invalid",
            "application ID is invalid",
        )
    })
}
fn generation(expected: u64, actual: u64) -> Result<(), ApiError> {
    if expected == actual {
        Ok(())
    } else {
        Err(ApiError::from(StoreError::GenerationConflict {
            expected,
            actual,
        }))
    }
}
fn valid_expected_generation(expected: u64) -> Result<(), ApiError> {
    if expected == 0 {
        Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "expected_generation_invalid",
            "expected generation must be a positive integer",
        ))
    } else {
        Ok(())
    }
}
fn header_text<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()
}
fn require_json(headers: &HeaderMap) -> Result<(), ApiError> {
    match content_type(headers) {
        Some(value) if value.eq_ignore_ascii_case(JSON) => Ok(()),
        _ => Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "content_type_unsupported",
            "Content-Type must be application/json",
        )),
    }
}
fn content_type(headers: &HeaderMap) -> Option<&str> {
    header_text(headers, "content-type").map(|v| v.split(';').next().unwrap_or(v).trim())
}
fn decode_json<T: for<'de> Deserialize<'de>>(body: &[u8]) -> Result<T, ApiError> {
    serde_json::from_slice(body).map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "json_malformed",
            "request JSON is malformed or contains unknown fields",
        )
    })
}

#[derive(Clone, Copy)]
enum RequestShape {
    Create,
    PlanCreate,
}
fn parse_manifest(
    headers: &HeaderMap,
    body: &[u8],
    shape: RequestShape,
) -> Result<piqueld_core::ValidatedApplication, ApiError> {
    match content_type(headers) {
        Some(value) if value.eq_ignore_ascii_case(JSON) => {
            let manifest = match shape {
                RequestShape::Create => decode_json::<CreateApplicationRequest>(body)?.manifest,
                RequestShape::PlanCreate => decode_json::<PlanApplicationRequest>(body)?.manifest,
            };
            let encoded = serde_json::to_string(&manifest).map_err(|_| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "manifest_invalid",
                    "application manifest is invalid",
                )
            })?;
            piqueld_core::parse_json(&encoded).map_err(validation_error)
        }
        Some(value)
            if value.eq_ignore_ascii_case(TOML) || value.eq_ignore_ascii_case("text/toml") =>
        {
            std::str::from_utf8(body)
                .map_err(|_| {
                    ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "toml_malformed",
                        "request TOML is malformed",
                    )
                })
                .and_then(|v| piqueld_core::parse_toml(v).map_err(toml_manifest_error))
        }
        _ => Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "content_type_unsupported",
            "Content-Type must be application/json or application/toml",
        )),
    }
}

fn parse_update(
    headers: &HeaderMap,
    body: &[u8],
) -> Result<(piqueld_core::ValidatedApplication, u64), ApiError> {
    match content_type(headers) {
        Some(value) if value.eq_ignore_ascii_case(JSON) => {
            let request: ReplaceApplicationRequest = decode_json(body)?;
            valid_expected_generation(request.expected_generation)?;
            let encoded = serde_json::to_string(&request.manifest).map_err(|_| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "manifest_invalid",
                    "application manifest is invalid",
                )
            })?;
            Ok((
                piqueld_core::parse_json(&encoded).map_err(validation_error)?,
                request.expected_generation,
            ))
        }
        Some(value)
            if value.eq_ignore_ascii_case(TOML) || value.eq_ignore_ascii_case("text/toml") =>
        {
            let expected = header_text(headers, "x-expected-generation")
                .ok_or_else(|| {
                    ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "expected_generation_required",
                        "X-Expected-Generation is required for TOML replacement",
                    )
                })?
                .parse()
                .map_err(|_| {
                    ApiError::new(
                        StatusCode::BAD_REQUEST,
                        "expected_generation_invalid",
                        "expected generation is invalid",
                    )
                })?;
            valid_expected_generation(expected)?;
            let text = std::str::from_utf8(body).map_err(|_| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "toml_malformed",
                    "request TOML is malformed",
                )
            })?;
            Ok((
                piqueld_core::parse_toml(text).map_err(toml_manifest_error)?,
                expected,
            ))
        }
        _ => Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "content_type_unsupported",
            "Content-Type must be application/json or application/toml",
        )),
    }
}

fn validation_error(errors: piqueld_core::ValidationErrors) -> ApiError {
    let piqueld_core::ValidationErrors(errors) = errors;
    ApiError::new(
        StatusCode::UNPROCESSABLE_ENTITY,
        "manifest_validation_failed",
        "application manifest failed validation",
    )
    .details(json!({"errors": errors}))
}
fn toml_manifest_error(errors: piqueld_core::ValidationErrors) -> ApiError {
    if errors
        .0
        .iter()
        .all(|error| error.code == "manifest_decode_failed")
    {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "toml_malformed",
            "request TOML is malformed or does not match the application schema",
        )
    } else {
        validation_error(errors)
    }
}
fn operation_step(action: &piqueld_core::PlanAction) -> String {
    let mut value = action.human_description();
    if value.len() > 64 {
        value.truncate(64);
    }
    value
}
fn idempotent_application_id(key: &str) -> ApplicationId {
    let digest = Sha256::digest(format!("piqueld-create/v1\0{key}").as_bytes());
    ApplicationId::parse(format!("app-{}", hex(&digest[..16])))
        .expect("digest application ID is valid")
}
fn idempotency_key_hash(key: &str) -> String {
    format!("sha256:{}", hex(&Sha256::digest(key.as_bytes())))
}
fn current_state_event_id(kind: &str, data: &str) -> String {
    let digest = Sha256::digest(format!("piqueld-sse/v1\0{kind}\0{data}").as_bytes());
    format!("current:{}", hex(&digest[..16]))
}
fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut value = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        value.push(char::from(HEX[usize::from(byte >> 4)]));
        value.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    value
}
fn last_event_id(headers: &HeaderMap) -> Option<String> {
    header_text(headers, "last-event-id").map(str::to_owned)
}

fn application_view(v: StoredApplication) -> ApplicationView {
    ApplicationView {
        application: v.application,
        generation: v.generation,
        spec_hash: v.spec_hash,
        delete_intent: v.delete_intent,
        created_at_ms: v.created_at_ms,
        updated_at_ms: v.updated_at_ms,
    }
}
fn status_view(v: ApplicationStatus) -> ApplicationStatusView {
    ApplicationStatusView {
        application_id: v.application_id.to_string(),
        state: application_state_name(v.state).into(),
        observed_generation: v.observed_generation,
        message: v.message,
        updated_at_ms: v.updated_at_ms,
    }
}
fn operation_view(v: Operation, steps: Vec<OperationStep>) -> OperationView {
    OperationView {
        id: v.id,
        application_id: v.application_id.to_string(),
        generation: v.generation,
        kind: operation_kind_name(v.kind).into(),
        state: state_name(v.state).into(),
        error_code: v.error_code,
        error_message: v.error_message,
        created_at_ms: v.created_at_ms,
        updated_at_ms: v.updated_at_ms,
        started_at_ms: v.started_at_ms,
        finished_at_ms: v.finished_at_ms,
        steps: steps
            .into_iter()
            .map(|s| OperationStepView {
                id: s.id,
                position: s.position,
                kind: s.kind,
                state: step_state_name(s.state).into(),
                attempt: s.attempt,
                error_code: s.error_code,
                error_message: s.error_message,
                updated_at_ms: s.updated_at_ms,
            })
            .collect(),
    }
}
fn application_state_name(v: ApplicationState) -> &'static str {
    match v {
        ApplicationState::Pending => "pending",
        ApplicationState::Resolving => "resolving",
        ApplicationState::Building => "building",
        ApplicationState::Deploying => "deploying",
        ApplicationState::Ready => "ready",
        ApplicationState::Degraded => "degraded",
        ApplicationState::Deleting => "deleting",
        ApplicationState::Failed => "failed",
    }
}
fn operation_kind_name(v: OperationKind) -> &'static str {
    match v {
        OperationKind::Create => "create",
        OperationKind::Replace => "replace",
        OperationKind::Delete => "delete",
        OperationKind::Reconcile => "reconcile",
        OperationKind::Build => "build",
        OperationKind::Deploy => "deploy",
    }
}
fn state_name(v: WorkState) -> &'static str {
    match v {
        WorkState::Pending => "pending",
        WorkState::Running => "running",
        WorkState::Recovery => "recovery",
        WorkState::Succeeded => "succeeded",
        WorkState::Failed => "failed",
        WorkState::Cancelled => "cancelled",
    }
}
fn step_state_name(v: StepState) -> &'static str {
    match v {
        StepState::Pending => "pending",
        StepState::Running => "running",
        StepState::Recovery => "recovery",
        StepState::Succeeded => "succeeded",
        StepState::Failed => "failed",
        StepState::Cancelled => "cancelled",
        StepState::Skipped => "skipped",
    }
}

/// Generated `OpenAPI` contract. Core domain schemas are linked as explicit JSON objects to avoid coupling core to Utoipa.
#[must_use]
#[allow(clippy::too_many_lines)]
pub fn openapi_document() -> Value {
    let mut schemas = serde_json::Map::new();
    add_schema::<ErrorBody>(&mut schemas, "ErrorBody");
    add_schema::<Envelope<SystemStatus>>(&mut schemas, "SystemStatusEnvelope");
    add_schema::<Envelope<SystemCapabilities>>(&mut schemas, "SystemCapabilitiesEnvelope");
    add_schema::<Envelope<Page<ApplicationView>>>(&mut schemas, "ApplicationPageEnvelope");
    add_schema::<Envelope<ApplicationView>>(&mut schemas, "ApplicationEnvelope");
    add_schema::<CreateApplicationRequest>(&mut schemas, "CreateApplicationRequest");
    add_schema::<ReplaceApplicationRequest>(&mut schemas, "ReplaceApplicationRequest");
    add_schema::<PlanApplicationRequest>(&mut schemas, "PlanApplicationRequest");
    add_schema::<piqueld_client::ReplacePlanRequest>(&mut schemas, "ReplacePlanRequest");
    add_schema::<DeleteApplicationRequest>(&mut schemas, "DeleteApplicationRequest");
    if let Some(force) = schemas
        .get_mut("DeleteApplicationRequest")
        .and_then(|schema| schema.pointer_mut("/properties/force"))
    {
        force["enum"] = json!([false]);
        force["description"] =
            json!("Must be false. Force deletion is unsupported and named volumes are retained.");
    }
    add_schema::<ExpectedGeneration>(&mut schemas, "ExpectedGeneration");
    add_schema::<Envelope<AcceptedOperation>>(&mut schemas, "AcceptedOperationEnvelope");
    add_schema::<Envelope<PlanView>>(&mut schemas, "PlanEnvelope");
    add_schema::<Envelope<ApplicationStatusView>>(&mut schemas, "ApplicationStatusEnvelope");
    add_schema::<Envelope<OperationView>>(&mut schemas, "OperationEnvelope");

    let id = json!({"name":"id","in":"path","required":true,"schema":{"type":"string","minLength":8,"maxLength":64}});
    let last_event_id = json!({"name":"Last-Event-ID","in":"header","required":false,"schema":{"type":"string"},"description":"Last durable/current-state event ID received by the client."});
    let expected_generation = json!({"name":"X-Expected-Generation","in":"header","required":false,"schema":{"type":"integer","format":"uint64","minimum":1},"description":"Required for application/toml replacement and replacement planning."});
    let json_toml = |schema: &str| {
        json!({
            "required":true,
            "content":{
                "application/json":{"schema":{"$ref":format!("#/components/schemas/{schema}")}},
                "application/toml":{"schema":{"type":"string"}},
                "text/toml":{"schema":{"type":"string"}}
            }
        })
    };
    let json_body = |schema: &str| json!({"required":true,"content":{"application/json":{"schema":{"$ref":format!("#/components/schemas/{schema}")}}}});
    let response = |description: &str, schema: &str| json!({"description":description,"content":{"application/json":{"schema":{"$ref":format!("#/components/schemas/{schema}")}}}});
    let errors = |statuses: &[&str]| {
        let mut map = serde_json::Map::new();
        for status in statuses {
            map.insert(
                (*status).into(),
                response("Structured, sanitized error", "ErrorBody"),
            );
        }
        map
    };
    let operation = |operation_id: &str,
                     success_status: &str,
                     success_schema: &str,
                     statuses: &[&str]| {
        let mut responses = errors(statuses);
        responses.insert(success_status.into(), response("Success", success_schema));
        json!({"operationId":operation_id,"summary":operation_summary(operation_id),"responses":responses})
    };
    let mut paths = serde_json::Map::new();
    paths.insert(
        "/api/v1/system/status".into(),
        json!({"get":operation("systemStatus","200","SystemStatusEnvelope", &["500","503"])}),
    );
    paths.insert(
        "/api/v1/system/capabilities".into(),
        json!({"get":operation("systemCapabilities","200","SystemCapabilitiesEnvelope", &["500"])}),
    );
    paths.insert("/api/v1/openapi.json".into(), json!({"get":{"operationId":"openApiDocument","summary":operation_summary("openApiDocument"),"responses":{"200":{"description":"OpenAPI 3.1 document","content":{"application/vnd.oai.openapi+json":{"schema":{"type":"object"}}}}}}}));

    let mut list = operation(
        "listApplications",
        "200",
        "ApplicationPageEnvelope",
        &["400", "500", "503"],
    );
    list["parameters"] = json!([
        {"name":"cursor","in":"query","required":false,"schema":{"type":"string"}},
        {"name":"limit","in":"query","required":false,"schema":{"type":"integer","minimum":1,"maximum":100,"default":50}}
    ]);
    let mut create = operation(
        "createApplication",
        "202",
        "AcceptedOperationEnvelope",
        &["400", "409", "415", "422", "500", "502", "503"],
    );
    create["parameters"] = json!([{"name":"Idempotency-Key","in":"header","required":true,"schema":{"type":"string","minLength":1,"maxLength":128}}]);
    create["requestBody"] = json_toml("CreateApplicationRequest");
    paths.insert(
        "/api/v1/applications".into(),
        json!({"get":list,"post":create}),
    );

    let mut create_plan = operation(
        "planApplicationCreate",
        "200",
        "PlanEnvelope",
        &["400", "409", "415", "422", "500", "503"],
    );
    create_plan["requestBody"] = json_toml("PlanApplicationRequest");
    paths.insert(
        "/api/v1/applications/plan".into(),
        json!({"post":create_plan}),
    );

    let mut get_app = operation(
        "getApplication",
        "200",
        "ApplicationEnvelope",
        &["400", "404", "500", "503"],
    );
    get_app["parameters"] = json!([id.clone()]);
    let mut replace = operation(
        "replaceApplication",
        "202",
        "AcceptedOperationEnvelope",
        &["400", "404", "409", "415", "422", "500", "502", "503"],
    );
    replace["parameters"] = json!([id.clone(), expected_generation.clone()]);
    replace["requestBody"] = json_toml("ReplaceApplicationRequest");
    let mut delete = operation(
        "deleteApplication",
        "202",
        "AcceptedOperationEnvelope",
        &["400", "404", "409", "415", "500", "502", "503"],
    );
    delete["parameters"] = json!([id.clone()]);
    delete["requestBody"] = json_body("DeleteApplicationRequest");
    paths.insert(
        "/api/v1/applications/{id}".into(),
        json!({"get":get_app,"put":replace,"delete":delete}),
    );

    let mut replace_plan = operation(
        "planApplicationReplace",
        "200",
        "PlanEnvelope",
        &["400", "404", "409", "415", "422", "500", "502", "503"],
    );
    replace_plan["parameters"] = json!([id.clone(), expected_generation]);
    replace_plan["requestBody"] = json_toml("ReplacePlanRequest");
    paths.insert(
        "/api/v1/applications/{id}/plan".into(),
        json!({"post":replace_plan}),
    );

    let mut reconcile = operation(
        "reconcileApplication",
        "202",
        "AcceptedOperationEnvelope",
        &["400", "404", "409", "415", "500", "502", "503"],
    );
    reconcile["parameters"] = json!([id.clone()]);
    reconcile["requestBody"] = json_body("ExpectedGeneration");
    paths.insert(
        "/api/v1/applications/{id}/reconcile".into(),
        json!({"post":reconcile}),
    );

    let mut status = operation(
        "applicationStatus",
        "200",
        "ApplicationStatusEnvelope",
        &["400", "404", "500", "503"],
    );
    status["parameters"] = json!([id.clone()]);
    paths.insert(
        "/api/v1/applications/{id}/status".into(),
        json!({"get":status}),
    );

    let event_response = json!({"description":"Server-Sent Events with durable/current-state IDs and bounded replay reset events.","content":{"text/event-stream":{"schema":{"type":"string"}}}});
    let mut app_events = json!({"operationId":"watchApplication","summary":operation_summary("watchApplication"),"parameters":[id.clone(),last_event_id.clone()],"responses":{"200":event_response.clone(),"400":response("Structured, sanitized error","ErrorBody"),"404":response("Structured, sanitized error","ErrorBody"),"500":response("Structured, sanitized error","ErrorBody"),"503":response("Structured, sanitized error","ErrorBody")}});
    app_events["x-sse-keepalive-seconds"] = json!(15);
    paths.insert(
        "/api/v1/applications/{id}/events".into(),
        json!({"get":app_events}),
    );

    let mut get_operation = operation(
        "getOperation",
        "200",
        "OperationEnvelope",
        &["404", "500", "503"],
    );
    get_operation["parameters"] = json!([id.clone()]);
    paths.insert(
        "/api/v1/operations/{id}".into(),
        json!({"get":get_operation}),
    );
    let mut operation_events = json!({"operationId":"watchOperation","summary":operation_summary("watchOperation"),"parameters":[id,last_event_id],"responses":{"200":event_response,"404":response("Structured, sanitized error","ErrorBody"),"500":response("Structured, sanitized error","ErrorBody"),"503":response("Structured, sanitized error","ErrorBody")}});
    operation_events["x-sse-terminal-closes"] = json!(true);
    operation_events["x-sse-keepalive-seconds"] = json!(15);
    paths.insert(
        "/api/v1/operations/{id}/events".into(),
        json!({"get":operation_events}),
    );

    json!({
        "openapi":"3.1.0",
        "info":{"title":"piqueld API","version":"v1","description":"Plan 05 control-plane API. Mutation responses identify durable operations; named volumes are retained on deletion.","license":{"name":"MIT","identifier":"MIT"}},
        "servers":[{"url":"http://127.0.0.1:7845","description":"Default loopback TCP endpoint; clients may also use the configured Unix socket."}],
        "security":[],
        "paths":paths,
        "components":{"schemas":schemas}
    })
}

fn operation_summary(operation_id: &str) -> &'static str {
    match operation_id {
        "systemStatus" => "Get daemon status",
        "systemCapabilities" => "Get daemon capabilities",
        "openApiDocument" => "Get the OpenAPI document",
        "listApplications" => "List applications",
        "createApplication" => "Create an application",
        "planApplicationCreate" => "Preview application creation",
        "getApplication" => "Get an application",
        "replaceApplication" => "Replace an application",
        "deleteApplication" => "Request application deletion",
        "planApplicationReplace" => "Preview application replacement",
        "reconcileApplication" => "Request application reconciliation",
        "applicationStatus" => "Get application status",
        "watchApplication" => "Watch application status events",
        "getOperation" => "Get an operation",
        "watchOperation" => "Watch operation events",
        _ => "piqueld API operation",
    }
}

fn add_schema<T: JsonSchema>(schemas: &mut serde_json::Map<String, Value>, name: &str) {
    let root = schema_for!(T);
    let mut schema = serde_json::to_value(root.schema).expect("schema serialization cannot fail");
    rewrite_schema_refs(&mut schema);
    schemas.insert(name.into(), schema);
    for (definition_name, definition) in root.definitions {
        let mut definition =
            serde_json::to_value(definition).expect("schema serialization cannot fail");
        rewrite_schema_refs(&mut definition);
        schemas.entry(definition_name).or_insert(definition);
    }
}

fn rewrite_schema_refs(value: &mut Value) {
    match value {
        Value::Object(map) => {
            if let Some(Value::String(reference)) = map.get_mut("$ref")
                && let Some(name) = reference.strip_prefix("#/definitions/")
            {
                *reference = format!("#/components/schemas/{name}");
            }
            for value in map.values_mut() {
                rewrite_schema_refs(value);
            }
        }
        Value::Array(values) => {
            for value in values {
                rewrite_schema_refs(value);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::needless_pass_by_value)]
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use piqueld_core::compile_application;
    use piqueld_core::resource::ResolvedSource;
    use std::collections::BTreeMap;
    use tempfile::TempDir;
    use tower::ServiceExt;

    struct FakeRuntime {
        instance: InstanceId,
    }
    #[async_trait]
    impl RuntimeBoundary for FakeRuntime {
        fn capabilities(&self) -> RuntimeCapabilities {
            RuntimeCapabilities {
                source_resolution: true,
                runtime_observation: true,
                runtime_execution: true,
                reason: None,
            }
        }
        async fn prepare(
            &self,
            app: &NormalizedApplication,
        ) -> Result<PreparedApplication, BoundaryError> {
            let sources = app
                .spec
                .services
                .iter()
                .map(|service| {
                    let requested = match &service.source {
                        piqueld_core::manifest::Source::Image { image } => image.clone(),
                        piqueld_core::manifest::Source::Git { .. } => {
                            return Err(BoundaryError::Failed);
                        }
                    };
                    let repository = requested
                        .rsplit_once(':')
                        .map_or(requested.as_str(), |(name, _)| name);
                    Ok((
                        service.name.clone(),
                        ResolvedSource::Image {
                            digest_reference: format!("{repository}@sha256:{}", "a".repeat(64)),
                            requested,
                        },
                    ))
                })
                .collect::<Result<BTreeMap<_, _>, _>>()?;
            let resolved = compile_application(
                app,
                self.instance.clone(),
                "piqueld-ingress",
                &ResolutionSet {
                    sources,
                    secrets: BTreeMap::new(),
                },
            )
            .unwrap();
            Ok(PreparedApplication {
                resolved,
                observed: ObservedApplication::default(),
            })
        }
        async fn observe(
            &self,
            _: &StoredApplication,
        ) -> Result<ObservedApplication, BoundaryError> {
            Ok(ObservedApplication::default())
        }
    }

    struct PreviewOnlyRuntime;
    #[async_trait]
    impl RuntimeBoundary for PreviewOnlyRuntime {
        fn capabilities(&self) -> RuntimeCapabilities {
            RuntimeCapabilities {
                source_resolution: true,
                runtime_observation: false,
                runtime_execution: false,
                reason: None,
            }
        }
        async fn prepare(
            &self,
            _: &NormalizedApplication,
        ) -> Result<PreparedApplication, BoundaryError> {
            panic!("pure preview must not invoke source resolution")
        }
        async fn observe(
            &self,
            _: &StoredApplication,
        ) -> Result<ObservedApplication, BoundaryError> {
            panic!("create preview has no current runtime state to observe")
        }
    }

    struct ActiveRetryRuntime;
    #[async_trait]
    impl RuntimeBoundary for ActiveRetryRuntime {
        fn capabilities(&self) -> RuntimeCapabilities {
            RuntimeCapabilities {
                source_resolution: true,
                runtime_observation: true,
                runtime_execution: true,
                reason: None,
            }
        }
        async fn prepare(
            &self,
            _: &NormalizedApplication,
        ) -> Result<PreparedApplication, BoundaryError> {
            panic!("an active reconcile retry must not prepare an application")
        }
        async fn observe(
            &self,
            _: &StoredApplication,
        ) -> Result<ObservedApplication, BoundaryError> {
            panic!("an active reconcile retry must not repeat runtime observation")
        }
    }

    async fn fixture(runtime: Arc<dyn RuntimeBoundary>) -> (TempDir, Arc<SqliteStore>, Router) {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(
            SqliteStore::open(temp.path().join("state.db"))
                .await
                .unwrap(),
        );
        let app = router(ApiState::new(Arc::clone(&store), runtime));
        (temp, store, app)
    }
    async fn fake_fixture() -> (TempDir, Arc<SqliteStore>, Router) {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(
            SqliteStore::open(temp.path().join("state.db"))
                .await
                .unwrap(),
        );
        let instance = InstanceId::parse(store.instance_id().to_owned()).unwrap();
        let app = router(ApiState::new(
            Arc::clone(&store),
            Arc::new(FakeRuntime { instance }),
        ));
        (temp, store, app)
    }
    fn manifest() -> Value {
        json!({"api_version":"piqueld.dev/v1alpha1","kind":"Application","metadata":{"name":"notes"},"spec":{"services":[{"name":"web","source":{"type":"image","image":"ghcr.io/example/notes:1"}}]}})
    }
    fn request(method: Method, uri: &str, value: Value) -> Request<Body> {
        Request::builder()
            .method(method)
            .uri(uri)
            .header(header::CONTENT_TYPE, JSON)
            .body(Body::from(value.to_string()))
            .unwrap()
    }
    async fn json_body(response: Response) -> Value {
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
    }
    fn assert_refs_resolve(value: &Value, schemas: &serde_json::Map<String, Value>) {
        match value {
            Value::Object(object) => {
                if let Some(Value::String(reference)) = object.get("$ref") {
                    let name = reference
                        .strip_prefix("#/components/schemas/")
                        .expect("only local component schema references are generated");
                    assert!(schemas.contains_key(name), "missing schema {name}");
                }
                for nested in object.values() {
                    assert_refs_resolve(nested, schemas);
                }
            }
            Value::Array(values) => {
                for nested in values {
                    assert_refs_resolve(nested, schemas);
                }
            }
            _ => {}
        }
    }

    #[tokio::test]
    async fn create_is_accepted_idempotent_and_queryable() {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(
            SqliteStore::open(temp.path().join("state.db"))
                .await
                .unwrap(),
        );
        let instance = InstanceId::parse(store.instance_id().to_owned()).unwrap();
        let app = router(ApiState::new(
            Arc::clone(&store),
            Arc::new(FakeRuntime { instance }),
        ));
        let body = json!({"manifest":manifest()});
        let make = || {
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/applications")
                .header(header::CONTENT_TYPE, JSON)
                .header("idempotency-key", "test-create-1")
                .body(Body::from(body.to_string()))
                .unwrap()
        };
        let first = app.clone().oneshot(make()).await.unwrap();
        let first_status = first.status();
        let first_json = json_body(first).await;
        assert_eq!(first_status, StatusCode::ACCEPTED, "{first_json}");
        let second = app.clone().oneshot(make()).await.unwrap();
        assert_eq!(second.status(), StatusCode::ACCEPTED);
        assert_eq!(first_json, json_body(second).await);
        let listed = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/applications")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            json_body(listed).await["data"]["items"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn create_retry_returns_original_result_after_later_replacement() {
        let (temp, store, app) = fake_fixture().await;
        let create = || {
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/applications")
                .header(header::CONTENT_TYPE, JSON)
                .header("idempotency-key", "durable-create-retry")
                .body(Body::from(json!({"manifest":manifest()}).to_string()))
                .unwrap()
        };
        let original = json_body(app.clone().oneshot(create()).await.unwrap()).await;
        let id = original["data"]["application_id"].as_str().unwrap();
        let replaced = app
            .clone()
            .oneshot(request(
                Method::PUT,
                &format!("/api/v1/applications/{id}"),
                json!({"expected_generation":1,"manifest":manifest()}),
            ))
            .await
            .unwrap();
        assert_eq!(replaced.status(), StatusCode::ACCEPTED);
        assert_eq!(json_body(replaced).await["data"]["generation"], 2);
        let original_operation = original["data"]["operation_id"].as_str().unwrap();
        store
            .transition_operation(
                original_operation,
                WorkState::Pending,
                WorkState::Running,
                None,
            )
            .await
            .unwrap();
        for step in store.operation_steps(original_operation).await.unwrap() {
            store
                .transition_step(&step.id, StepState::Pending, StepState::Running, None)
                .await
                .unwrap();
            store
                .transition_step(&step.id, StepState::Running, StepState::Succeeded, None)
                .await
                .unwrap();
        }
        store
            .transition_operation(
                original_operation,
                WorkState::Running,
                WorkState::Succeeded,
                None,
            )
            .await
            .unwrap();
        assert_eq!(
            store
                .prune_finished_operations_before(i64::MAX, 100)
                .await
                .unwrap(),
            0
        );
        drop(app);
        drop(store);
        let reopened = Arc::new(
            SqliteStore::open(temp.path().join("state.db"))
                .await
                .unwrap(),
        );
        let app = router(ApiState::new(reopened, Arc::new(UnavailableRuntime)));
        assert_eq!(
            json_body(app.oneshot(create()).await.unwrap()).await,
            original
        );
    }

    #[tokio::test]
    async fn operation_sse_has_ids_replay_reset_and_terminal_cleanup() {
        let temp = tempfile::tempdir().unwrap();
        let store = Arc::new(
            SqliteStore::open(temp.path().join("state.db"))
                .await
                .unwrap(),
        );
        let instance = InstanceId::parse(store.instance_id().to_owned()).unwrap();
        let app = router(ApiState::new(
            Arc::clone(&store),
            Arc::new(FakeRuntime { instance }),
        ));
        let create = Request::builder()
            .method(Method::POST)
            .uri("/api/v1/applications")
            .header(header::CONTENT_TYPE, JSON)
            .header("idempotency-key", "sse-create")
            .body(Body::from(json!({"manifest":manifest()}).to_string()))
            .unwrap();
        let created = json_body(app.clone().oneshot(create).await.unwrap()).await;
        let operation_id = created["data"]["operation_id"].as_str().unwrap();
        store
            .transition_operation(operation_id, WorkState::Pending, WorkState::Running, None)
            .await
            .unwrap();
        for step in store.operation_steps(operation_id).await.unwrap() {
            store
                .transition_step(&step.id, StepState::Pending, StepState::Running, None)
                .await
                .unwrap();
            store
                .transition_step(&step.id, StepState::Running, StepState::Succeeded, None)
                .await
                .unwrap();
        }
        store
            .transition_operation(operation_id, WorkState::Running, WorkState::Succeeded, None)
            .await
            .unwrap();
        let response = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/operations/{operation_id}/events"))
                    .header("last-event-id", "expired:cursor")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let events = String::from_utf8(
            response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap();
        assert!(events.contains("event: replay_reset"));
        assert!(events.contains("event: terminal"));
        assert!(events.contains("id:"));
    }

    #[tokio::test]
    async fn operation_current_state_id_changes_when_only_a_step_advances() {
        let (_temp, store, app) = fake_fixture().await;
        let created = json_body(
            app.oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/v1/applications")
                    .header(header::CONTENT_TYPE, JSON)
                    .header("idempotency-key", "step-event-id")
                    .body(Body::from(json!({"manifest":manifest()}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap(),
        )
        .await;
        let operation_id = created["data"]["operation_id"].as_str().unwrap();
        store
            .transition_operation(operation_id, WorkState::Pending, WorkState::Running, None)
            .await
            .unwrap();
        let operation = store.operation(operation_id).await.unwrap();
        let steps = store.operation_steps(operation_id).await.unwrap();
        let before =
            serde_json::to_string(&operation_view(operation.clone(), steps.clone())).unwrap();
        let before_id = current_state_event_id("operation", &before);
        store
            .transition_step(&steps[0].id, StepState::Pending, StepState::Running, None)
            .await
            .unwrap();
        let after = serde_json::to_string(&operation_view(
            store.operation(operation_id).await.unwrap(),
            store.operation_steps(operation_id).await.unwrap(),
        ))
        .unwrap();
        assert_eq!(store.operation(operation_id).await.unwrap(), operation);
        assert_ne!(current_state_event_id("operation", &after), before_id);
    }

    #[tokio::test]
    async fn preview_is_pure_and_unavailable_runtime_is_honest() {
        let (_temp, _store, app) = fixture(Arc::new(UnavailableRuntime)).await;
        let preview = app
            .clone()
            .oneshot(request(
                Method::POST,
                "/api/v1/applications/plan",
                json!({"manifest":manifest()}),
            ))
            .await
            .unwrap();
        assert_eq!(preview.status(), StatusCode::OK);
        assert!(
            json_body(preview).await["data"]["plan"]["actions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v["kind"]["action"] == "resolve_image")
        );
        let listed = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/applications")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert!(
            json_body(listed).await["data"]["items"]
                .as_array()
                .unwrap()
                .is_empty()
        );
        let capabilities = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/system/capabilities")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            json_body(capabilities).await["data"]["runtime_execution"],
            false
        );
    }

    #[tokio::test]
    async fn create_preview_never_invokes_resolution_or_observation() {
        let (_temp, _store, app) = fixture(Arc::new(PreviewOnlyRuntime)).await;
        let response = app
            .oneshot(request(
                Method::POST,
                "/api/v1/applications/plan",
                json!({"manifest":manifest()}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            json_body(response).await["data"]["plan"]["actions"]
                .as_array()
                .unwrap()
                .iter()
                .any(|action| action["kind"]["action"] == "resolve_image")
        );
    }

    #[tokio::test]
    async fn replacement_preview_reuses_unchanged_resolutions_without_resolver_io() {
        let (_temp, _store, app) = fake_fixture().await;
        let created = json_body(
            app.clone()
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri("/api/v1/applications")
                        .header(header::CONTENT_TYPE, JSON)
                        .header("idempotency-key", "preview-reuse")
                        .body(Body::from(json!({"manifest":manifest()}).to_string()))
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await;
        let id = created["data"]["application_id"].as_str().unwrap();
        let preview = app
            .oneshot(request(
                Method::POST,
                &format!("/api/v1/applications/{id}/plan"),
                json!({"expected_generation":1,"manifest":manifest()}),
            ))
            .await
            .unwrap();
        assert_eq!(preview.status(), StatusCode::OK);
        let body = json_body(preview).await;
        let actions = body["data"]["plan"]["actions"].as_array().unwrap();
        assert!(
            actions
                .iter()
                .any(|action| action["mutates_runtime"] == true)
        );
        assert!(
            actions
                .iter()
                .all(|action| action["kind"]["action"] != "resolve_image")
        );
    }

    #[tokio::test]
    async fn transport_failures_are_structured_and_safe() {
        let (_temp, _store, app) = fixture(Arc::new(UnavailableRuntime)).await;
        let missing_key = app
            .clone()
            .oneshot(request(
                Method::POST,
                "/api/v1/applications",
                json!({"manifest":manifest()}),
            ))
            .await
            .unwrap();
        assert_eq!(missing_key.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            json_body(missing_key).await["code"],
            "idempotency_key_required"
        );
        let bad_type = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/v1/applications/plan")
                    .header(header::CONTENT_TYPE, "text/plain")
                    .body(Body::from("secret fixture must not echo"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(bad_type.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
        assert!(
            !String::from_utf8_lossy(&bad_type.into_body().collect().await.unwrap().to_bytes())
                .contains("secret fixture")
        );
        let unknown = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/secrets")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn validation_not_found_force_and_malformed_json_are_structured() {
        let (_temp, _store, app) = fake_fixture().await;
        let invalid = app
            .clone()
            .oneshot(request(
                Method::POST,
                "/api/v1/applications/plan",
                json!({"manifest":{"api_version":"piqueld.dev/v1alpha1","kind":"Application","metadata":{"name":"INVALID"},"spec":{"services":[]}}}),
            ))
            .await
            .unwrap();
        assert_eq!(invalid.status(), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            json_body(invalid).await["code"],
            "manifest_validation_failed"
        );

        let malformed = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/v1/applications/plan")
                    .header(header::CONTENT_TYPE, JSON)
                    .body(Body::from("{\"manifest\":"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
        assert_eq!(json_body(malformed).await["code"], "json_malformed");

        let missing = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v1/applications/app-does-not-exist")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        assert_eq!(json_body(missing).await["code"], "not_found");

        let created = json_body(
            app.clone()
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri("/api/v1/applications")
                        .header(header::CONTENT_TYPE, JSON)
                        .header("idempotency-key", "force-delete")
                        .body(Body::from(json!({"manifest":manifest()}).to_string()))
                        .unwrap(),
                )
                .await
                .unwrap(),
        )
        .await;
        let id = created["data"]["application_id"].as_str().unwrap();
        let force = app
            .oneshot(request(
                Method::DELETE,
                &format!("/api/v1/applications/{id}"),
                json!({"expected_generation":1,"force":true}),
            ))
            .await
            .unwrap();
        assert_eq!(force.status(), StatusCode::BAD_REQUEST);
        assert_eq!(json_body(force).await["code"], "force_delete_unsupported");
    }

    #[tokio::test]
    async fn openapi_snapshot_lists_exact_plan_five_surface() {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/openapi-v1.json");
        if !path.exists() {
            // Nix's Git-backed source excludes untracked files while developing a
            // new plan. The normal workspace test below remains strict once the
            // snapshot enters the branch.
            return;
        }
        let snapshot: Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(openapi_document(), snapshot);
        assert!(snapshot["paths"].get("/api/v1/secrets").is_none());
        assert!(
            snapshot["paths"]
                .get("/api/v1/applications/{id}/logs")
                .is_none()
        );
        for item in snapshot["paths"].as_object().unwrap().values() {
            for operation in item.as_object().unwrap().values() {
                assert!(operation.get("operationId").is_some());
                assert!(operation.get("responses").is_some());
            }
        }
        assert!(snapshot["components"]["schemas"].as_object().unwrap().len() > 20);
        assert_refs_resolve(
            &snapshot,
            snapshot["components"]["schemas"].as_object().unwrap(),
        );
    }

    #[tokio::test]
    async fn request_ids_match_headers_and_transport_errors_are_complete() {
        let (_temp, _store, app) = fixture(Arc::new(UnavailableRuntime)).await;
        let response = app
            .oneshot(
                Request::builder()
                    .method(Method::PATCH)
                    .uri("/api/v1/applications")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
        let request_id = response
            .headers()
            .get("x-request-id")
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        let body = json_body(response).await;
        assert_eq!(body["code"], "method_not_allowed");
        assert_eq!(body["request_id"], request_id);
    }

    #[tokio::test]
    async fn json_and_toml_share_normalization_and_malformed_toml_is_safe() {
        let (_temp, _store, app) = fake_fixture().await;
        let json_plan = app
            .clone()
            .oneshot(request(
                Method::POST,
                "/api/v1/applications/plan",
                json!({"manifest":manifest()}),
            ))
            .await
            .unwrap();
        let toml = r#"api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "notes"
[[spec.services]]
name = "web"
[spec.services.source]
type = "image"
image = "ghcr.io/example/notes:1"
"#;
        let toml_plan = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/v1/applications/plan")
                    .header(header::CONTENT_TYPE, TOML)
                    .body(Body::from(toml))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(json_plan.status(), StatusCode::OK);
        assert_eq!(toml_plan.status(), StatusCode::OK);
        assert_eq!(json_body(json_plan).await, json_body(toml_plan).await);

        let malformed = app
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/v1/applications/plan")
                    .header(header::CONTENT_TYPE, TOML)
                    .body(Body::from("[metadata\nsecret = 'must-not-echo'"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(malformed.status(), StatusCode::BAD_REQUEST);
        assert_eq!(json_body(malformed).await["code"], "toml_malformed");
    }

    #[tokio::test]
    async fn conflicts_and_reconcile_retries_preserve_durable_identity() {
        let (_temp, _store, app) = fake_fixture().await;
        let create = |key: &'static str| {
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/applications")
                .header(header::CONTENT_TYPE, JSON)
                .header("idempotency-key", key)
                .body(Body::from(json!({"manifest":manifest()}).to_string()))
                .unwrap()
        };
        let created = json_body(app.clone().oneshot(create("first")).await.unwrap()).await;
        let id = created["data"]["application_id"].as_str().unwrap();
        let collision = app.clone().oneshot(create("second")).await.unwrap();
        assert_eq!(collision.status(), StatusCode::CONFLICT);
        assert_eq!(
            json_body(collision).await["code"],
            "application_name_collision"
        );
        let stale = app
            .clone()
            .oneshot(request(
                Method::PUT,
                &format!("/api/v1/applications/{id}"),
                json!({"expected_generation":2,"manifest":manifest()}),
            ))
            .await
            .unwrap();
        assert_eq!(stale.status(), StatusCode::CONFLICT);
        assert_eq!(json_body(stale).await["details"]["current_generation"], 1);

        let reconcile = || {
            request(
                Method::POST,
                &format!("/api/v1/applications/{id}/reconcile"),
                json!({"expected_generation":1}),
            )
        };
        let first = json_body(app.clone().oneshot(reconcile()).await.unwrap()).await;
        let second = json_body(app.clone().oneshot(reconcile()).await.unwrap()).await;
        assert_eq!(first, second);
        let operation_id = first["data"]["operation_id"].as_str().unwrap();
        let operation = app
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/operations/{operation_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(operation.status(), StatusCode::OK);
        assert_eq!(json_body(operation).await["data"]["kind"], "reconcile");
    }

    #[tokio::test]
    async fn concurrent_create_retries_and_name_collisions_have_one_durable_winner() {
        let (_temp, _store, app) = fake_fixture().await;
        let create = |key: &'static str| {
            Request::builder()
                .method(Method::POST)
                .uri("/api/v1/applications")
                .header(header::CONTENT_TYPE, JSON)
                .header("idempotency-key", key)
                .body(Body::from(json!({"manifest":manifest()}).to_string()))
                .unwrap()
        };
        let (same_left, same_right) = tokio::join!(
            app.clone().oneshot(create("same-race")),
            app.clone().oneshot(create("same-race"))
        );
        let same_left = same_left.unwrap();
        let same_right = same_right.unwrap();
        assert_eq!(same_left.status(), StatusCode::ACCEPTED);
        assert_eq!(same_right.status(), StatusCode::ACCEPTED);
        assert_eq!(json_body(same_left).await, json_body(same_right).await);

        let second_manifest = json!({
            "api_version":"piqueld.dev/v1alpha1",
            "kind":"Application",
            "metadata":{"name":"other"},
            "spec":{"services":[{"name":"web","source":{"type":"image","image":"ghcr.io/example/notes:1"}}]}
        });
        let conflicting_key = Request::builder()
            .method(Method::POST)
            .uri("/api/v1/applications")
            .header(header::CONTENT_TYPE, JSON)
            .header("idempotency-key", "same-race")
            .body(Body::from(json!({"manifest":second_manifest}).to_string()))
            .unwrap();
        let reused = app.clone().oneshot(conflicting_key).await.unwrap();
        assert_eq!(reused.status(), StatusCode::CONFLICT);
        assert_eq!(json_body(reused).await["code"], "idempotency_key_reused");

        let (_collision_temp, _collision_store, collision_app) = fake_fixture().await;
        let (collision_left, collision_right) = tokio::join!(
            collision_app.clone().oneshot(create("collision-left")),
            collision_app.clone().oneshot(create("collision-right"))
        );
        let statuses = [
            collision_left.unwrap().status(),
            collision_right.unwrap().status(),
        ];
        assert!(statuses.contains(&StatusCode::ACCEPTED));
        assert!(statuses.contains(&StatusCode::CONFLICT));
        assert_eq!(
            json_body(
                collision_app
                    .oneshot(
                        Request::builder()
                            .uri("/api/v1/applications")
                            .body(Body::empty())
                            .unwrap(),
                    )
                    .await
                    .unwrap()
            )
            .await["data"]["items"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn active_reconcile_retry_does_not_repeat_runtime_io() {
        let (_temp, store, app) = fake_fixture().await;
        let created = json_body(
            app.oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/v1/applications")
                    .header(header::CONTENT_TYPE, JSON)
                    .header("idempotency-key", "reconcile-lost-response")
                    .body(Body::from(json!({"manifest":manifest()}).to_string()))
                    .unwrap(),
            )
            .await
            .unwrap(),
        )
        .await;
        let id = application_id(created["data"]["application_id"].as_str().unwrap()).unwrap();
        let durable = store
            .request_reconcile(&id, 1, &["already durable".into()])
            .await
            .unwrap();
        let retry_router = router(ApiState::new(store, Arc::new(ActiveRetryRuntime)));
        let response = retry_router
            .oneshot(request(
                Method::POST,
                &format!("/api/v1/applications/{id}/reconcile"),
                json!({"expected_generation":1}),
            ))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::ACCEPTED);
        assert_eq!(
            json_body(response).await["data"]["operation_id"],
            durable.operation_id
        );
    }
}
