//! Individual typed editing endpoints. Persistence applies changes inside its writer transaction.
use super::{
    ApiError, ApiPath, ApiState, applications::accept_mutation, openapi::ApiErrorResponse,
};
use crate::api::Mutation;
use axum::{
    body::Bytes,
    extract::{
        Query, State,
        rejection::{BytesRejection, QueryRejection},
    },
    http::{HeaderMap, StatusCode},
    response::Response,
};
use piqueld_core::{
    ApplicationId,
    api::{Envelope, SavedApplication},
    edit::{
        ApplicationEdit, CpuValue, EditOptions, EnvironmentValue, HealthValue, MemoryValue,
        MountsValue, OptionalStringValue, ReplicasValue, RepositoryValue, ResourcesValue,
        SecondsValue, ServiceEdit, ServiceGeneral, ServiceProcess, SourceValue, StringValue,
        StringsValue, VolumesValue,
    },
    manifest::{Mount, Service, Volume},
};
use utoipa_axum::{router::OpenApiRouter, routes};

fn options(query: Result<Query<EditOptions>, QueryRejection>) -> Result<EditOptions, ApiError> {
    query.map(|Query(value)| value).map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "query_invalid",
            "expected_generation must be an integer; force and deploy must be true or false",
        )
    })
}
fn body<T: serde::de::DeserializeOwned>(
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
    super::decode_json(&super::applications::request_body(body)?)
}

// One declaration supplies the handler, route metadata, body type and typed mutation.
macro_rules! edit_endpoint {
    ($name:ident, $method:ident, $path:literal, ($($part:ident : $part_ty:ty = $label:literal),+), $body:ty, $decode:expr, $edit:expr) => {
        #[utoipa::path($method, path = $path, operation_id = stringify!($name),
            params($( ($label = String, Path), )+ EditOptions, ("Idempotency-Key" = Option<String>, Header)),
            request_body = $body,
            responses((status = 200, description = "Configuration saved", body = Envelope<SavedApplication>),
                (status = 202, description = "Configuration saved and deployment accepted", body = Envelope<SavedApplication>),
                (status = 400, response = inline(ApiErrorResponse)), (status = 404, response = inline(ApiErrorResponse)),
                (status = 409, response = inline(ApiErrorResponse)), (status = 413, response = inline(ApiErrorResponse)),
                (status = 415, response = inline(ApiErrorResponse)), (status = 422, response = inline(ApiErrorResponse)),
                (status = 500, response = inline(ApiErrorResponse)), (status = 503, response = inline(ApiErrorResponse))))]
        async fn $name(State(state): State<ApiState>, ApiPath(($($part,)+)): ApiPath<($($part_ty,)+)>, query: Result<Query<EditOptions>, QueryRejection>, headers: HeaderMap, bytes: Result<Bytes, BytesRejection>) -> Result<Response, ApiError> {
            let options = options(query)?;
            let body: $body = $decode(&headers, bytes)?;
            let edit = ($edit)(($($part.clone()),+), body);
            accept_mutation(&state, Mutation::Edit { id: ApplicationId::parse(edit_endpoint!(@id $($part),+))?, edit, deploy: options.deploy }, options.expected_generation, options.force, &headers).await
        }
    };
    (@id $first:ident $(,$rest:ident)*) => { $first };
}
edit_endpoint!(set_application_volumes, put, "/api/v1/applications/{id}/volumes", (id: String = "id"), VolumesValue, body::<VolumesValue>, |_, body: VolumesValue| ApplicationEdit::Volumes(body.value));
edit_endpoint!(set_application_name, put, "/api/v1/applications/{id}/name", (id: String = "id"), StringValue, body::<StringValue>, |_, body: StringValue| ApplicationEdit::Name(body.value));
edit_endpoint!(set_manifest_repository, put, "/api/v1/applications/{id}/repository", (id: String = "id"), RepositoryValue, body::<RepositoryValue>, |_, body: RepositoryValue| ApplicationEdit::Repository(body.value));
#[utoipa::path(delete, path = "/api/v1/applications/{id}/repository", operation_id = "disconnect_manifest_repository",
    params(("id" = String, Path), EditOptions, ("Idempotency-Key" = Option<String>, Header)),
    responses((status = 200, description = "Configuration saved", body = Envelope<SavedApplication>),
        (status = 202, description = "Deployment accepted", body = Envelope<SavedApplication>),
        (status = 400, response = inline(ApiErrorResponse)), (status = 404, response = inline(ApiErrorResponse)),
        (status = 409, response = inline(ApiErrorResponse)), (status = 422, response = inline(ApiErrorResponse)),
        (status = 500, response = inline(ApiErrorResponse)), (status = 503, response = inline(ApiErrorResponse))))]
