use super::{
    ApiError, ApiState, BoundaryError, accepted, ok, openapi::ApiErrorResponse, parse_manifest,
};
use crate::application::{Mutation, MutationResponse};
use crate::store::{ApplicationStatus, StoreError, StoredApplication};
use axum::{
    body::Bytes,
    extract::{
        Path, Query, State,
        rejection::{BytesRejection, QueryRejection},
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use piqueld_core::api::{
    AcceptedOperation, ApplicationDetailView, ApplicationStatusView, ApplicationView,
    ApplyApplicationRequest, DiagnosticView, Envelope, ManifestChange, ObservedApplicationView,
    ObservedServiceView, Page, PlanView, RenameApplicationRequest, RenamedApplication,
};
use piqueld_core::{
    ApplicationId, NormalizedApplication, ObservedApplication, Plan, PlanRequest, ResolutionSet,
    compile_application, preview_resolution,
    resource::{Convergence, ObservedService, TaskDiagnostic, TaskState},
};
use serde::Deserialize;

// A manifest can approach the 2 MiB request limit and JSON escaping can
// approximately double textual fields. Three entries stay below the clients'
// 16 MiB response budget with envelope overhead.
const APPLICATION_PAGE_SIZE: usize = 3;

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(super) struct ListQuery {
    cursor: Option<String>,
    #[param(minimum = 1, maximum = 3, default = 3)]
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
    let limit = query.limit.unwrap_or(APPLICATION_PAGE_SIZE);
    if !(1..=APPLICATION_PAGE_SIZE).contains(&limit) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "pagination_invalid",
            "pagination parameters are invalid",
        ));
    }
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
    let (stored, status) = state.store.get_with_status(&id).await?;
    let observed = state.runtime.observe(&stored).await?;
    let observed_view = observed_view(
        &stored,
        &observed,
        status.state == piqueld_core::ApplicationState::Ready,
    );
    let status = status_view(status);
    let latest_operation = state.store.latest_operation_for_application(&id).await?;
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
    post, path = "/api/v1/applications/apply", operation_id = "applyApplication",
    summary = "Apply an application manifest",
    params(ForceQuery,("X-Expected-Generation"=Option<u64>,Header,description="TOML only: required unless forced; zero requires absence. JSON uses expected_generation in the request body."),("X-Expected-Application-Id"=Option<String>,Header,description="TOML only: inspected application identity. JSON uses expected_application_id in the request body."),("Idempotency-Key"=Option<String>,Header)),
    request_body(content((ApplyApplicationRequest = "application/json"), (String = "application/toml"), (String = "text/toml"))),
    responses(
        (status = 202, description = "Accepted or unchanged target", body = Envelope<AcceptedOperation>),
        (status = 400, response = inline(ApiErrorResponse)),
        (status = 409, response = inline(ApiErrorResponse)),
        (status = 413, response = inline(ApiErrorResponse)),
        (status = 415, response = inline(ApiErrorResponse)),
        (status = 422, response = inline(ApiErrorResponse)),
        (status = 500, response = inline(ApiErrorResponse)),
        (status = 502, response = inline(ApiErrorResponse)),
        (status = 503, response = inline(ApiErrorResponse))
    )
)]
pub(super) async fn apply(
    State(state): State<ApiState>,
    query: Result<Query<ForceQuery>, axum::extract::rejection::QueryRejection>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let (manifest, expected, expected_id) = parse_manifest(&headers, &request_body(body)?)?;
    accept_mutation(
        &state,
        Mutation::apply(manifest, expected_id),
        expected,
        ForceQuery::decode(query)?.force,
        &headers,
    )
    .await
}

