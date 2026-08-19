use axum::{
    body::Bytes,
    extract::{
        Path, Query, State,
        rejection::{BytesRejection, QueryRejection},
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response, Sse, sse::KeepAlive},
};
use piqueld_client::{
    AcceptedOperation, ApplicationDetailView, ApplicationLogsOptions, ApplicationStatusView,
    ApplicationView, ContainerLogView, CreateApplicationRequest, DeleteApplicationRequest,
    DiagnosticView, Envelope, ExpectedGeneration, ObservedApplicationView, ObservedServiceView,
    Page, PlanApplicationRequest, PlanView, ReplaceApplicationRequest, ReplacePlanRequest,
};
use piqueld_core::{
    ApplicationId, InstanceId, NormalizedApplication, ObservedApplication, Plan, PlanAction,
    PlanRequest, ResolutionSet, compile_application, preview_resolution,
    resource::{
        Convergence, ObservedService, ResolvedApplication, ResolvedSource, TaskDiagnostic,
        TaskState,
    },
};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest, Sha256};

use super::operations;
use super::{
    ApiError, ApiState, BoundaryError, PreparedBuild, RequestShape, StateEventSnapshot, accepted,
    current_state_stream, decode_json, generation, hex, idempotency_key_hash,
    idempotent_application_id, last_event_id, mutation_request_hash, ok, openapi::ApiErrorResponse,
    optional_idempotency_key, parse_manifest, parse_update, require_json,
    valid_expected_generation,
};
use crate::store::{
    ApplicationStatus, DEFAULT_PAGE_SIZE, MAX_PAGE_SIZE, OperationKind, StoreError,
    StoredApplication,
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
pub(super) async fn get(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let id = ApplicationId::parse(&id)?;
    Ok(ok(application_view(state.store.get(&id).await?)))
}

#[utoipa::path(
    get,
    path = "/api/v1/applications/{id}/detail",
    operation_id = "getApplicationDetail",
    summary = "Get desired and observed application state",
    params(("id" = String, Path, min_length = 8, max_length = 64)),
    responses(
        (status = 200, description = "Success", body = Envelope<ApplicationDetailView>),
        (status = 400, response = inline(ApiErrorResponse)),
        (status = 404, response = inline(ApiErrorResponse)),
        (status = 502, response = inline(ApiErrorResponse)),
        (status = 500, response = inline(ApiErrorResponse)),
        (status = 503, response = inline(ApiErrorResponse)),
    )
)]
pub(super) async fn detail(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let id = ApplicationId::parse(&id)?;
    let stored = state.store.get(&id).await?;
    let durable_status = state.store.status(&id).await?;
    let status = status_view(
        durable_status,
        state.runtime.infrastructure_status(&stored).await?,
        state.runtime.runtime_status(&stored).await?,
    );
    let observed = state.runtime.observe(&stored).await?;
    let observed_view = observed_view(&stored, &observed);
    let latest_operation = state
        .store
        .latest_operation_for_application(&id)
        .await?
        .map(|(operation, steps)| operations::view(operation, steps));
    let diagnostics = detail_diagnostics(&status, &observed_view, latest_operation.as_ref());
    Ok(ok(ApplicationDetailView {
        application: application_view(stored),
        status,
        observed: observed_view,
        latest_operation,
        diagnostics,
    }))
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
    let _maintenance = state.ordinary_lease()?;
    let key = optional_idempotency_key(&headers)?.ok_or_else(|| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "idempotency_key_required",
            "Idempotency-Key is required for application creation",
        )
    })?;
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
    reject_application_conflicts(&state, &app, None).await?;
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
    record_prepared_builds(&state, &mutation.operation_id, &app.id, &prepared.builds).await?;
    state.runtime.trigger_reconciliation();
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
        ("Idempotency-Key" = Option<String>, Header, min_length = 1, max_length = 128, description = "Binds a retry-safe mutation to one request."),
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
        (status = 413, response = inline(ApiErrorResponse)),
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
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let body = request_body(body)?;
    let _maintenance = state.ordinary_lease()?;
    let key = optional_idempotency_key(&headers)?;
    let id = ApplicationId::parse(&id)?;
    let (validated, expected) = parse_update(&headers, &body)?;
    let app = validated.normalize(id.clone());
    let spec_hash = app.spec_hash();
    let key_binding = key.map(|key| {
        (
            idempotency_key_hash(key),
            mutation_request_hash("replace", &id, expected, Some(&spec_hash)),
        )
    });
    if let Some((key_hash, request_hash)) = &key_binding
        && let Some(mutation) = state
            .store
            .mutation_idempotency(&id, key_hash, request_hash, OperationKind::Replace)
            .await?
    {
        return Ok(accepted(AcceptedOperation {
            operation_id: mutation.operation_id,
            application_id: id.to_string(),
            generation: mutation.generation,
        }));
    }
    let current = state.store.get(&id).await?;
    generation(expected, current.generation)?;
    reject_application_conflicts(&state, &app, Some(&id)).await?;
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
    let mutation = if let Some((key_hash, request_hash)) = &key_binding {
        state
            .store
            .replace_idempotent(
                &app,
                &prepared.resolved,
                expected,
                &steps,
                key_hash,
                request_hash,
            )
            .await?
    } else {
        state
            .store
            .replace(&app, &prepared.resolved, expected, &steps)
            .await?
    };
    record_prepared_builds(&state, &mutation.operation_id, &app.id, &prepared.builds).await?;
    state.runtime.trigger_reconciliation();
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
    params(
        ("id" = String, Path, min_length = 8, max_length = 64),
        ("Idempotency-Key" = Option<String>, Header, min_length = 1, max_length = 128, description = "Binds a retry-safe mutation to one request."),
    ),
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
    let _maintenance = state.ordinary_lease()?;
    require_json(&headers)?;
    let request: DeleteApplicationRequest = decode_json(&body)?;
    valid_expected_generation(request.expected_generation)?;
    let key = optional_idempotency_key(&headers)?;
    let id = ApplicationId::parse(&id)?;
    let key_binding = key.map(|key| {
        (
            idempotency_key_hash(key),
            mutation_request_hash("delete", &id, request.expected_generation, None),
        )
    });
    if let Some((key_hash, request_hash)) = &key_binding
        && let Some(mutation) = state
            .store
            .mutation_idempotency(&id, key_hash, request_hash, OperationKind::Delete)
            .await?
    {
        return Ok(accepted(AcceptedOperation {
            operation_id: mutation.operation_id,
            application_id: id.to_string(),
            generation: mutation.generation,
        }));
    }
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
    let mutation = if let Some((key_hash, request_hash)) = &key_binding {
        state
            .store
            .request_delete_idempotent(
                &id,
                request.expected_generation,
                &steps,
                key_hash,
                request_hash,
            )
            .await?
    } else {
        state
            .store
            .request_delete(&id, request.expected_generation, &steps)
            .await?
    };
    state.runtime.trigger_reconciliation();
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
    reject_application_conflicts(&state, &app, None).await?;
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
    reject_application_conflicts(&state, &app, Some(&id)).await?;
    let plan = preview_plan(&state, &app, Some(&current)).await?;
    Ok(ok(PlanView {
        application_id: id.to_string(),
        proposed_generation: next_generation(expected)?,
        plan,
    }))
}