async fn disconnect_manifest_repository(
    State(state): State<ApiState>,
    ApiPath(id): ApiPath<String>,
    query: Result<Query<EditOptions>, QueryRejection>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let options = options(query)?;
    accept_mutation(
        &state,
        Mutation::Edit {
            id: ApplicationId::parse(id)?,
            edit: ApplicationEdit::Repository(None),
            deploy: options.deploy,
        },
        options.expected_generation,
        options.force,
        &headers,
    )
    .await
}
edit_endpoint!(set_manifest_repository_url, put, "/api/v1/applications/{id}/repository/url", (id: String = "id"), StringValue, body::<StringValue>, |_, body: StringValue| ApplicationEdit::RepositoryUrl(body.value));
edit_endpoint!(set_manifest_repository_branch, put, "/api/v1/applications/{id}/repository/branch", (id: String = "id"), StringValue, body::<StringValue>, |_, body: StringValue| ApplicationEdit::RepositoryBranch(body.value));
edit_endpoint!(set_manifest_repository_commit, put, "/api/v1/applications/{id}/repository/commit", (id: String = "id"), OptionalStringValue, body::<OptionalStringValue>, |_, body: OptionalStringValue| ApplicationEdit::RepositoryCommit(body.value));
edit_endpoint!(set_manifest_repository_path, put, "/api/v1/applications/{id}/repository/path", (id: String = "id"), StringValue, body::<StringValue>, |_, body: StringValue| ApplicationEdit::RepositoryPath(body.value));
edit_endpoint!(add_application_service, post, "/api/v1/applications/{id}/services", (id: String = "id"), Service, body::<Service>, |_, body: Service| ApplicationEdit::AddService(body));
edit_endpoint!(add_application_volume, post, "/api/v1/applications/{id}/volumes", (id: String = "id"), Volume, body::<Volume>, |_, body: Volume| ApplicationEdit::AddVolume(body));
#[utoipa::path(delete, path = "/api/v1/applications/{id}/services/{service}", operation_id = "remove_application_service",
    params(("id" = String, Path), ("service" = String, Path), EditOptions, ("Idempotency-Key" = Option<String>, Header)),
    responses((status = 200, description = "Configuration saved", body = Envelope<SavedApplication>),
        (status = 202, description = "Deployment accepted", body = Envelope<SavedApplication>),
        (status = 400, response = inline(ApiErrorResponse)), (status = 404, response = inline(ApiErrorResponse)),
        (status = 409, response = inline(ApiErrorResponse)), (status = 422, response = inline(ApiErrorResponse)),
        (status = 500, response = inline(ApiErrorResponse)), (status = 503, response = inline(ApiErrorResponse))))]
async fn remove_application_service(
    State(state): State<ApiState>,
    ApiPath((id, service)): ApiPath<(String, String)>,
    query: Result<Query<EditOptions>, QueryRejection>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let options = options(query)?;
    accept_mutation(
        &state,
        Mutation::Edit {
            id: ApplicationId::parse(id)?,
            edit: ApplicationEdit::RemoveService(service),
            deploy: options.deploy,
        },
        options.expected_generation,
        options.force,
        &headers,
    )
    .await
}
#[utoipa::path(delete, path = "/api/v1/applications/{id}/volumes/{volume}", operation_id = "remove_application_volume",
    params(("id" = String, Path), ("volume" = String, Path), EditOptions, ("Idempotency-Key" = Option<String>, Header)),
    responses((status = 200, description = "Configuration saved", body = Envelope<SavedApplication>),
        (status = 202, description = "Deployment accepted", body = Envelope<SavedApplication>),
        (status = 400, response = inline(ApiErrorResponse)), (status = 404, response = inline(ApiErrorResponse)),
        (status = 409, response = inline(ApiErrorResponse)), (status = 422, response = inline(ApiErrorResponse)),
        (status = 500, response = inline(ApiErrorResponse)), (status = 503, response = inline(ApiErrorResponse))))]
