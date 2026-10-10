//! Environment lifecycle, status, and repair. Deployment, history, logs, and
//! secrets of an environment live with their own handlers.
use super::applications::{ForceQuery, GenerationQuery, accept_mutation, request_body};
use super::{ApiError, ApiPath, ApiState, accepted, ok, openapi::ApiErrorResponse};
use crate::api::{Mutation, PromotionMutation, SourceChoice};
use crate::auth::Identity;
use axum::{
    Extension,
    body::Bytes,
    extract::{Query, State, rejection::BytesRejection},
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use piqueld_core::api::{
    AcceptedOperation, AcceptedPromotion, CreateEnvironmentRequest, Envelope,
    EnvironmentBranchRequest, EnvironmentDetailView, EnvironmentRequest, EnvironmentSourceRequest,
    EnvironmentStatusView, EnvironmentView, PlanView, PromoteRequest, PromotionSelection,
};
use piqueld_core::sync::EnvironmentSyncRequest;
use piqueld_core::{ApplicationId, EnvironmentId, EnvironmentName, ReleaseId, TrackedBranch};

/// Decodes a JSON environment or preview request, requiring the JSON content type.
pub(super) fn environment_request<T: serde::de::DeserializeOwned>(
    headers: &HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<T, ApiError> {
    if super::content_type(headers) != Some("application/json") {
        return Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "content_type_unsupported",
            "Content-Type must be application/json",
        ));
    }
    super::decode_json(&request_body(body)?)
}

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
    query: Result<Query<ForceQuery>, axum::extract::rejection::QueryRejection>,
    ApiPath(id): ApiPath<String>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let request: CreateEnvironmentRequest = environment_request(&headers, body)?;
    let source = match (request.branch, request.commit, request.promote_from) {
        (Some(branch), commit, None) => {
            Some(SourceChoice::Branch(TrackedBranch::new(branch, commit)?))
        }
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
        ForceQuery::decode(query)?.force,
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
// reported as diagnostics rather than failing the request.
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
    ApiPath(id): ApiPath<String>,
) -> Result<impl IntoResponse, ApiError> {
    Ok(ok(state
        .environment_detail(&EnvironmentId::parse(id)?)
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
    query: Result<Query<ForceQuery>, axum::extract::rejection::QueryRejection>,
    ApiPath(id): ApiPath<String>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let request: EnvironmentRequest = environment_request(&headers, body)?;
    accept_mutation(
        &state,
        &identity,
        Mutation::RenameEnvironment {
            id: EnvironmentId::parse(id)?,
            name: EnvironmentName::parse(request.name)?,
        },
        request.expected_generation,
        ForceQuery::decode(query)?.force,
        &headers,
    )
    .await
}

/// Changes the branch an environment follows.
///
/// Points an environment of a repository-backed application at another branch
/// of its manifest repository, or pins or unpins a commit. Nothing is fetched
/// or redeployed: the next deployment fetches the new branch. Environments
/// deploying the saved manifest fail with `manifest_repository_required`. The
/// inspected application `expected_generation` goes in the JSON body.
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
    query: Result<Query<ForceQuery>, axum::extract::rejection::QueryRejection>,
    ApiPath(id): ApiPath<String>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let request: EnvironmentBranchRequest = environment_request(&headers, body)?;
    accept_mutation(
        &state,
        &identity,
        Mutation::SetBranch {
            id: EnvironmentId::parse(id)?,
            branch: TrackedBranch::new(request.branch, request.commit)?,
        },
        request.expected_generation,
        ForceQuery::decode(query)?.force,
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
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let request: EnvironmentSyncRequest = environment_request(&headers, body)?;
    let mutation = Mutation::SetSync {
        id: EnvironmentId::parse(id)?,
        enabled: request.enabled,
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
    query: Result<Query<ForceQuery>, axum::extract::rejection::QueryRejection>,
    ApiPath(id): ApiPath<String>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let request: EnvironmentSourceRequest = environment_request(&headers, body)?;
    accept_mutation(
        &state,
        &identity,
        Mutation::Promotion(PromotionMutation::SetSource {
            id: EnvironmentId::parse(id)?,
            promote_from: request.promote_from.map(EnvironmentId::parse).transpose()?,
        }),
        request.expected_generation,
        ForceQuery::decode(query)?.force,
        &headers,
    )
    .await
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
    query: Result<Query<ForceQuery>, axum::extract::rejection::QueryRejection>,
    ApiPath(id): ApiPath<String>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let request: PromoteRequest = environment_request(&headers, body)?;
    let request_id = super::optional_header(&headers, "idempotency-key")?;
    Ok(accepted(
        state
            .promote(
                identity.actor(),
                &EnvironmentId::parse(id)?,
                selection(&request)?,
                request.expected_generation,
                ForceQuery::decode(query)?.force,
                request_id.as_deref(),
            )
            .await?,
    ))
}

/// Plans a promotion, or deploying an earlier release, without changing
/// anything.
///
/// Shows the release and its provenance and image availability, the
/// rendered diff against the environment's latest deployment, the runtime
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
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let request: PromoteRequest = environment_request(&headers, body)?;
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
    query: Result<Query<GenerationQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Response, ApiError> {
    let query = GenerationQuery::decode(query)?;
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

/// Reconciles an environment.
///
/// Returns 202 with a durable operation that repairs the runtime to match the
/// accepted configuration. Unlike other mutations, `expected_generation` is
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
    query: Result<Query<GenerationQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Response, ApiError> {
    let query = GenerationQuery::decode(query)?;
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
