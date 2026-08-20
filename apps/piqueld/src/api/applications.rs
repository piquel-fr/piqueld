use axum::{
    body::Bytes,
    extract::{
        Path, Query, State,
        rejection::{BytesRejection, QueryRejection},
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use piqueld_client::{
    AcceptedOperation, ApplicationStatusView, ApplicationView, CreateApplicationRequest,
    DeleteApplicationRequest, Envelope, ExpectedGeneration, Page, PlanApplicationRequest, PlanView,
    ReplaceApplicationRequest, ReplacePlanRequest,
};
use piqueld_core::{
    ApplicationId, InstanceId, NormalizedApplication, ObservedApplication, Plan, PlanAction,
    PlanRequest, ResolutionSet, compile_application, preview_resolution,
    resource::{ResolvedApplication, ResolvedSource},
};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use super::{
    ApiError, ApiState, BoundaryError, RequestShape, accepted, decode_json, generation,
    header_text, hex, idempotency_key_hash, idempotent_application_id, ok,
    openapi::ApiErrorResponse, parse_manifest, parse_update, require_json,
    valid_expected_generation,
};
use crate::store::{ApplicationStatus, DEFAULT_PAGE_SIZE, StoreError, StoredApplication};

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(super) struct ListQuery {
    cursor: Option<String>,
    #[param(minimum = 1, maximum = 100, default = 50)]
    limit: Option<usize>,
}

/// Lists applications using an optional pagination cursor and page size.
///
/// Invalid pagination parameters produce a `400 Bad Request` response.
///
/// # Examples
///
/// ```
/// #[test]
/// fn lists_applications_with_pagination() {
///     let uri = "/api/v1/applications?limit=20";
///     assert!(uri.contains("limit=20"));
/// }
/// ```
pub(super) async fn list(
    State(state): State<ApiState>,
    query: Result<Query<ListQuery>, QueryRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let Query(query) = query.map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "pagination_invalid",
            "pagination parameters are invalid",
        )
    })?;
    let limit = query.limit.unwrap_or(DEFAULT_PAGE_SIZE);
    let page = state
        .store
        .list(query.cursor.as_deref(), limit)
        .await
        .map_err(|error| match error {
            StoreError::InvalidInput | StoreError::InvalidInputSource(_) => ApiError::new(
                StatusCode::BAD_REQUEST,
                "pagination_invalid",
                "pagination parameters are invalid",
            ),
            error => error.into(),
        })?;
    Ok(ok(Page {
        items: page.items.into_iter().map(application_view).collect(),
        next_cursor: page.next_cursor,
    }))
}

#[utoipa::path(
get,
path = "/api/v1/applications/{id}",
operation_id = "getApplication",
summary = "Get an application",
params(("id" = String, Path, min_length = 8, max_length = 64)),
responses(
(status = 200, description = "Success", body = Envelope<ApplicationView>),
(status = 400, response = inline(ApiErrorResponse)),
(status = 404, response = inline(ApiErrorResponse)),
(status = 500, response = inline(ApiErrorResponse)),
(status = 503, response = inline(ApiErrorResponse)),
)
)]
/// Retrieves an application by its identifier.
///
/// # Examples
///
/// ```no_run
/// # async fn example() {
/// # let application_id = "application-id";
/// let response = get_application(application_id).await;
/// # }
/// ```
///
/// # Returns
///
/// The requested application view.
pub(super) async fn get(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let id = ApplicationId::parse(&id)?;
    Ok(ok(application_view(state.store.get(&id).await?)))
}