async fn remove_application_volume(
    State(state): State<ApiState>,
    ApiPath((id, volume)): ApiPath<(String, String)>,
    query: Result<Query<EditOptions>, QueryRejection>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let options = options(query)?;
    accept_mutation(
        &state,
        Mutation::Edit {
            id: ApplicationId::parse(id)?,
            edit: ApplicationEdit::RemoveVolume(volume),
            deploy: options.deploy,
        },
        options.expected_generation,
        options.force,
        &headers,
    )
    .await
}
edit_endpoint!(set_service_name, put, "/api/v1/applications/{id}/services/{service}/name", (id: String = "id", service: String = "service"), StringValue, body::<StringValue>, |(_, service), body: StringValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::Name(body.value) });
edit_endpoint!(set_service_source, put, "/api/v1/applications/{id}/services/{service}/source", (id: String = "id", service: String = "service"), SourceValue, body::<SourceValue>, |(_, service), body: SourceValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::Source(body.value) });
edit_endpoint!(set_service_image, put, "/api/v1/applications/{id}/services/{service}/source/image", (id: String = "id", service: String = "service"), StringValue, body::<StringValue>, |(_, service), body: StringValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::Image(body.value) });
edit_endpoint!(set_service_git_url, put, "/api/v1/applications/{id}/services/{service}/source/git/url", (id: String = "id", service: String = "service"), StringValue, body::<StringValue>, |(_, service), body: StringValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::GitUrl(body.value) });
edit_endpoint!(set_service_git_branch, put, "/api/v1/applications/{id}/services/{service}/source/git/branch", (id: String = "id", service: String = "service"), StringValue, body::<StringValue>, |(_, service), body: StringValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::GitBranch(body.value) });
edit_endpoint!(set_service_git_commit, put, "/api/v1/applications/{id}/services/{service}/source/git/commit", (id: String = "id", service: String = "service"), OptionalStringValue, body::<OptionalStringValue>, |(_, service), body: OptionalStringValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::GitCommit(body.value) });
edit_endpoint!(set_service_dockerfile, put, "/api/v1/applications/{id}/services/{service}/source/git/dockerfile", (id: String = "id", service: String = "service"), StringValue, body::<StringValue>, |(_, service), body: StringValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::Dockerfile(body.value) });
edit_endpoint!(set_service_context, put, "/api/v1/applications/{id}/services/{service}/source/git/context", (id: String = "id", service: String = "service"), StringValue, body::<StringValue>, |(_, service), body: StringValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::Context(body.value) });
edit_endpoint!(set_service_replicas, put, "/api/v1/applications/{id}/services/{service}/replicas", (id: String = "id", service: String = "service"), ReplicasValue, body::<ReplicasValue>, |(_, service), body: ReplicasValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::Replicas(body.value) });
edit_endpoint!(set_service_environment, put, "/api/v1/applications/{id}/services/{service}/environment", (id: String = "id", service: String = "service"), EnvironmentValue, body::<EnvironmentValue>, |(_, service), body: EnvironmentValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::Environment(body.value) });
edit_endpoint!(set_service_command, put, "/api/v1/applications/{id}/services/{service}/command", (id: String = "id", service: String = "service"), StringsValue, body::<StringsValue>, |(_, service), body: StringsValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::Command(body.value) });
edit_endpoint!(set_service_arguments, put, "/api/v1/applications/{id}/services/{service}/arguments", (id: String = "id", service: String = "service"), StringsValue, body::<StringsValue>, |(_, service), body: StringsValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::Arguments(body.value) });
edit_endpoint!(set_service_mounts, put, "/api/v1/applications/{id}/services/{service}/mounts", (id: String = "id", service: String = "service"), MountsValue, body::<MountsValue>, |(_, service), body: MountsValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::Mounts(body.value) });
edit_endpoint!(set_service_healthcheck, put, "/api/v1/applications/{id}/services/{service}/healthcheck", (id: String = "id", service: String = "service"), HealthValue, body::<HealthValue>, |(_, service), body: HealthValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::Healthcheck(body.value) });
edit_endpoint!(set_service_health_port, put, "/api/v1/applications/{id}/services/{service}/healthcheck/port", (id: String = "id", service: String = "service"), ReplicasValue, body::<ReplicasValue>, |(_, service), body: ReplicasValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::HealthPort(body.value) });
edit_endpoint!(set_service_health_path, put, "/api/v1/applications/{id}/services/{service}/healthcheck/path", (id: String = "id", service: String = "service"), StringValue, body::<StringValue>, |(_, service), body: StringValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::HealthPath(body.value) });
edit_endpoint!(set_service_health_command, put, "/api/v1/applications/{id}/services/{service}/healthcheck/command", (id: String = "id", service: String = "service"), StringsValue, body::<StringsValue>, |(_, service), body: StringsValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::HealthCommand(body.value) });
edit_endpoint!(set_service_health_interval, put, "/api/v1/applications/{id}/services/{service}/healthcheck/interval", (id: String = "id", service: String = "service"), SecondsValue, body::<SecondsValue>, |(_, service), body: SecondsValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::HealthInterval(body.value) });
edit_endpoint!(set_service_health_timeout, put, "/api/v1/applications/{id}/services/{service}/healthcheck/timeout", (id: String = "id", service: String = "service"), SecondsValue, body::<SecondsValue>, |(_, service), body: SecondsValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::HealthTimeout(body.value) });
edit_endpoint!(set_service_resources, put, "/api/v1/applications/{id}/services/{service}/resources", (id: String = "id", service: String = "service"), ResourcesValue, body::<ResourcesValue>, |(_, service), body: ResourcesValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::Resources(body.value) });
edit_endpoint!(set_service_cpu, put, "/api/v1/applications/{id}/services/{service}/resources/cpu", (id: String = "id", service: String = "service"), CpuValue, body::<CpuValue>, |(_, service), body: CpuValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::Cpu(body.value) });
edit_endpoint!(set_service_memory, put, "/api/v1/applications/{id}/services/{service}/resources/memory", (id: String = "id", service: String = "service"), MemoryValue, body::<MemoryValue>, |(_, service), body: MemoryValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::Memory(body.value) });
edit_endpoint!(set_service_general, put, "/api/v1/applications/{id}/services/{service}/general", (id: String = "id", service: String = "service"), ServiceGeneral, body::<ServiceGeneral>, |(_, service), body: ServiceGeneral| ApplicationEdit::Service { name: service, edit: ServiceEdit::General(body) });
edit_endpoint!(set_service_process, put, "/api/v1/applications/{id}/services/{service}/process", (id: String = "id", service: String = "service"), ServiceProcess, body::<ServiceProcess>, |(_, service), body: ServiceProcess| ApplicationEdit::Service { name: service, edit: ServiceEdit::Process(body) });
edit_endpoint!(set_service_environment_entry, put, "/api/v1/applications/{id}/services/{service}/environment/{key}", (id: String = "id", service: String = "service", key: String = "key"), StringValue, body::<StringValue>, |(_, service, key), body: StringValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::EnvironmentEntry((key, Some(body.value))) });
#[utoipa::path(delete, path = "/api/v1/applications/{id}/services/{service}/environment/{key}", operation_id = "remove_service_environment_entry",
    params(("id" = String, Path), ("service" = String, Path), ("key" = String, Path), EditOptions, ("Idempotency-Key" = Option<String>, Header)),
    responses((status = 200, description = "Configuration saved", body = Envelope<SavedApplication>),
        (status = 202, description = "Deployment accepted", body = Envelope<SavedApplication>),
        (status = 400, response = inline(ApiErrorResponse)), (status = 404, response = inline(ApiErrorResponse)),
        (status = 409, response = inline(ApiErrorResponse)), (status = 422, response = inline(ApiErrorResponse)),
        (status = 500, response = inline(ApiErrorResponse)), (status = 503, response = inline(ApiErrorResponse))))]