#[utoipa::path(
    delete, path = "/api/v1/applications/{id}", operation_id = "deleteApplication",
    summary = "Delete services and networks, retaining volumes",
    params(("id" = String, Path, min_length = 8, max_length = 64), GenerationQuery,("Idempotency-Key"=Option<String>,Header)),
    responses(
        (status = 409, response = inline(ApiErrorResponse)),
        (status = 202, description = "Deletion operation", body = Envelope<AcceptedOperation>),
        (status = 400, response = inline(ApiErrorResponse)),
        (status = 404, response = inline(ApiErrorResponse)),
        (status = 500, response = inline(ApiErrorResponse)),
        (status = 503, response = inline(ApiErrorResponse))
    )
)]
pub(super) async fn delete(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    query: Result<Query<GenerationQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Response, ApiError> {
    let query = GenerationQuery::decode(query)?;
    accept_mutation(
        &state,
        Mutation::Delete {
            id: ApplicationId::parse(id)?,
        },
        query.expected_generation,
        query.force,
        &headers,
    )
    .await
}

#[utoipa::path(
    post, path = "/api/v1/applications/plan", operation_id = "planApplication",
    summary = "Preview an application manifest",
    params(("X-Expected-Generation"=Option<u64>,Header,description="TOML only: inspected intent revision; zero requires absence. JSON uses expected_generation in the request body."),("X-Expected-Application-Id"=Option<String>,Header,description="TOML only: inspected application identity. JSON uses expected_application_id in the request body.")),
    request_body(content((ApplyApplicationRequest = "application/json"), (String = "application/toml"), (String = "text/toml"))),
    responses(
        (status = 200, description = "Preview", body = Envelope<PlanView>),
        (status = 409, response = inline(ApiErrorResponse)),
        (status = 400, response = inline(ApiErrorResponse)),
        (status = 413, response = inline(ApiErrorResponse)),
        (status = 415, response = inline(ApiErrorResponse)),
        (status = 422, response = inline(ApiErrorResponse)),
        (status = 500, response = inline(ApiErrorResponse)),
        (status = 502, response = inline(ApiErrorResponse)),
        (status = 503, response = inline(ApiErrorResponse))
    )
)]
pub(super) async fn plan(
    State(state): State<ApiState>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let (manifest, expected, expected_id) = parse_manifest(&headers, &request_body(body)?)?;
    let current = state.store.find_by_name(manifest.name()).await?;
    crate::store::SqliteStore::check_generation(
        expected,
        current.as_ref().map_or(0, |app| app.generation),
    )?;
    if let Some(expected_id) = expected_id
        && current
            .as_ref()
            .is_none_or(|app| app.application.id.as_str() != expected_id)
    {
        return Err(StoreError::IdentityConflict.into());
    }
    let id = current.as_ref().map_or_else(
        || ApplicationId::parse("preview-application").expect("valid preview ID"),
        |app| app.application.id.clone(),
    );
    let application = manifest.normalize(id.clone());
    let plan = preview_plan(&state, &application, current.as_ref()).await?;
    let operation = if let Some(current) = &current {
        state
            .store
            .latest_operation_for_application(&current.application.id)
            .await?
    } else {
        None
    };
    Ok(ok(PlanView {
        application_id: id.to_string(),
        generation: current.as_ref().map_or(0, |app| app.generation),
        identical: current
            .as_ref()
            .is_some_and(|app| !app.delete_intent && app.application == application),
        operation,
        changes: ManifestChange::between(
            current.as_ref().map(|app| &app.application),
            &application,
        ),
        plan,
    }))
}

async fn preview_plan(
    state: &ApiState,
    app: &NormalizedApplication,
    current: Option<&StoredApplication>,
) -> Result<piqueld_core::Plan, ApiError> {
    let observed = if let Some(current) = current {
        state.runtime.observe(current).await?
    } else {
        state.runtime.check_available().await?;
        ObservedApplication::default()
    };
    if current.is_some_and(|current| !current.delete_intent && current.application == *app) {
        return Ok(Plan::default());
    }
    let resolutions = current
        .and_then(|app| app.resolved.as_ref())
        .map_or_else(ResolutionSet::default, |target| {
            target.reusable_resolutions(app)
        });
    let unresolved = preview_resolution(app, &resolutions);
    let desired = if unresolved.is_empty() {
        Some(
            compile_application(
                app,
                piqueld_core::InstanceId::parse(state.store.instance_id())
                    .map_err(StoreError::corrupt)?,
                &resolutions,
            )
            .map_err(BoundaryError::Compilation)?,
        )
    } else {
        None
    };
    let mut plan = Plan::from_request(
        &PlanRequest::Preview {
            unresolved,
            desired,
        },
        &observed,
    );
    plan.redact_configuration();
    Ok(plan)
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
    Ok(ok(status_view(state.store.status(&id).await?)))
}