/// Creates an application from a validated manifest and schedules reconciliation.
///
/// An `Idempotency-Key` header is required. Repeated requests with the same key
/// return the previously accepted operation.
///
/// # Examples
///
/// ```
/// let idempotency_key = "create-example";
/// assert!(!idempotency_key.is_empty());
/// ```
///
/// Returns an accepted operation containing the operation ID, application ID,
/// and generation.
///
/// The request fails when the idempotency key, manifest, application name, or
/// runtime plan is invalid.
#[utoipa::path(
post,
path = "/api/v1/applications",
operation_id = "createApplication",
summary = "Create an application",
params(("Idempotency-Key" = String, Header, min_length = 1, max_length = 128)),
request_body(
content(
(CreateApplicationRequest = "application/json"),
(String = "application/toml"),
(String = "text/toml")
)
),
responses(
(status = 202, description = "Success", body = Envelope<AcceptedOperation>),
(status = 400, response = inline(ApiErrorResponse)),
(status = 409, response = inline(ApiErrorResponse)),
(status = 413, response = inline(ApiErrorResponse)),
(status = 415, response = inline(ApiErrorResponse)),
(status = 422, response = inline(ApiErrorResponse)),
(status = 500, response = inline(ApiErrorResponse)),
(status = 502, response = inline(ApiErrorResponse)),
(status = 503, response = inline(ApiErrorResponse)),
)
)]
pub(super) async fn create(
    State(state): State<ApiState>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let body = request_body(body)?;
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
    // preparation prevents concurrent retries from duplicating image-resolution
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
    let plan = Plan::from_request(
        &PlanRequest::Reconcile {
            desired: prepared.resolved.clone(),
        },
        &prepared.observed,
    );
    if plan.is_blocked() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "plan_blocked",
            "runtime plan contains blocking conflicts",
        )
        .details(json!({"diagnostics": plan.diagnostics})));
    }
    let steps = plan
        .actions
        .iter()
        .map(PlanAction::operation_step)
        .collect::<Vec<_>>();
    let mutation = state
        .store
        .create_idempotent(&app, &prepared.resolved, &steps, &key_hash, &request_hash)
        .await?;
    state.runtime.trigger_reconciliation();
    Ok(accepted(AcceptedOperation {
        operation_id: mutation.operation_id,
        application_id: id.to_string(),
        generation: mutation.generation,
    }))
}

/// Replaces an existing application and starts reconciliation for the new configuration.
///
/// The replacement must include the current expected generation. The operation is accepted
/// asynchronously and returns its operation identifier, application identifier, and new generation.
///
/// # Examples
///
/// ```ignore
/// let response = replace(state, Path::from("app-12345678"), headers, body).await?;
/// assert_eq!(response.status(), axum::http::StatusCode::ACCEPTED);
/// # Ok::<(), ApiError>(())
/// ```
pub(super) async fn replace(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let body = request_body(body)?;
    let id = ApplicationId::parse(&id)?;
    let (validated, expected) = parse_update(&headers, &body)?;
    let current = state.store.get(&id).await?;
    generation(expected, current.generation)?;
    let app = validated.normalize(id.clone());
    reject_name_collision(&state, &app, Some(&id)).await?;
    let prepared = state.runtime.prepare(&app).await?;
    let plan = Plan::from_request(
        &PlanRequest::Reconcile {
            desired: prepared.resolved.clone(),
        },
        &prepared.observed,
    );
    if plan.is_blocked() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "plan_blocked",
            "runtime plan contains blocking conflicts",
        ));
    }
    let steps = plan
        .actions
        .iter()
        .map(PlanAction::operation_step)
        .collect::<Vec<_>>();
    let mutation = state
        .store
        .replace(&app, &prepared.resolved, expected, &steps)
        .await?;
    state.runtime.trigger_reconciliation();
    Ok(accepted(AcceptedOperation {
        operation_id: mutation.operation_id,
        application_id: id.to_string(),
        generation: mutation.generation,
    }))
}