async fn remove_service_environment_entry(
    State(state): State<ApiState>,
    ApiPath((id, service, key)): ApiPath<(String, String, String)>,
    query: Result<Query<EditOptions>, QueryRejection>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let options = options(query)?;
    accept_mutation(
        &state,
        Mutation::Edit {
            id: ApplicationId::parse(id)?,
            edit: ApplicationEdit::Service {
                name: service,
                edit: ServiceEdit::EnvironmentEntry((key, None)),
            },
            deploy: options.deploy,
        },
        options.expected_generation,
        options.force,
        &headers,
    )
    .await
}
edit_endpoint!(set_service_mount, put, "/api/v1/applications/{id}/services/{service}/mount", (id: String = "id", service: String = "service"), Mount, body::<Mount>, |(_, service), body: Mount| ApplicationEdit::Service { name: service, edit: ServiceEdit::Mount(body) });
edit_endpoint!(remove_service_mount, delete, "/api/v1/applications/{id}/services/{service}/mount", (id: String = "id", service: String = "service"), StringValue, body::<StringValue>, |(_, service), body: StringValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::RemoveMount(body.value) });

#[derive(Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct CreateQuery {
    deploy: bool,
}

#[utoipa::path(post, path = "/api/v1/applications", operation_id = "create_application",
    params(("deploy" = Option<bool>, Query), ("Idempotency-Key" = Option<String>, Header)), request_body = StringValue,
    responses((status = 200, description = "Empty application saved", body = Envelope<SavedApplication>),
        (status = 202, description = "Deployment accepted", body = Envelope<SavedApplication>),
        (status = 400, response = inline(ApiErrorResponse)), (status = 409, response = inline(ApiErrorResponse)),
        (status = 413, response = inline(ApiErrorResponse)), (status = 415, response = inline(ApiErrorResponse)),
        (status = 422, response = inline(ApiErrorResponse)), (status = 500, response = inline(ApiErrorResponse)), (status = 503, response = inline(ApiErrorResponse))))]
