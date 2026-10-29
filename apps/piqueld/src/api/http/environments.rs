//! Environment lifecycle, status, and repair. Deployment, history, logs, and
//! secrets of an environment live with their own handlers.
use super::applications::{ForceQuery, GenerationQuery, accept_mutation};
use super::extract::{IDEMPOTENCY_KEY, optional_header};
use super::{
    ApiError, ApiJson, ApiPath, ApiQuery, ApiState, accepted, ok, openapi::ApiErrorResponse,
};
use crate::api::{Mutation, PromotionMutation, Restore, SourceChoice};
use crate::auth::Identity;
use axum::{
    Extension,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use piqueld_core::access::AppPermission;
use piqueld_core::api::{
    AcceptedOperation, AcceptedPromotion, CreateEnvironmentRequest, Envelope,
    EnvironmentBranchRequest, EnvironmentDetailView, EnvironmentRequest, EnvironmentSourceRequest,
    EnvironmentStatusView, EnvironmentTelemetryRequest, EnvironmentView, PlanView, PromoteRequest,
    PromotionSelection, RestoreRequest,
};
use piqueld_core::sync::EnvironmentSyncRequest;
use piqueld_core::{
    ApplicationId, EnvironmentId, EnvironmentName, ReleaseId, TrackedBranch, ValidationErrors,
    codes,
};

/// Adds an environment to an application.
///
/// The environment starts `not_deployed`. It deploys the application's saved
/// manifest or, when the application is repository-backed, the manifest on
/// `branch` (by default the branch `spec.manifest` names), optionally pinned
/// to `commit`. A branch for an application without a repository fails with
/// `manifest_repository_required`. With `promote_from`, it never builds and
/// only receives releases promoted from that environment, which must be
/// another environment of the application, never a preview
/// (`promotion_source_invalid`). The inspected application
/// `expected_generation` goes in the JSON body.
#[utoipa::path(post,path="/api/v1/applications/{id}/environments",operation_id="createEnvironment",
    params(("id"=String,Path),ForceQuery,("Idempotency-Key"=Option<String>,Header)),
    request_body=CreateEnvironmentRequest,
    responses((status=200,description="Environment created",body=Envelope<EnvironmentView>),
    (status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),
    (status=409,response=inline(ApiErrorResponse)),(status=415,response=inline(ApiErrorResponse)),
    (status=422,response=inline(ApiErrorResponse)),(status=500,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn create(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    ApiQuery(query): ApiQuery<ForceQuery>,
    ApiPath(id): ApiPath<String>,
    headers: HeaderMap,
    ApiJson(request): ApiJson<CreateEnvironmentRequest>,
) -> Result<Response, ApiError> {
    let source = match (request.branch, request.commit, request.promote_from) {
        (Some(branch), commit, None) => Some(SourceChoice::Branch(tracked_branch(branch, commit)?)),
        (None, None, Some(source)) => {
            Some(SourceChoice::PromoteFrom(EnvironmentId::parse(source)?))
        }
        (None, None, None) => None,
        (None, Some(_), None) => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "revision_invalid",
                "a pinned commit requires a branch",
            ));
        }
        (_, _, Some(_)) => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "source_invalid",
                "a promoted environment follows no branch",
            ));
        }
    };
    accept_mutation(
        &state,
        &identity,
        Mutation::CreateEnvironment {
            application: ApplicationId::parse(id)?,
            name: EnvironmentName::parse(request.name)?,
            source,
        },
        request.expected_generation,
        query.force,
        &headers,
    )
    .await
}

#[utoipa::path(
    get,
    path = "/api/v1/environments/{id}",
    operation_id = "getEnvironment",
    summary = "Get an environment",
    params(("id" = String, Path, min_length = 8, max_length = 64)),
    responses(
        (status = 200, description = "Success", body = Envelope<EnvironmentView>),
        (status = 400, response = inline(ApiErrorResponse)),
        (status = 404, response = inline(ApiErrorResponse)),
        (status = 500, response = inline(ApiErrorResponse)),
        (status = 503, response = inline(ApiErrorResponse)),
    )
)]
pub(super) async fn get(
    State(state): State<ApiState>,
    ApiPath(id): ApiPath<String>,
) -> Result<impl IntoResponse, ApiError> {
    Ok(ok(state.environment(&EnvironmentId::parse(id)?).await?))
}