/// Requests deletion of an application after verifying its current generation.
///
/// # Examples
///
/// ```no_run
/// // Send a deletion request containing the application's current generation.
/// let request = DeleteApplicationRequest {
///     expected_generation: 1,
/// };
/// let _ = request;
/// ```
///
/// The deletion is accepted only when the runtime plan contains no blocking
/// conflicts.
///
/// Returns an accepted operation containing the application and operation IDs
/// and the resulting generation.
#[utoipa::path(
delete,
path = "/api/v1/applications/{id}",
operation_id = "deleteApplication",
summary = "Request application deletion",
params(("id" = String, Path, min_length = 8, max_length = 64)),
request_body = DeleteApplicationRequest,
responses(
(status = 202, description = "Success", body = Envelope<AcceptedOperation>),
(status = 400, response = inline(ApiErrorResponse)),
(status = 404, response = inline(ApiErrorResponse)),
(status = 409, response = inline(ApiErrorResponse)),
(status = 413, response = inline(ApiErrorResponse)),
(status = 415, response = inline(ApiErrorResponse)),
(status = 500, response = inline(ApiErrorResponse)),
(status = 502, response = inline(ApiErrorResponse)),
(status = 503, response = inline(ApiErrorResponse)),
)
)]
pub(super) async fn delete(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let body = request_body(body)?;
    require_json(&headers)?;
    let request: DeleteApplicationRequest = decode_json(&body)?;
    valid_expected_generation(request.expected_generation)?;
    let id = ApplicationId::parse(&id)?;
    let current = state.store.get(&id).await?;
    generation(request.expected_generation, current.generation)?;
    let observed = state.runtime.observe(&current).await?;
    let instance = InstanceId::parse(&state.instance_id).map_err(StoreError::corrupt)?;
    let plan = Plan::from_request(
        &PlanRequest::Delete {
            application_id: id.clone(),
            instance_id: instance,
        },
        &observed,
    );
    if plan.is_blocked() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "plan_blocked",
            "runtime plan contains blocking conflicts",
        )
        .details(json!({"diagnostics": plan.diagnostics})));
    }
    let steps = plan
        .actions
        .iter()
        .map(PlanAction::operation_step)
        .collect::<Vec<_>>();
    let mutation = state
        .store
        .request_delete(&id, request.expected_generation, &steps)
        .await?;
    state.runtime.trigger_reconciliation();
    Ok(accepted(AcceptedOperation {
        operation_id: mutation.operation_id,
        application_id: id.to_string(),
        generation: mutation.generation,
    }))
}

/// Previews the creation of an application from a manifest.
///
/// The preview includes the deterministic application ID, proposed initial generation,
/// and reconciliation plan without persisting the application.
///
/// # Examples
///
/// ```no_run
/// # async fn example(state: ApiState) {
/// let response = plan_create(
///     axum::extract::State(state),
///     axum::http::HeaderMap::new(),
///     Ok(bytes::Bytes::from_static(b"name = \"example\"")),
/// ).await;
///
/// assert!(response.is_ok());
/// # }
/// ```
#[utoipa::path(
post,
path = "/api/v1/applications/plan",
operation_id = "planApplicationCreate",
summary = "Preview application creation",
request_body(
content(
(PlanApplicationRequest = "application/json"),
(String = "application/toml"),
(String = "text/toml")
)
),
responses(
(status = 200, description = "Success", body = Envelope<PlanView>),
(status = 400, response = inline(ApiErrorResponse)),
(status = 409, response = inline(ApiErrorResponse)),
(status = 413, response = inline(ApiErrorResponse)),
(status = 415, response = inline(ApiErrorResponse)),
(status = 422, response = inline(ApiErrorResponse)),
(status = 500, response = inline(ApiErrorResponse)),
(status = 503, response = inline(ApiErrorResponse)),
)
)]
pub(super) async fn plan_create(
    State(state): State<ApiState>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let body = request_body(body)?;
    let validated = parse_manifest(&headers, &body, RequestShape::PlanCreate)?;
    let hash = Sha256::digest(validated.name().as_bytes());
    let id = ApplicationId::parse(format!("preview-{}", hex(&hash[..8])))
        .map_err(StoreError::corrupt)?;
    let app = validated.normalize(id.clone());
    reject_name_collision(&state, &app, None).await?;
    let plan = preview_plan(&state, &app, None).await?;
    Ok(ok(PlanView {
        application_id: id.to_string(),
        proposed_generation: 1,
        plan,
    }))
}