async fn create_application(
    State(state): State<ApiState>,
    query: Result<Query<CreateQuery>, QueryRejection>,
    headers: HeaderMap,
    bytes: Result<Bytes, BytesRejection>,
) -> Result<Response, ApiError> {
    let Query(query) = query.map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "query_invalid",
            "deploy must be true or false",
        )
    })?;
    let name: StringValue = body(&headers, bytes)?;
    let manifest = piqueld_core::manifest::ApplicationManifest {
        api_version: piqueld_core::manifest::APPLICATION_API_VERSION.into(),
        kind: piqueld_core::manifest::APPLICATION_KIND.into(),
        metadata: piqueld_core::manifest::Metadata { name: name.value },
        spec: piqueld_core::manifest::ApplicationSpec::default(),
    }
    .validate()?;
    accept_mutation(
        &state,
        Mutation::save(manifest, None, query.deploy),
        Some(0),
        false,
        &headers,
    )
    .await
}

pub(super) fn router() -> OpenApiRouter<ApiState> {
    OpenApiRouter::new()
        .routes(routes!(create_application))
        .routes(routes!(set_application_volumes))
        .routes(routes!(set_application_name))
        .routes(routes!(set_manifest_repository))
        .routes(routes!(disconnect_manifest_repository))
        .routes(routes!(set_manifest_repository_url))
        .routes(routes!(set_manifest_repository_branch))
        .routes(routes!(set_manifest_repository_commit))
        .routes(routes!(set_manifest_repository_path))
        .routes(routes!(add_application_service))
        .routes(routes!(add_application_volume))
        .routes(routes!(remove_application_service))
        .routes(routes!(remove_application_volume))
        .routes(routes!(set_service_name))
        .routes(routes!(set_service_source))
        .routes(routes!(set_service_image))
        .routes(routes!(set_service_git_url))
        .routes(routes!(set_service_git_branch))
        .routes(routes!(set_service_git_commit))
        .routes(routes!(set_service_dockerfile))
        .routes(routes!(set_service_context))
        .routes(routes!(set_service_replicas))
        .routes(routes!(set_service_environment))
        .routes(routes!(set_service_command))
        .routes(routes!(set_service_arguments))
        .routes(routes!(set_service_mounts))
        .routes(routes!(set_service_healthcheck))
        .routes(routes!(set_service_health_port))
        .routes(routes!(set_service_health_path))
        .routes(routes!(set_service_health_command))
        .routes(routes!(set_service_health_interval))
        .routes(routes!(set_service_health_timeout))
        .routes(routes!(set_service_resources))
        .routes(routes!(set_service_cpu))
        .routes(routes!(set_service_memory))
        .routes(routes!(set_service_general))
        .routes(routes!(set_service_process))
        .routes(routes!(set_service_environment_entry))
        .routes(routes!(remove_service_environment_entry))
        .routes(routes!(set_service_mount))
        .routes(routes!(remove_service_mount))
}