#[utoipa::path(
    post,
    path = "/api/v1/applications/{id}/delete-plan",
    operation_id = "planApplicationDelete",
    summary = "Preview application deletion",
    params(("id" = String, Path, min_length = 8, max_length = 64)),
    request_body = DeleteApplicationRequest,
    responses(
        (status = 200, description = "Success", body = Envelope<PlanView>),
        (status = 400, response = inline(ApiErrorResponse)),
        (status = 404, response = inline(ApiErrorResponse)),
        (status = 409, response = inline(ApiErrorResponse)),
        (status = 500, response = inline(ApiErrorResponse)),
        (status = 502, response = inline(ApiErrorResponse)),
        (status = 503, response = inline(ApiErrorResponse)),
    )
)]
pub(super) async fn plan_delete(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let body = request_body(body)?;
    require_json(&headers)?;
    let request: DeleteApplicationRequest = decode_json(&body)?;
    valid_expected_generation(request.expected_generation)?;
    let id = ApplicationId::parse(&id)?;
    let current = state.store.get(&id).await?;
    generation(request.expected_generation, current.generation)?;
    let observed = state.runtime.observe(&current).await?;
    let instance = InstanceId::parse(&state.instance_id).map_err(StoreError::corrupt)?;
    let plan = plan(
        &PlanRequest::Delete {
            application_id: id.clone(),
            instance_id: instance,
        },
        &observed,
    );
    Ok(ok(PlanView {
        application_id: id.to_string(),
        proposed_generation: next_generation(request.expected_generation)?,
        plan,
    }))
}