// Combines saved intent with a live runtime observation; runtime outages are
// reported as diagnostics rather than failing the request. Links list only
// importers the caller can read.
#[utoipa::path(
    get,
    path = "/api/v1/environments/{id}/detail",
    operation_id = "getEnvironmentDetail",
    summary = "Get desired and observed environment state",
    params(("id" = String, Path, min_length = 8, max_length = 64)),
    responses(
        (status = 200, description = "Success", body = Envelope<EnvironmentDetailView>),
        (status = 400, response = inline(ApiErrorResponse)),
        (status = 404, response = inline(ApiErrorResponse)),
        (status = 502, response = inline(ApiErrorResponse)),
        (status = 500, response = inline(ApiErrorResponse)),
        (status = 503, response = inline(ApiErrorResponse)),
    )
)]
pub(super) async fn detail(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    ApiPath(id): ApiPath<String>,
) -> Result<impl IntoResponse, ApiError> {
    Ok(ok(state
        .environment_detail(
            &EnvironmentId::parse(id)?,
            &identity.grants.app_scope(AppPermission::Read),
        )
        .await?))
}

#[utoipa::path(
    get,
    path = "/api/v1/environments/{id}/status",
    operation_id = "environmentStatus",
    summary = "Get environment status",
    params(("id" = String, Path, min_length = 8, max_length = 64)),
    responses(
        (status = 200, description = "Success", body = Envelope<EnvironmentStatusView>),
        (status = 400, response = inline(ApiErrorResponse)),
        (status = 404, response = inline(ApiErrorResponse)),
        (status = 500, response = inline(ApiErrorResponse)),
        (status = 503, response = inline(ApiErrorResponse)),
    )
)]
pub(super) async fn status(
    State(state): State<ApiState>,
    ApiPath(id): ApiPath<String>,
) -> Result<impl IntoResponse, ApiError> {
    Ok(ok(state
        .environment_status(&EnvironmentId::parse(id)?)
        .await?))
}

