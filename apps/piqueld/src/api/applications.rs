use std::time::Duration;

use axum::{
    body::Bytes,
    extract::{Path, Query, State},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response, Sse, sse::KeepAlive},
};
use piqueld_client::{
    AcceptedOperation, ApplicationStatusView, ApplicationView, CreateApplicationRequest,
    DeleteApplicationRequest, Envelope, ExpectedGeneration, Page, PlanApplicationRequest, PlanView,
    ReplaceApplicationRequest, ReplacePlanRequest,
};
use piqueld_core::{
    ApplicationId, InstanceId, NormalizedApplication, ObservedApplication, PlanAction, PlanRequest,
    ResolutionSet, compile_application, plan, preview_resolution,
    resource::{ResolvedApplication, ResolvedSource, SecretGeneration},
};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use super::{
    ApiError, ApiState, BoundaryError, RequestShape, StateEventSnapshot, accepted,
    current_state_stream, decode_json, generation, header_text, hex, idempotency_key_hash,
    idempotent_application_id, last_event_id, ok, openapi::ApiErrorResponse, parse_manifest,
    parse_update, require_json, valid_expected_generation,
};
use crate::store::{
    ApplicationRepository, DEFAULT_PAGE_SIZE, StatusRepository, StoreError, StoredApplication,
};

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(super) struct ListQuery {
    cursor: Option<String>,
    #[param(minimum = 1, maximum = 100, default = 50)]
    limit: Option<usize>,
}

#[utoipa::path(
    get,
    path = "/api/v1/applications",
    operation_id = "listApplications",
    summary = "List applications",
    params(ListQuery),
    responses(
        (status = 200, description = "Success", body = Envelope<Page<ApplicationView>>),
        (status = 400, response = inline(ApiErrorResponse)),
        (status = 500, response = inline(ApiErrorResponse)),
        (status = 503, response = inline(ApiErrorResponse)),
    )
)]
pub(super) async fn list(
    State(state): State<ApiState>,
    Query(query): Query<ListQuery>,
) -> Result<impl IntoResponse, ApiError> {
    let limit = query.limit.unwrap_or(DEFAULT_PAGE_SIZE);
    let page = state
        .store
        .list(query.cursor.as_deref(), limit)
        .await
        .map_err(|error| match error {
            StoreError::InvalidInput => ApiError::new(
                StatusCode::BAD_REQUEST,
                "pagination_invalid",
                "pagination parameters are invalid",
            ),
            error => error.into(),
        })?;
    Ok(ok(Page {
        items: page
            .items
            .into_iter()
            .map(StoredApplication::view)
            .collect(),
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
pub(super) async fn get(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let id = ApplicationId::parse(&id)?;
    Ok(ok(state.store.get(&id).await?.view()))
}

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
    require_runtime_execution(&state)?;
    reject_name_collision(&state, &app, None).await?;
    let prepared = state.runtime.prepare(&app).await?;
    let plan = plan(
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

#[utoipa::path(
    put,
    path = "/api/v1/applications/{id}",
    operation_id = "replaceApplication",
    summary = "Replace an application",
    params(
        ("id" = String, Path, min_length = 8, max_length = 64),
        ("X-Expected-Generation" = Option<u64>, Header, nullable = false, format = "uint64", minimum = 1, description = "Required for application/toml replacement and replacement planning."),
    ),
    request_body(
        content(
            (ReplaceApplicationRequest = "application/json"),
            (String = "application/toml"),
            (String = "text/toml")
        )
    ),
    responses(
        (status = 202, description = "Success", body = Envelope<AcceptedOperation>),
        (status = 400, response = inline(ApiErrorResponse)),
        (status = 404, response = inline(ApiErrorResponse)),
        (status = 409, response = inline(ApiErrorResponse)),
        (status = 415, response = inline(ApiErrorResponse)),
        (status = 422, response = inline(ApiErrorResponse)),
        (status = 500, response = inline(ApiErrorResponse)),
        (status = 502, response = inline(ApiErrorResponse)),
        (status = 503, response = inline(ApiErrorResponse)),
    )
)]
pub(super) async fn replace(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let id = ApplicationId::parse(&id)?;
    let (validated, expected) = parse_update(&headers, &body)?;
    let current = state.store.get(&id).await?;
    generation(expected, current.generation)?;
    require_runtime_execution(&state)?;
    let app = validated.normalize(id.clone());
    reject_name_collision(&state, &app, Some(&id)).await?;
    let prepared = state.runtime.prepare(&app).await?;
    let plan = plan(
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
        .replace(&app, Some(&prepared.resolved), expected, &steps)
        .await?;
    Ok(accepted(AcceptedOperation {
        operation_id: mutation.operation_id,
        application_id: id.to_string(),
        generation: mutation.generation,
    }))
}

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
    body: Bytes,
) -> Result<Response, ApiError> {
    require_json(&headers)?;
    let request: DeleteApplicationRequest = decode_json(&body)?;
    valid_expected_generation(request.expected_generation)?;
    let id = ApplicationId::parse(&id)?;
    let current = state.store.get(&id).await?;
    generation(request.expected_generation, current.generation)?;
    require_runtime_execution(&state)?;
    let observed = state.runtime.observe(&current).await?;
    let instance = InstanceId::parse(&state.instance_id).map_err(|_| StoreError::Corrupt)?;
    let plan = plan(
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
    Ok(accepted(AcceptedOperation {
        operation_id: mutation.operation_id,
        application_id: id.to_string(),
        generation: mutation.generation,
    }))
}

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
        (status = 415, response = inline(ApiErrorResponse)),
        (status = 422, response = inline(ApiErrorResponse)),
        (status = 500, response = inline(ApiErrorResponse)),
        (status = 503, response = inline(ApiErrorResponse)),
    )
)]
pub(super) async fn plan_create(
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
    body: Bytes,
) -> Result<impl IntoResponse, ApiError> {
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

#[utoipa::path(
    post,
    path = "/api/v1/applications/{id}/reconcile",
    operation_id = "reconcileApplication",
    summary = "Request application reconciliation",
    params(("id" = String, Path, min_length = 8, max_length = 64)),
    request_body = ExpectedGeneration,
    responses(
        (status = 202, description = "Success", body = Envelope<AcceptedOperation>),
        (status = 400, response = inline(ApiErrorResponse)),
        (status = 404, response = inline(ApiErrorResponse)),
        (status = 409, response = inline(ApiErrorResponse)),
        (status = 415, response = inline(ApiErrorResponse)),
        (status = 500, response = inline(ApiErrorResponse)),
        (status = 502, response = inline(ApiErrorResponse)),
        (status = 503, response = inline(ApiErrorResponse)),
    )
)]
pub(super) async fn reconcile(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
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
    require_runtime_execution(&state)?;
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
    let steps = plan
        .actions
        .iter()
        .map(PlanAction::operation_step)
        .collect::<Vec<_>>();
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

fn require_runtime_execution(state: &ApiState) -> Result<(), ApiError> {
    if state.runtime.capabilities().runtime_execution {
        Ok(())
    } else {
        Err(BoundaryError::Unavailable.into())
    }
}

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
    Ok(ok(state.store.status(&id).await?.view()))
}