#[utoipa::path(
    post,
    path = "/api/v1/applications/{id}/plan",
    operation_id = "planApplicationReplace",
    summary = "Preview application replacement",
    params(
        ("id" = String, Path, min_length = 8, max_length = 64),
        ("X-Expected-Generation" = Option<u64>, Header, nullable = false, format = "uint64", minimum = 1, description = "Required for application/toml replacement and replacement planning."),
    ),
    request_body(
        content(
            (ReplacePlanRequest = "application/json"),
            (String = "application/toml"),
            (String = "text/toml")
        )
    ),
    responses(
        (status = 200, description = "Success", body = Envelope<PlanView>),
        (status = 400, response = inline(ApiErrorResponse)),
        (status = 404, response = inline(ApiErrorResponse)),
        (status = 409, response = inline(ApiErrorResponse)),
        (status = 413, response = inline(ApiErrorResponse)),
        (status = 415, response = inline(ApiErrorResponse)),
        (status = 422, response = inline(ApiErrorResponse)),
        (status = 500, response = inline(ApiErrorResponse)),
        (status = 502, response = inline(ApiErrorResponse)),
        (status = 503, response = inline(ApiErrorResponse)),
    )
)]
pub(super) async fn plan_replace(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let body = request_body(body)?;
    let id = ApplicationId::parse(&id)?;
    let (validated, expected) = parse_update(&headers, &body)?;
    let current = state.store.get(&id).await?;
    generation(expected, current.generation)?;
    let app = validated.normalize(id.clone());
    reject_name_collision(&state, &app, Some(&id)).await?;
    let plan = preview_plan(&state, &app, Some(&current)).await?;
    Ok(ok(PlanView {
        application_id: id.to_string(),
        proposed_generation: expected + 1,
        plan,
    }))
}

/// Builds a reconciliation plan preview for an application, optionally using the current stored application state.
///
/// Existing compatible resolutions are reused, while unresolved sources remain in the preview plan.
///
/// # Examples
///
/// ```ignore
/// let plan = preview_plan(&state, &application, current.as_ref()).await?;
/// ```
///
/// Returns the computed preview plan or an API error if runtime observation or application compilation fails.
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
    let resolutions = current.map_or_else(ResolutionSet::default, |stored| {
        reusable_resolutions(app, &stored.resolved)
    });
    let unresolved = preview_resolution(app, &resolutions);
    let desired = if unresolved.is_empty() {
        current
            .map(|stored| {
                compile_application(app, stored.resolved.instance_id.clone(), &resolutions)
                    .map_err(BoundaryError::Compilation)
            })
            .transpose()?
    } else {
        None
    };
    Ok(Plan::from_request(
        &PlanRequest::Preview {
            unresolved,
            desired,
        },
        &observed,
    ))
}

/// Reuses resolved image sources for services whose requested image is unchanged.
///
/// # Examples
///
/// ```ignore
/// let resolutions = reusable_resolutions(&application, &current);
/// assert!(resolutions.sources.contains_key("web"));
/// ```
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
            };
            reusable.then(|| (service.name.clone(), resolved.source.clone()))
        })
        .collect();
    ResolutionSet { sources }
}

/// Rejects the application when another application already uses its name.
///
/// `own_id` identifies the application being replaced, allowing that application
/// to retain its existing name.
///
/// # Examples
///
/// ```no_run
/// # async fn example(
/// #     state: &ApiState,
/// #     app: &NormalizedApplication,
/// # ) {
/// reject_name_collision(state, app, None).await.unwrap();
/// # }
/// ```
async fn reject_name_collision(
    state: &ApiState,
    app: &NormalizedApplication,
    own_id: Option<&ApplicationId>,
) -> Result<(), ApiError> {
    if state
        .store
        .find_by_name(&app.metadata.name)
        .await?
        .is_some_and(|stored| own_id != Some(&stored.application.id))
    {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "application_name_collision",
            "application name already exists",
        ));
    }
    Ok(())
}

