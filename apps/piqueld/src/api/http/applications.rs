use super::{ApiError, ApiState, accepted, ok, openapi::ApiErrorResponse, parse_manifest};
use crate::api::{Mutation, MutationResponse};
use axum::{
    body::Bytes,
    extract::{
        Path, Query, State,
        rejection::{BytesRejection, QueryRejection},
    },
    http::{HeaderMap, StatusCode, header},
    response::{IntoResponse, Response},
};
use piqueld_core::ApplicationId;
use piqueld_core::api::{
    AcceptedOperation, ApplicationDetailView, ApplicationStatusView, ApplicationSummary,
    ApplicationView, ApplyApplicationRequest, Envelope, Page, PlanView, RenameApplicationRequest,
    RenamedApplication, SavedApplication,
};
use serde::Deserialize;

#[derive(Deserialize, utoipa::IntoParams)]
#[into_params(parameter_in = Query)]
pub(super) struct ListQuery {
    cursor: Option<String>,
    #[param(minimum = 1, maximum = 100, default = 100)]
    limit: Option<u16>,
}

#[utoipa::path(
    get,
    path = "/api/v1/applications",
    operation_id = "listApplications",
    summary = "List applications",
    params(ListQuery),
    responses(
        (status = 200, description = "Success", body = Envelope<Page<ApplicationSummary>>),
        (status = 400, response = inline(ApiErrorResponse)),
        (status = 500, response = inline(ApiErrorResponse)),
        (status = 503, response = inline(ApiErrorResponse)),
    )
)]
pub(super) async fn list(
    State(state): State<ApiState>,
    query: Result<Query<ListQuery>, QueryRejection>,
) -> Result<impl IntoResponse, ApiError> {
    let Query(query) =
        query.map_err(|_| ApiError::from(crate::api::ApplicationError::InvalidPagination))?;
    Ok(ok(state
        .applications(query.cursor.as_deref(), query.limit)
        .await?))
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
    Ok(ok(state.application(&ApplicationId::parse(id)?).await?))
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
    Ok(ok(state
        .application_detail(&ApplicationId::parse(id)?)
        .await?))
}

#[utoipa::path(
    post, path = "/api/v1/applications/apply", operation_id = "applyApplication",
    summary = "Apply an application manifest",
    params(ApplyQuery,("X-Expected-Generation"=Option<u64>,Header,description="TOML only: required unless forced; zero requires absence. JSON uses `expected_generation` in the request body."),("X-Expected-Application-Id"=Option<String>,Header,description="TOML only: inspected application identity. JSON uses `expected_application_id` in the request body."),("Idempotency-Key"=Option<String>,Header)),
    request_body(content((ApplyApplicationRequest = "application/json"), (String = "application/toml"), (String = "text/toml"))),
    responses(
        (status = 200, description = "Configuration saved", body = Envelope<SavedApplication>),
        (status = 202, description = "Configuration saved and deployment accepted", body = Envelope<SavedApplication>),
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
    query: Result<Query<ApplyQuery>, axum::extract::rejection::QueryRejection>,
    headers: HeaderMap,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let Query(query) = query.map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "query_invalid",
            "force and deploy must be true or false",
        )
    })?;
    let (manifest, expected, expected_id) = parse_manifest(&headers, &request_body(body)?)?;
    accept_mutation(
        &state,
        Mutation::save(manifest, expected_id, query.deploy),
        expected,
        query.force,
        &headers,
    )
    .await
}

#[derive(Default, Deserialize, utoipa::IntoParams)]
#[serde(default, deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(super) struct ApplyQuery {
    /// Explicitly bypass revision and identity preconditions.
    pub(super) force: bool,
    /// Deploy the saved configuration; omission saves only.
    deploy: bool,
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
    params(("X-Expected-Generation"=Option<u64>,Header,description="TOML only: inspected intent revision; zero requires absence. JSON uses `expected_generation` in the request body."),("X-Expected-Application-Id"=Option<String>,Header,description="TOML only: inspected application identity. JSON uses `expected_application_id` in the request body.")),
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
    Ok(ok(state.plan(manifest, expected, expected_id).await?))
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
    Ok(ok(state
        .application_status(&ApplicationId::parse(id)?)
        .await?))
}

#[derive(Default, Deserialize, utoipa::IntoParams)]
#[serde(deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub(super) struct GenerationQuery {
    /// Current intent revision; optional for reconcile, required for deployment and deletion unless forced.
    pub(super) expected_generation: Option<u64>,
    /// Explicitly bypass intent preconditions.
    #[serde(default)]
    pub(super) force: bool,
}
impl GenerationQuery {
    pub(super) fn decode(
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

pub(super) async fn accept_mutation(
    state: &ApiState,
    mutation: Mutation,
    expected: Option<u64>,
    force: bool,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    let request_id = super::optional_header(headers, "idempotency-key")?;
    match state
        .accept(mutation, expected, force, request_id.as_deref())
        .await?
    {
        MutationResponse::Operation(operation) => Ok(accepted(operation)),
        MutationResponse::Saved(saved) => {
            if saved.operation_id.is_some() {
                Ok(accepted(saved))
            } else {
                Ok(ok(saved).into_response())
            }
        }
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
    pub(super) force: bool,
}
impl ForceQuery {
    pub(super) fn decode(
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

/// Download only saved configuration; runtime availability is irrelevant.
#[utoipa::path(get,path="/api/v1/applications/{id}/manifest",operation_id="downloadApplicationManifest",
    params(("id"=String,Path)),
    responses((status=200,description="Saved application configuration",body=String,content_type="application/toml",
        headers(("Content-Disposition"=String,description="Attachment filename for the saved TOML manifest"),
                ("Cache-Control"=String,description="no-store"))),
    (status=400,response=inline(ApiErrorResponse)),(status=404,response=inline(ApiErrorResponse)),
    (status=500,response=inline(ApiErrorResponse)),(status=503,response=inline(ApiErrorResponse))))]
pub(super) async fn manifest_download(
    State(state): State<ApiState>,
    Path(id): Path<String>,
) -> Result<impl IntoResponse, ApiError> {
    let manifest = state.manifest(&ApplicationId::parse(id)?).await?;
    let filename = format!("attachment; filename=\"{}\"", manifest.filename);
    let body = manifest.contents;
    Ok((
        [
            (header::CONTENT_TYPE, "application/toml".to_owned()),
            (header::CONTENT_DISPOSITION, filename),
            (header::CACHE_CONTROL, "no-store".to_owned()),
        ],
        body,
    ))
}