#[utoipa::path(
    get,
    path = "/api/v1/applications/{id}/events",
    operation_id = "watchApplication",
    summary = "Watch application status events",
    params(
        ("id" = String, Path, min_length = 8, max_length = 64),
        ("Last-Event-ID" = Option<String>, Header, nullable = false, description = "Last durable/current-state event ID received by the client."),
    ),
    responses(
        (status = 200, description = "Server-Sent Events with durable/current-state IDs and bounded replay reset events.", body = String, content_type = "text/event-stream"),
        (status = 400, response = inline(ApiErrorResponse)),
        (status = 404, response = inline(ApiErrorResponse)),
        (status = 500, response = inline(ApiErrorResponse)),
        (status = 503, response = inline(ApiErrorResponse)),
    ),
    extensions(("x-sse-keepalive-seconds" = json!(15)))
)]
pub(super) async fn events(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let id = ApplicationId::parse(&id)?;
    state.store.status(&id).await?;
    let last = last_event_id(&headers);
    let store = state.store;
    Ok(Sse::new(current_state_stream("application", last, move || {
        let store = store.clone();
        let id = id.clone();
        async move {
            let status = match store.status(&id).await {
                Ok(status) => status,
                Err(error) => {
                    tracing::warn!(application_id = %id, %error, "application event stream store read failed");
                    return None;
                }
            };
            let data = match serde_json::to_string(&status.view()) {
                Ok(data) => data,
                Err(error) => {
                    tracing::error!(application_id = %id, %error, "application event stream serialization failed");
                    return None;
                }
            };
            Some(StateEventSnapshot {
                data,
                event: "application",
                terminal: false,
            })
        }
    }))
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("keepalive"),
        )
        .into_response())
}