fn next_generation(current: u64) -> Result<u64, ApiError> {
    current.checked_add(1).ok_or_else(|| {
        ApiError::new(
            StatusCode::CONFLICT,
            "application_generation_exhausted",
            "application generation cannot be incremented",
        )
    })
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
                piqueld_core::SecretGeneration {
                    logical_name: secret.logical_name.clone(),
                    generation: secret.generation.clone(),
                    swarm_name: secret.name.clone(),
                },
            )
        })
        .collect();
    ResolutionSet { sources, secrets }
}

async fn record_prepared_builds(
    state: &ApiState,
    operation_id: &str,
    application_id: &ApplicationId,
    builds: &[PreparedBuild],
) -> Result<(), ApiError> {
    for build in builds {
        state
            .store
            .record_prepared_build(
                operation_id,
                application_id,
                &build.service_name,
                &build.source_commit,
                &build.image_reference,
                &build.image_digest,
                &build.build_key,
                &build.context_hash,
                &build.logs,
            )
            .await?;
    }
    Ok(())
}

async fn reject_application_conflicts(
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
    let requested_hosts = app
        .spec
        .routes
        .iter()
        .map(|route| route.host.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    if requested_hosts.is_empty() {
        return Ok(());
    }
    let mut cursor = None;
    loop {
        let page = state.store.list(cursor.as_deref(), MAX_PAGE_SIZE).await?;
        for stored in page.items {
            if own_id == Some(&stored.application.id) {
                continue;
            }
            if stored
                .application
                .spec
                .routes
                .iter()
                .any(|route| requested_hosts.contains(route.host.as_str()))
            {
                return Err(ApiError::new(
                    StatusCode::CONFLICT,
                    "route_host_collision",
                    "a requested route hostname is already owned by another application",
                ));
            }
        }
        let Some(next) = page.next_cursor else { break };
        cursor = Some(next);
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
        (status = 413, response = inline(ApiErrorResponse)),
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
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let body = request_body(body)?;
    let _maintenance = state.ordinary_lease()?;
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
    let stored = state.store.get(&id).await?;
    let status = state.store.status(&id).await?;
    let services = state.runtime.runtime_status(&stored).await?;
    let infrastructure = state.runtime.infrastructure_status(&stored).await?;
    Ok(ok(status_view(status, infrastructure, services)))
}

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(super) struct LogsQuery {
    /// Include records newer than this many seconds.
    since_seconds: Option<u64>,
    /// Maximum records returned.
    #[param(minimum = 1, maximum = 1000, default = 200)]
    tail: Option<u16>,
    /// Maximum approximate response size.
    #[param(minimum = 1, maximum = 1_048_576, default = 262_144)]
    max_bytes: Option<u32>,
    /// Keep the connection open and emit a new bounded snapshot when it changes.
    #[serde(default)]
    follow: bool,
}

#[utoipa::path(
    get,
    path = "/api/v1/applications/{id}/logs",
    operation_id = "applicationLogs",
    summary = "Read bounded application logs",
    params(("id" = String, Path, min_length = 8, max_length = 64), LogsQuery),
    responses(
        (status = 200, description = "Success", body = Envelope<Vec<ContainerLogView>>),
        (status = 400, response = inline(ApiErrorResponse)),
        (status = 404, response = inline(ApiErrorResponse)),
        (status = 502, response = inline(ApiErrorResponse)),
        (status = 503, response = inline(ApiErrorResponse)),
    )
)]
pub(super) async fn logs(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    query: Result<Query<LogsQuery>, QueryRejection>,
) -> Result<Response, ApiError> {
    let Query(query) = query.map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "logs_query_invalid",
            "log query parameters are invalid",
        )
    })?;
    let id = ApplicationId::parse(&id)?;
    let application = state.store.get(&id).await?;
    let options = ApplicationLogsOptions {
        since_seconds: query.since_seconds,
        tail: query.tail,
        max_bytes: query.max_bytes,
    };
    if !query.follow {
        return Ok(ok(state
            .runtime
            .application_logs(&application, &options)
            .await?)
        .into_response());
    }
    let runtime = state.runtime.clone();
    let last = last_event_id(&headers);
    let stream = current_state_stream("logs", last, move || {
        let runtime = runtime.clone();
        let application = application.clone();
        let options = options.clone();
        async move {
            let records = runtime
                .application_logs(&application, &options)
                .await
                .ok()?;
            Some(StateEventSnapshot {
                data: serde_json::to_string(&records).ok()?,
                event: "logs",
                terminal: false,
            })
        }
    });
    Ok(Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(std::time::Duration::from_secs(15))
                .text("keepalive"),
        )
        .into_response())
}