/// Requests reconciliation of an application at the specified generation.
///
/// If reconciliation is already active for that generation, returns the existing
/// operation. Otherwise, evaluates the current runtime state and queues a new
/// reconciliation when the resulting plan contains no blocking conflicts.
///
/// # Examples
///
/// ```rust,ignore
/// let response = reconcile(state, Path::from("application-id"), headers, body).await?;
/// assert_eq!(response.status(), StatusCode::ACCEPTED);
/// ```
pub(super) async fn reconcile(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let body = request_body(body)?;
    require_json(&headers)?;
    let request: ExpectedGeneration = decode_json(&body)?;
    valid_expected_generation(request.expected_generation)?;
    let id = ApplicationId::parse(&id)?;
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
    let desired = current.resolved.clone();
    let observed = state.runtime.observe(&current).await?;
    let plan = Plan::from_request(&PlanRequest::Reconcile { desired }, &observed);
    if plan.is_blocked() {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "plan_blocked",
            "runtime plan contains blocking conflicts",
        )
        .details(json!({"diagnostics": plan.diagnostics})));
    }
    let steps = plan
        .actions
        .iter()
        .map(PlanAction::operation_step)
        .collect::<Vec<_>>();
    let mutation = state
        .store
        .request_reconcile(&id, request.expected_generation, &steps)
        .await?;
    state.runtime.trigger_reconciliation();
    Ok(accepted(AcceptedOperation {
        operation_id: mutation.operation_id,
        application_id: id.to_string(),
        generation: mutation.generation,
    }))
}

fn request_body(body: Result<Bytes, BytesRejection>) -> Result<Bytes, ApiError> {
    body.map_err(|rejection| {
        if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE {
            ApiError::new(
                StatusCode::PAYLOAD_TOO_LARGE,
                "request_body_too_large",
                "request body exceeds the maximum allowed size",
            )
        } else {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "request_body_unreadable",
                "request body could not be read",
            )
        }
    })
}

/// Retrieves the current status of an application.
///
/// # Parameters
///
/// * `id` - The application identifier.
///
/// # Returns
///
/// The application's status view.
///
/// # Examples
///
/// ```no_run
/// # async fn example(state: ApiState) {
/// let status = status(State(state), Path("application-id".to_owned())).await?;
/// # let _: _ = status;
/// # Ok::<(), ApiError>(())
/// # }
/// ```
#[utoipa::path(
get,
path = "/api/v1/applications/{id}/status",
operation_id = "applicationStatus",
summary = "Get application status",
params(("id" = String, Path, min_length = 8, max_length = 64)),
responses(
(status = 200, description = "Success", body = Envelope<ApplicationStatusView>),
(status = 400, response = inline(ApiErrorResponse)),
(status = 404, response = inline(ApiErrorResponse)),
(status = 500, response = inline(ApiErrorResponse)),
(status = 503, response = inline(ApiErrorResponse)),
)
)]
pub(super) async fn status(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let id = ApplicationId::parse(&id)?;
    Ok(ok(status_view(state.store.status(&id).await?)))
}

/// Converts a stored application record into its API view.
///
/// # Examples
///
/// ```
/// # let stored = todo!();
/// let view = application_view(stored);
/// ```
fn application_view(stored: StoredApplication) -> ApplicationView {
    ApplicationView {
        application: stored.application,
        generation: stored.generation,
        spec_hash: stored.spec_hash,
        delete_intent: stored.delete_intent,
        created_at_ms: stored.created_at_ms,
        updated_at_ms: stored.updated_at_ms,
    }
}

/// Converts a stored application status into its API response view.
///
/// # Examples
///
/// ```no_run
/// # let status: ApplicationStatus = todo!();
/// let view = status_view(status);
/// assert_eq!(view.application_id, status.application_id.to_string());
/// ```
fn status_view(status: ApplicationStatus) -> ApplicationStatusView {
    ApplicationStatusView {
        application_id: status.application_id.to_string(),
        state: status.state.as_str().into(),
        observed_generation: status.observed_generation,
        message: status.message,
        updated_at_ms: status.updated_at_ms,
    }
}