/// Renames an environment.
///
/// Changes only the name, without redeploying. The inspected application
/// `expected_generation` goes in the JSON body; the new name must not already
/// be used by another environment of the application, even with `force=true`.
#[utoipa::path(post,path="/api/v1/environments/{id}/rename",operation_id="renameEnvironment",
    params(("id"=String,Path),ForceQuery,("Idempotency-Key"=Option<String>,Header)),
    request_body=EnvironmentRequest,
    responses((status=200,description="Environment renamed without redeployment",body=Envelope<EnvironmentView>),
    (status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),
    (status=409,response=inline(ApiErrorResponse)),(status=415,response=inline(ApiErrorResponse)),
    (status=500,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn rename(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    ApiQuery(query): ApiQuery<ForceQuery>,
    ApiPath(id): ApiPath<String>,
    headers: HeaderMap,
    ApiJson(request): ApiJson<EnvironmentRequest>,
) -> Result<Response, ApiError> {
    accept_mutation(
        &state,
        &identity,
        Mutation::RenameEnvironment {
            id: EnvironmentId::parse(id)?,
            name: EnvironmentName::parse(request.name)?,
        },
        request.expected_generation,
        query.force,
        &headers,
    )
    .await
}

/// Changes the branch an environment follows.
///
/// Points an environment of a repository-backed application at another branch
/// of its manifest repository, or pins or unpins a commit. Nothing is fetched
/// or redeployed: the next deployment fetches the new branch. Environments of
/// an application without a manifest repository fail with
/// `manifest_repository_required` (422), those whose source the saved
/// manifest declares with `environment_declared` (409), and promoted ones,
/// which have no branch, with `environment_promoted` (409). The inspected
/// application `expected_generation` goes in the JSON body.
#[utoipa::path(put,path="/api/v1/environments/{id}/branch",operation_id="setEnvironmentBranch",
    params(("id"=String,Path),ForceQuery,("Idempotency-Key"=Option<String>,Header)),
    request_body=EnvironmentBranchRequest,
    responses((status=200,description="Environment follows the branch from its next deployment",body=Envelope<EnvironmentView>),
    (status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),
    (status=409,response=inline(ApiErrorResponse)),(status=415,response=inline(ApiErrorResponse)),
    (status=422,response=inline(ApiErrorResponse)),(status=500,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn branch(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    ApiQuery(query): ApiQuery<ForceQuery>,
    ApiPath(id): ApiPath<String>,
    headers: HeaderMap,
    ApiJson(request): ApiJson<EnvironmentBranchRequest>,
) -> Result<Response, ApiError> {
    accept_mutation(
        &state,
        &identity,
        Mutation::SetBranch {
            id: EnvironmentId::parse(id)?,
            branch: tracked_branch(request.branch, request.commit)?,
        },
        request.expected_generation,
        query.force,
        &headers,
    )
    .await
}

/// Opts an environment into or out of its application's sync.
///
/// While its application syncs (`spec.manifest.sync`), pushes to the branch
/// an environment follows deploy it once it opted in and was deployed. Pinned
/// environments never sync. Previews follow their application and are
/// `NotFound` here. Needs no application revision: nothing the environment
/// deploys changes. Opting in while the application syncs also needs
/// `apps:deploy`, since pushes then deploy the environment.
#[utoipa::path(put,path="/api/v1/environments/{id}/sync",operation_id="setEnvironmentSync",
    params(("id"=String,Path),("Idempotency-Key"=Option<String>,Header)),
    request_body=EnvironmentSyncRequest,
    responses((status=200,description="Environment sync changed",body=Envelope<EnvironmentView>),
    (status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),
    (status=409,response=inline(ApiErrorResponse)),(status=415,response=inline(ApiErrorResponse)),
    (status=500,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn sync(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    ApiPath(id): ApiPath<String>,
    headers: HeaderMap,
    ApiJson(request): ApiJson<EnvironmentSyncRequest>,
) -> Result<Response, ApiError> {
    let mutation = Mutation::SetSync {
        id: EnvironmentId::parse(id)?,
        enabled: request.enabled,
    };
    accept_mutation(&state, &identity, mutation, None, false, &headers).await
}

/// Grants or revokes an environment's telemetry.
///
/// A granted environment's receiving service (`receives_telemetry`) gets the
/// daemon's traces, metrics and logs from every application, over the socket
/// directory mounted at `/run/piqueld/telemetry`, so granting needs
/// `telemetry:grant` as well as reading the application. Revoking, like
/// deleting the environment, stops export at once; the controller mounts or
/// unmounts the directory within seconds, which rolls the service. Previews
/// are never granted it and are `NotFound`. Needs no application revision.
#[utoipa::path(put,path="/api/v1/environments/{id}/telemetry",operation_id="setEnvironmentTelemetry",
    params(("id"=String,Path),("Idempotency-Key"=Option<String>,Header)),
    request_body=EnvironmentTelemetryRequest,
    responses((status=200,description="Environment telemetry changed",body=Envelope<EnvironmentView>),
    (status=400,response=inline(ApiErrorResponse)),(status=403,response=inline(ApiErrorResponse)),
    (status=404,response=inline(ApiErrorResponse)),(status=409,response=inline(ApiErrorResponse)),
    (status=415,response=inline(ApiErrorResponse)),(status=500,response=inline(ApiErrorResponse)),
    (status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn telemetry(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    ApiPath(id): ApiPath<String>,
    headers: HeaderMap,
    ApiJson(request): ApiJson<EnvironmentTelemetryRequest>,
) -> Result<Response, ApiError> {
    let mutation = Mutation::SetTelemetry {
        id: EnvironmentId::parse(id)?,
        granted: request.granted,
    };
    accept_mutation(&state, &identity, mutation, None, false, &headers).await
}

/// Changes where an environment's releases come from.
///
/// With `promote_from`, the environment never builds or fetches again: it
/// only receives releases promoted from that environment, by ID, which must
/// be another live environment of the application, never a preview
/// (`promotion_source_invalid`), and must not promote from it, directly or
/// through others (`promotion_cycle`). Without it, a promoted environment
/// tracks the saved manifest, or the branch `spec.manifest` names, again.
/// Nothing is deployed. The inspected application `expected_generation`
/// goes in the JSON body.
#[utoipa::path(put,path="/api/v1/environments/{id}/source",operation_id="setEnvironmentSource",
    params(("id"=String,Path),ForceQuery,("Idempotency-Key"=Option<String>,Header)),
    request_body=EnvironmentSourceRequest,
    responses((status=200,description="Environment deploys from its new source from its next deployment or promotion",body=Envelope<EnvironmentView>),
    (status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),
    (status=409,response=inline(ApiErrorResponse)),(status=415,response=inline(ApiErrorResponse)),
    (status=422,response=inline(ApiErrorResponse)),(status=500,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn source(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    ApiQuery(query): ApiQuery<ForceQuery>,
    ApiPath(id): ApiPath<String>,
    headers: HeaderMap,
    ApiJson(request): ApiJson<EnvironmentSourceRequest>,
) -> Result<Response, ApiError> {
    accept_mutation(
        &state,
        &identity,
        Mutation::Promotion(PromotionMutation::SetSource {
            id: EnvironmentId::parse(id)?,
            promote_from: request.promote_from.map(EnvironmentId::parse).transpose()?,
        }),
        request.expected_generation,
        query.force,
        &headers,
    )
    .await
}

/// A request's `branch` and pinned `commit`, refused like other malformed
/// request fields, previews' branches included: 400 `git_branch_invalid`,
/// or `git_commit_invalid` when only the commit is.
fn tracked_branch(branch: String, commit: Option<String>) -> Result<TrackedBranch, ApiError> {
    TrackedBranch::new(branch, commit).map_err(|ValidationErrors(errors)| {
        let code = if errors
            .iter()
            .any(|error| error.code == codes::GIT_BRANCH_INVALID)
        {
            codes::GIT_BRANCH_INVALID
        } else {
            codes::GIT_COMMIT_INVALID
        };
        let messages: Vec<_> = errors.into_iter().map(|error| error.message).collect();
        ApiError::new(StatusCode::BAD_REQUEST, code, messages.join("; "))
    })
}

/// What a promotion request selects: the source's current deployment
/// (`deployment` requires it to be that one), or an earlier `release`.
fn selection(request: &PromoteRequest) -> Result<PromotionSelection, ApiError> {
    match (&request.deployment, &request.release) {
        (deployment, None) => Ok(PromotionSelection::Source {
            deployment: deployment.clone(),
        }),
        (None, Some(release)) => Ok(PromotionSelection::Release {
            release: ReleaseId::parse(release.clone()).map_err(|_| {
                ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "release_invalid",
                    "invalid release ID",
                )
            })?,
        }),
        (Some(_), Some(_)) => Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "promotion_invalid",
            "promote either a source deployment or a release, not both",
        )),
    }
}

/// Promotes a release into a promoted environment.
///
/// Deploys the release of the source environment's current deployment
/// (which must be `deployment` when given), or an earlier `release`, without
/// building or fetching. The release is pinned at acceptance: its own
/// manifest is rendered with this environment's `[spec.environments.<name>]`
/// block and variables, and its images deployed as recorded. Refused unless
/// the source deployment is current and succeeded and its services are
/// healthy now (`promotion_source_not_ready`, `promotion_source_changed`),
/// the release's build inputs render as they did for it
/// (`release_incompatible`), its images are present or can be pulled again
/// (`image_unavailable`), and every secret it mounts exists and allows this
/// environment (`secrets_unavailable`, listing all of them). The inspected
/// application `expected_generation` goes in the JSON body.
#[utoipa::path(post,path="/api/v1/environments/{id}/promote",operation_id="promoteEnvironment",
    params(("id"=String,Path),ForceQuery,("Idempotency-Key"=Option<String>,Header)),
    request_body=PromoteRequest,
    responses((status=202,description="Promotion accepted with its release pinned",body=Envelope<AcceptedPromotion>),
    (status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),
    (status=409,response=inline(ApiErrorResponse)),(status=415,response=inline(ApiErrorResponse)),
    (status=422,response=inline(ApiErrorResponse)),(status=500,response=inline(ApiErrorResponse)),
    (status=502,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn promote(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    ApiQuery(query): ApiQuery<ForceQuery>,
    ApiPath(id): ApiPath<String>,
    headers: HeaderMap,
    ApiJson(request): ApiJson<PromoteRequest>,
) -> Result<Response, ApiError> {
    let request_id = optional_header(&headers, IDEMPOTENCY_KEY)?;
    Ok(accepted(
        state
            .promote(
                identity.actor(),
                &EnvironmentId::parse(id)?,
                selection(&request)?,
                request.expected_generation,
                query.force,
                request_id.as_deref(),
            )
            .await?,
    ))
}

/// Plans a promotion, or deploying an earlier release, without changing
/// anything.
///
/// Shows the release and its provenance and image availability, the
/// rendered diff against the environment's current deployment, the runtime
/// plan, new (empty) volumes, and every secret that is missing or that the
/// environment may not use. The source must be promotable, as for
/// `promoteEnvironment`; an earlier `release` may be planned for any
/// environment. `expected_generation` is ignored.
#[utoipa::path(post,path="/api/v1/environments/{id}/promote/plan",operation_id="planPromotion",
    params(("id"=String,Path)),
    request_body=PromoteRequest,
    responses((status=200,description="Plan",body=Envelope<PlanView>),
    (status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),
    (status=409,response=inline(ApiErrorResponse)),(status=415,response=inline(ApiErrorResponse)),
    (status=422,response=inline(ApiErrorResponse)),(status=500,response=inline(ApiErrorResponse)),
    (status=502,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn plan_promotion(
    State(state): State<ApiState>,
    ApiPath(id): ApiPath<String>,
    ApiJson(request): ApiJson<PromoteRequest>,
) -> Result<impl IntoResponse, ApiError> {
    Ok(ok(state
        .plan_release(&EnvironmentId::parse(id)?, &selection(&request)?)
        .await?))
}

// Accepts a deletion operation; named volumes are retained.
#[utoipa::path(
    delete, path = "/api/v1/environments/{id}", operation_id = "deleteEnvironment",
    summary = "Delete an environment's services and networks, retaining volumes",
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
    Extension(identity): Extension<Identity>,
    ApiPath(id): ApiPath<String>,
    headers: HeaderMap,
    ApiQuery(query): ApiQuery<GenerationQuery>,
) -> Result<Response, ApiError> {
    accept_mutation(
        &state,
        &identity,
        Mutation::Delete {
            id: EnvironmentId::parse(id)?,
        },
        query.expected_generation,
        query.force,
        &headers,
    )
    .await
}

/// Reconciles an environment, starting it if it is stopped.
///
/// Returns 202 with the environment's latest operation, requested again when it
/// finished or failed, or as is while it is still in progress; 404 when the
/// environment has none. Unlike other mutations, `expected_generation` is
/// optional. Repeating a request with the same `Idempotency-Key` returns the
/// original response.
#[utoipa::path(post,path="/api/v1/environments/{id}/reconcile",operation_id="reconcileEnvironment",
    params(("id"=String,Path),GenerationQuery,("Idempotency-Key"=Option<String>,Header)),
    responses((status=202,description="Reconciliation accepted",body=Envelope<AcceptedOperation>),
    (status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),
    (status=409,response=inline(ApiErrorResponse)),(status=500,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn reconcile(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    ApiPath(id): ApiPath<String>,
    headers: HeaderMap,
    ApiQuery(query): ApiQuery<GenerationQuery>,
) -> Result<Response, ApiError> {
    accept_mutation(
        &state,
        &identity,
        Mutation::Reconcile {
            id: EnvironmentId::parse(id)?,
        },
        query.expected_generation,
        query.force,
        &headers,
    )
    .await
}

/// Stops an environment.
///
/// Its services scale to zero replicas, keeping their configuration, volumes
/// and secrets, and the gateway stops serving its routes before the request
/// returns, or on its next pass if that fails. It stays stopped until it is
/// started, deployed, promoted into or reconciled; sync deploys nothing to it
/// meanwhile. Returns 202 with its latest operation: run again when its
/// deployment had rolled out and has ended, so the services stop now, or as
/// is while it is in progress, which then rolls out with no replicas. 404
/// when it was never deployed, 409 while it is deleted. `expected_generation` is optional.
#[utoipa::path(post,path="/api/v1/environments/{id}/stop",operation_id="stopEnvironment",
    params(("id"=String,Path),GenerationQuery,("Idempotency-Key"=Option<String>,Header)),
    responses((status=202,description="Stop accepted",body=Envelope<AcceptedOperation>),
    (status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),
    (status=409,response=inline(ApiErrorResponse)),(status=500,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn stop(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    ApiPath(id): ApiPath<String>,
    headers: HeaderMap,
    ApiQuery(query): ApiQuery<GenerationQuery>,
) -> Result<Response, ApiError> {
    accept_mutation(
        &state,
        &identity,
        Mutation::Stop {
            id: EnvironmentId::parse(id)?,
        },
        query.expected_generation,
        query.force,
        &headers,
    )
    .await
}

/// Starts a stopped environment.
///
/// Its services scale back to their replicas and the gateway serves its
/// routes again. Like `reconcileEnvironment`, which it is, returns 202 with
/// its latest operation, requested again when it finished or failed, or as
/// is while it is in progress; 404 when it was never deployed.
/// `expected_generation` is optional.
#[utoipa::path(post,path="/api/v1/environments/{id}/start",operation_id="startEnvironment",
    params(("id"=String,Path),GenerationQuery,("Idempotency-Key"=Option<String>,Header)),
    responses((status=202,description="Start accepted",body=Envelope<AcceptedOperation>),
    (status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),
    (status=409,response=inline(ApiErrorResponse)),(status=500,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn start(
    state: State<ApiState>,
    identity: Extension<Identity>,
    id: ApiPath<String>,
    headers: HeaderMap,
    query: ApiQuery<GenerationQuery>,
) -> Result<Response, ApiError> {
    reconcile(state, identity, id, headers, query).await
}

/// Restores an earlier deployment of an environment exactly.
///
/// Deploys `deployment` again as it ran: its rendered manifest, its images
/// and the secret versions it pinned, recorded as a new deployment whose
/// origin names it, which becomes the maintained target. Nothing is rendered,
/// fetched or built, and jobs never run. Volume contents and migrations are
/// not reverted. Refused unless it succeeded (`deployment_not_restorable`),
/// its images are present or can be pulled again by digest
/// (`image_unavailable`), and the secret versions it pinned still exist
/// (`restore_secrets_unavailable`, listing them). Needs no application
/// revision. Previews restore through `restorePreviewDeployment`. Repeating a
/// request with the same `Idempotency-Key` returns the original response.
#[utoipa::path(post,path="/api/v1/environments/{id}/restore",operation_id="restoreDeployment",
    params(("id"=String,Path),("Idempotency-Key"=Option<String>,Header)),
    request_body=RestoreRequest,
    responses((status=202,description="Restore accepted",body=Envelope<AcceptedOperation>),
    (status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),
    (status=409,response=inline(ApiErrorResponse)),(status=415,response=inline(ApiErrorResponse)),
    (status=500,response=inline(ApiErrorResponse)),(status=502,response=inline(ApiErrorResponse)),
    (status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn restore(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    ApiPath(id): ApiPath<String>,
    headers: HeaderMap,
    ApiJson(request): ApiJson<RestoreRequest>,
) -> Result<Response, ApiError> {
    let request_id = optional_header(&headers, IDEMPOTENCY_KEY)?;
    Ok(accepted(
        state
            .restore(
                identity.actor(),
                Restore::Environment {
                    id: EnvironmentId::parse(id)?,
                    deployment: request.deployment,
                },
                request_id.as_deref(),
            )
            .await?,
    ))
}