fn application_view(stored: StoredApplication) -> ApplicationView {
    ApplicationView {
        generation: stored.generation,
        resolved_generation: stored.resolved_generation,
        spec_hash: stored.application.spec_hash(),
        application: stored.application,
        delete_intent: stored.delete_intent,
        created_at_ms: stored.created_at_ms,
        updated_at_ms: stored.updated_at_ms,
    }
}

fn status_view(status: ApplicationStatus) -> ApplicationStatusView {
    ApplicationStatusView {
        application_id: status.application_id.to_string(),
        state: status.state,
        runtime_health: status.runtime_health,
        message: status.message,
        updated_at_ms: status.updated_at_ms,
    }
}

const MAX_DETAIL_DIAGNOSTICS: usize = 24;
const MAX_SERVICE_DIAGNOSTICS: usize = 8;

fn observed_view(
    stored: &StoredApplication,
    observed: &ObservedApplication,
    reconciled: bool,
) -> ObservedApplicationView {
    let services = stored
        .resolved
        .iter()
        .flat_map(|target| &target.services)
        .map(|desired| {
            let runtime = observed
                .services
                .iter()
                .find(|service| service.name == desired.name);
            let (image, observed_replicas, healthy_replicas, convergence, diagnostics) = runtime
                .map_or_else(
                    || {
                        let diagnostics = if reconciled {
                            vec![DiagnosticView {
                                code: "service_missing".into(),
                                message:
                                    "the desired service was not found in the runtime observation"
                                        .into(),
                            }]
                        } else {
                            Vec::new()
                        };
                        (
                            None,
                            0,
                            0,
                            if reconciled {
                                Convergence::Failed
                            } else {
                                Convergence::Updating
                            },
                            diagnostics,
                        )
                    },
                    |service| {
                        (
                            Some(service.image.clone()),
                            service.replicas,
                            healthy_replicas(service),
                            service.convergence.clone(),
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
                    && if service.healthcheck_configured {
                        task.healthy == Some(true)
                    } else {
                        task.healthy != Some(false)
                    }
            })
            .count(),
    )
    .unwrap_or(u16::MAX)
}

fn service_diagnostics(service: &ObservedService) -> Vec<DiagnosticView> {
    let mut diagnostics = Vec::new();
    if matches!(
        service.convergence,
        Convergence::Degraded | Convergence::Failed
    ) {
        let healthy_replicas = healthy_replicas(service);
        diagnostics.push(DiagnosticView {
            code: "service_not_converged".into(),
            message: format!(
                "{} of {} observed replicas are healthy",
                healthy_replicas, service.replicas
            ),
        });
    }
    diagnostics.extend(
        service
            .tasks
            .iter()
            .filter(|task| task.desired_running)
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
            }),
    );
    diagnostics.truncate(MAX_SERVICE_DIAGNOSTICS);
    diagnostics
}