#[utoipa::path(
    get,
    path = "/api/v1/applications/{id}/events",
    operation_id = "watchApplication",
    summary = "Watch application status events",
    params(
        ("id" = String, Path, min_length = 8, max_length = 64),
        ("Last-Event-ID" = Option<String>, Header, nullable = false)
    ),
    responses(
        (status = 200, description = "Server-Sent Events", body = String, content_type = "text/event-stream"),
        (status = 400, response = inline(ApiErrorResponse)),
        (status = 404, response = inline(ApiErrorResponse)),
        (status = 503, response = inline(ApiErrorResponse)),
    )
)]
pub(super) async fn events(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let id = ApplicationId::parse(&id)?;
    state.store.status(&id).await?;
    let store = state.store.clone();
    let runtime = state.runtime.clone();
    let last = last_event_id(&headers);
    let stream = current_state_stream("application", last, move || {
        let store = store.clone();
        let runtime = runtime.clone();
        let id = id.clone();
        async move {
            let stored = store.get(&id).await.ok()?;
            let durable = store.status(&id).await.ok()?;
            let view = status_view(
                durable,
                runtime.infrastructure_status(&stored).await.ok()?,
                runtime.runtime_status(&stored).await.ok()?,
            );
            Some(StateEventSnapshot {
                data: serde_json::to_string(&view).ok()?,
                event: "application",
                terminal: false,
            })
        }
    });
    Ok(Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(std::time::Duration::from_secs(15))
                .text("keepalive"),
        )
        .into_response())
}

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

fn status_view(
    status: ApplicationStatus,
    infrastructure: Option<String>,
    services: Vec<piqueld_client::ServiceStatusView>,
) -> ApplicationStatusView {
    ApplicationStatusView {
        application_id: status.application_id.to_string(),
        state: status.state.as_str().into(),
        observed_generation: status.observed_generation,
        message: status.message,
        infrastructure,
        services,
        updated_at_ms: status.updated_at_ms,
    }
}

const MAX_DETAIL_DIAGNOSTICS: usize = 24;
const MAX_SERVICE_DIAGNOSTICS: usize = 8;