fn detail_diagnostics(
    status: &ApplicationStatusView,
    observed: &ObservedApplicationView,
    operation: Option<&piqueld_core::Operation>,
) -> Vec<DiagnosticView> {
    let mut diagnostics = Vec::new();
    if let Some(message) = &status.message {
        diagnostics.push(DiagnosticView {
            code: "application_status".into(),
            message: message.clone(),
        });
    }
    if let Some(operation) = operation
        && let (Some(code), Some(message)) = (&operation.error_code, &operation.error_message)
    {
        diagnostics.push(DiagnosticView {
            code: code.clone(),
            message: message.clone(),
        });
    }
    diagnostics.extend(
        observed
            .services
            .iter()
            .flat_map(|service| service.diagnostics.iter().cloned()),
    );
    diagnostics.truncate(MAX_DETAIL_DIAGNOSTICS);
    diagnostics
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use piqueld_core::resource::{ObservedTask, TaskDiagnostic};

    use super::*;

    #[test]
    fn service_diagnostics_ignore_historical_tasks() {
        let service = ObservedService {
            name: "web".into(),
            image: "example/web@sha256:digest".into(),
            replicas: 1,
            environment: BTreeMap::new(),
            command: Vec::new(),
            arguments: Vec::new(),
            mounts: Vec::new(),
            healthcheck: None,
            healthcheck_configured: false,
            resources: None,
            networks: Vec::new(),
            labels: BTreeMap::new(),
            runtime_configuration_matches: true,
            tasks: vec![
                ObservedTask {
                    state: TaskState::Failed,
                    healthy: None,
                    desired_running: false,
                    diagnostic: Some(TaskDiagnostic::Failed { exit_code: Some(1) }),
                },
                ObservedTask {
                    state: TaskState::Rejected,
                    healthy: None,
                    desired_running: true,
                    diagnostic: Some(TaskDiagnostic::Rejected),
                },
            ],
            convergence: Convergence::Converged,
        };

        let diagnostics = service_diagnostics(&service);

        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "task_rejected");
    }

    #[test]
    fn pending_runtime_health_is_not_reported_as_healthy() {
        let task = ObservedTask {
            state: TaskState::Running,
            healthy: None,
            desired_running: true,
            diagnostic: None,
        };
        let mut service = ObservedService {
            name: "web".into(),
            image: "example/web@sha256:digest".into(),
            replicas: 1,
            environment: BTreeMap::new(),
            command: Vec::new(),
            arguments: Vec::new(),
            mounts: Vec::new(),
            healthcheck: None,
            healthcheck_configured: true,
            resources: None,
            networks: Vec::new(),
            labels: BTreeMap::new(),
            runtime_configuration_matches: false,
            tasks: vec![task],
            convergence: Convergence::Updating,
        };

        assert_eq!(healthy_replicas(&service), 0);
        service.healthcheck_configured = false;
        assert_eq!(healthy_replicas(&service), 1);
    }
}

#[derive(Default, Deserialize, utoipa::IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(super) struct GenerationQuery {
    /// Current intent revision; optional for reconcile and refresh, required for deletion unless forced.
    expected_generation: Option<u64>,
    /// Explicitly bypass intent preconditions.
    #[serde(default)]
    force: bool,
}
impl GenerationQuery {
    fn decode(
        query: Result<Query<Self>, axum::extract::rejection::QueryRejection>,
    ) -> Result<Self, ApiError> {
        query.map(|Query(value)| value).map_err(|_| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "generation_invalid",
                "invalid expected generation",
            )
        })
    }
}