fn observed_view(
    stored: &StoredApplication,
    observed: &ObservedApplication,
) -> ObservedApplicationView {
    let services = stored
        .resolved
        .services
        .iter()
        .map(|desired| {
            let runtime = observed
                .services
                .iter()
                .find(|service| service.name == desired.name);
            let (image, observed_replicas, healthy_replicas, convergence, diagnostics) = runtime
                .map_or_else(
                    || {
                        (
                            None,
                            0,
                            0,
                            "failed".into(),
                            vec![DiagnosticView {
                                code: "service_missing".into(),
                                message:
                                    "the desired service was not found in the runtime observation"
                                        .into(),
                            }],
                        )
                    },
                    |service| {
                        (
                            Some(service.image.clone()),
                            service.replicas,
                            healthy_replicas(service),
                            convergence_name(&service.convergence).into(),
                            service_diagnostics(service),
                        )
                    },
                );
            ObservedServiceView {
                name: desired.logical_name.clone(),
                image,
                desired_replicas: desired.replicas,
                observed_replicas,
                healthy_replicas,
                convergence,
                diagnostics,
            }
        })
        .collect();
    ObservedApplicationView {
        services,
        network_count: u32::try_from(observed.networks.len()).unwrap_or(u32::MAX),
        volume_count: u32::try_from(observed.volumes.len()).unwrap_or(u32::MAX),
    }
}

fn healthy_replicas(service: &ObservedService) -> u16 {
    u16::try_from(
        service
            .tasks
            .iter()
            .filter(|task| {
                task.desired_running
                    && task.state == TaskState::Running
                    && task.healthy != Some(false)
            })
            .count(),
    )
    .unwrap_or(u16::MAX)
}

fn convergence_name(convergence: &Convergence) -> &'static str {
    match convergence {
        Convergence::Converged => "converged",
        Convergence::Updating => "updating",
        Convergence::Degraded => "degraded",
        Convergence::Failed => "failed",
    }
}

fn service_diagnostics(service: &ObservedService) -> Vec<DiagnosticView> {
    let mut diagnostics = service
        .tasks
        .iter()
        .filter_map(|task| task.diagnostic.as_ref())
        .map(|diagnostic| match diagnostic {
            TaskDiagnostic::Failed { exit_code } => DiagnosticView {
                code: "task_failed".into(),
                message: exit_code.map_or_else(
                    || "a desired task exited unsuccessfully".into(),
                    |code| format!("a desired task exited with status code {code}"),
                ),
            },
            TaskDiagnostic::Rejected => DiagnosticView {
                code: "task_rejected".into(),
                message: "the runtime rejected a desired task before it started".into(),
            },
        })
        .collect::<Vec<_>>();
    if matches!(
        service.convergence,
        Convergence::Degraded | Convergence::Failed
    ) {
        diagnostics.push(DiagnosticView {
            code: "service_not_converged".into(),
            message: format!(
                "{} of {} desired replicas are healthy",
                healthy_replicas(service),
                service.replicas
            ),
        });
    }
    diagnostics.truncate(MAX_SERVICE_DIAGNOSTICS);
    diagnostics
}

fn detail_diagnostics(
    status: &ApplicationStatusView,
    observed: &ObservedApplicationView,
    operation: Option<&piqueld_client::OperationView>,
) -> Vec<DiagnosticView> {
    let mut diagnostics = Vec::new();
    if let Some(message) = &status.message {
        diagnostics.push(DiagnosticView {
            code: "application_status".into(),
            message: message.clone(),
        });
    }
    diagnostics.extend(
        observed
            .services
            .iter()
            .flat_map(|service| service.diagnostics.iter().cloned()),
    );
    if let Some(operation) = operation {
        if let (Some(code), Some(message)) = (&operation.error_code, &operation.error_message) {
            diagnostics.push(DiagnosticView {
                code: code.clone(),
                message: message.clone(),
            });
        }
        diagnostics.extend(operation.steps.iter().filter_map(|step| {
            let message = step.error_message.as_ref()?;
            Some(DiagnosticView {
                code: step
                    .error_code
                    .clone()
                    .unwrap_or_else(|| "operation_step_failed".into()),
                message: message.clone(),
            })
        }));
    }
    diagnostics.truncate(MAX_DETAIL_DIAGNOSTICS);
    diagnostics
}