#[utoipa::path(post,path="/api/v1/applications/{id}/reconcile",operation_id="reconcileApplication",
    params(("id"=String,Path),GenerationQuery,("Idempotency-Key"=Option<String>,Header)),
    responses((status=202,description="Reconciliation accepted",body=Envelope<AcceptedOperation>),
    (status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),
    (status=409,response=inline(ApiErrorResponse)),(status=500,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn reconcile(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    query: Result<Query<GenerationQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Response, ApiError> {
    let query = GenerationQuery::decode(query)?;
    accept_mutation(
        &state,
        Mutation::Reconcile {
            id: ApplicationId::parse(id)?,
        },
        query.expected_generation,
        query.force,
        &headers,
    )
    .await
}

#[utoipa::path(post,path="/api/v1/applications/{id}/refresh",operation_id="refreshApplication",
    params(("id"=String,Path),GenerationQuery,("Idempotency-Key"=Option<String>,Header)),
    responses((status=202,description="Image refresh accepted",body=Envelope<AcceptedOperation>),
    (status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),
    (status=409,response=inline(ApiErrorResponse)),(status=500,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn refresh(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    query: Result<Query<GenerationQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Response, ApiError> {
    let query = GenerationQuery::decode(query)?;
    accept_mutation(
        &state,
        Mutation::Refresh {
            id: ApplicationId::parse(id)?,
        },
        query.expected_generation,
        query.force,
        &headers,
    )
    .await
}

#[utoipa::path(post,path="/api/v1/applications/{id}/deploy",operation_id="deployApplication",
    params(("id"=String,Path),GenerationQuery,("Idempotency-Key"=Option<String>,Header)),
    responses((status=202,description="Deployment accepted",body=Envelope<AcceptedOperation>),
    (status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),
    (status=409,response=inline(ApiErrorResponse)),(status=500,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn deploy(
    State(state): State<ApiState>,
    Path(id): Path<String>,
    headers: HeaderMap,
    query: Result<Query<GenerationQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Response, ApiError> {
    let query = GenerationQuery::decode(query)?;
    accept_mutation(
        &state,
        Mutation::Deploy {
            id: ApplicationId::parse(id)?,
        },
        query.expected_generation,
        query.force,
        &headers,
    )
    .await
}

async fn accept_mutation(
    state: &ApiState,
    mutation: Mutation,
    expected: Option<u64>,
    force: bool,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    if !force {
        let missing = match &mutation {
            Mutation::Apply {
                expected_application_id,
                ..
            } => expected.is_none() || (expected != Some(0) && expected_application_id.is_none()),
            Mutation::Delete { .. } | Mutation::Rename { .. } => expected.is_none(),
            Mutation::Reconcile { .. } | Mutation::Refresh { .. } | Mutation::Deploy { .. } => {
                false
            }
        };
        if missing {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "precondition_required",
                "Supply the inspected revision and application identity, or explicitly set force=true",
            ));
        }
    }
    let request_id = super::optional_header(headers, "idempotency-key")?;
    match state
        .accept(mutation, expected, force, request_id.as_deref())
        .await?
    {
        MutationResponse::Operation(operation) => Ok(accepted(operation)),
        MutationResponse::Rename(renamed) => Ok(ok(renamed).into_response()),
    }
}

#[utoipa::path(post,path="/api/v1/applications/{id}/rename",operation_id="renameApplication",
    params(("id"=String,Path),ForceQuery,("Idempotency-Key"=Option<String>,Header)),
    request_body=RenameApplicationRequest,
    responses((status=200,description="Application renamed without redeployment",body=Envelope<RenamedApplication>),
    (status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),
    (status=409,response=inline(ApiErrorResponse)),(status=500,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn rename(
    State(state): State<ApiState>,
    query: Result<Query<ForceQuery>, axum::extract::rejection::QueryRejection>,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    if super::content_type(&headers) != Some("application/json") {
        return Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "content_type_unsupported",
            "Content-Type must be application/json",
        ));
    }
    let request: RenameApplicationRequest = super::decode_json(&request_body(body)?)?;
    accept_mutation(
        &state,
        Mutation::Rename {
            id: ApplicationId::parse(id)?,
            name: request.name,
        },
        request.expected_generation,
        ForceQuery::decode(query)?.force,
        &headers,
    )
    .await
}

#[derive(Default, Deserialize, utoipa::IntoParams)]
#[serde(default, deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(super) struct ForceQuery {
    /// Explicitly bypass the intent revision precondition and, for apply, the name-based identity precondition. Name availability is always enforced.
    force: bool,
}
impl ForceQuery {
    fn decode(
        query: Result<Query<Self>, axum::extract::rejection::QueryRejection>,
    ) -> Result<Self, ApiError> {
        query.map(|Query(value)| value).map_err(|_| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "force_invalid",
                "force must be true or false",
            )
        })
    }
}
