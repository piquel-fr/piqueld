//! Individual typed editing endpoints. Persistence applies changes inside its writer transaction.
use super::{
    ApiError, ApiJson, ApiPath, ApiQuery, ApiState, QueryParams, applications::accept_mutation,
    openapi::ApiErrorResponse,
};
use crate::api::Mutation;
use crate::auth::Identity;
use axum::{Extension, extract::State, http::HeaderMap, response::Response};
use piqueld_core::{
    ApplicationId,
    api::{Envelope, SavedApplication},
    edit::{
        ApplicationEdit, CpuValue, EditOptions, EnvironmentValue, ExportsValue, FilesValue,
        HealthValue, ImportsValue, JobsValue, MemoryValue, MountsValue, OptionalStringValue,
        ReplicasValue, RepositorySyncValue, RepositoryValue, ResourcesValue, RolloutValue,
        RoutesValue, SecondsValue, SecretsValue, ServiceEdit, ServiceGeneral, ServiceProcess,
        SourceValue, StringValue, StringsValue, TemplateValue, TemplatesValue, VariablesValue,
        VisibilityValue, VolumesValue,
    },
    manifest::{Mount, Service, ServiceFile, Volume},
};
use utoipa_axum::router::OpenApiRouter;

impl QueryParams for EditOptions {
    const INVALID_CODE: &'static str = "query_invalid";
    const INVALID_MESSAGE: &'static str =
        "expected_generation must be an integer; force and deploy must be true or false";
}

/// Submits `edit` of application `id` under the shared query `options`,
/// answering 200 when saved or 202 when `deploy=true` also accepted a
/// deployment.
async fn submit(
    state: &ApiState,
    identity: &Identity,
    id: String,
    edit: ApplicationEdit,
    options: EditOptions,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    let mutation = Mutation::Edit {
        id: ApplicationId::parse(id)?,
        edit: Box::new(edit),
        deploy: options.deploy,
    };
    let (expected, force) = (options.expected_generation, options.force);
    accept_mutation(state, identity, mutation, expected, force, headers).await
}

// One declaration supplies the handler, its route metadata, and its typed
// edit, submitted through `submit`.
//
// Arguments: optional doc comments (the `OpenAPI` description), handler name
// (also the `OpenAPI` operation ID), HTTP method, path, the path parameters
// as `(binding: Type = "label", ...)` with the application ID first, then either
// the JSON body type and a closure mapping `(path parts, body)` to an
// `ApplicationEdit`, or `remove` and a closure mapping the path parts alone,
// for removals without a body.
macro_rules! edit_endpoint {
    // Must precede the body arm, whose `$body:ty` would also match `remove`.
    ($(#[$doc:meta])* $name:ident, $method:ident, $path:literal, ($($part:ident : $part_ty:ty = $label:literal),+), remove, $edit:expr) => {
        $(#[$doc])*
        #[utoipa::path($method, path = $path, operation_id = stringify!($name),
            params($( ($label = String, Path), )+ EditOptions, ("Idempotency-Key" = Option<String>, Header)),
            responses((status = 200, description = "Configuration saved", body = Envelope<SavedApplication>),
                (status = 202, description = "Deployment accepted", body = Envelope<SavedApplication>),
                (status = 400, response = inline(ApiErrorResponse)), (status = 404, response = inline(ApiErrorResponse)),
                (status = 409, response = inline(ApiErrorResponse)), (status = 422, response = inline(ApiErrorResponse)),
                (status = 500, response = inline(ApiErrorResponse)), (status = 503, response = inline(ApiErrorResponse))))]
        async fn $name(State(state): State<ApiState>, Extension(identity): Extension<Identity>, ApiPath(($($part,)+)): ApiPath<($($part_ty,)+)>, ApiQuery(options): ApiQuery<EditOptions>, headers: HeaderMap) -> Result<Response, ApiError> {
            let edit = ($edit)(($($part.clone()),+));
            submit(&state, &identity, edit_endpoint!(@id $($part),+), edit, options, &headers).await
        }
    };
    ($(#[$doc:meta])* $name:ident, $method:ident, $path:literal, ($($part:ident : $part_ty:ty = $label:literal),+), $body:ty, $edit:expr) => {
        $(#[$doc])*
        #[utoipa::path($method, path = $path, operation_id = stringify!($name),
            params($( ($label = String, Path), )+ EditOptions, ("Idempotency-Key" = Option<String>, Header)),
            request_body = $body,
            responses((status = 200, description = "Configuration saved", body = Envelope<SavedApplication>),
                (status = 202, description = "Configuration saved and deployment accepted", body = Envelope<SavedApplication>),
                (status = 400, response = inline(ApiErrorResponse)), (status = 404, response = inline(ApiErrorResponse)),
                (status = 409, response = inline(ApiErrorResponse)), (status = 413, response = inline(ApiErrorResponse)),
                (status = 415, response = inline(ApiErrorResponse)), (status = 422, response = inline(ApiErrorResponse)),
                (status = 500, response = inline(ApiErrorResponse)), (status = 503, response = inline(ApiErrorResponse))))]
        async fn $name(State(state): State<ApiState>, Extension(identity): Extension<Identity>, ApiPath(($($part,)+)): ApiPath<($($part_ty,)+)>, ApiQuery(options): ApiQuery<EditOptions>, headers: HeaderMap, ApiJson(body): ApiJson<$body>) -> Result<Response, ApiError> {
            let edit = ($edit)(($($part.clone()),+), body);
            submit(&state, &identity, edit_endpoint!(@id $($part),+), edit, options, &headers).await
        }
    };
    (@id $first:ident $(,$rest:ident)*) => { $first };
}
edit_endpoint!(set_application_volumes, put, "/api/v1/applications/{id}/volumes", (id: String = "id"), VolumesValue, |_, body: VolumesValue| ApplicationEdit::Volumes(body.value));
edit_endpoint!(set_application_routes, put, "/api/v1/applications/{id}/routes", (id: String = "id"), RoutesValue, |_, body: RoutesValue| ApplicationEdit::Routes(body.value));
edit_endpoint!(set_application_exports, put, "/api/v1/applications/{id}/exports", (id: String = "id"), ExportsValue, |_, body: ExportsValue| ApplicationEdit::Exports(body.value));
edit_endpoint!(set_application_imports, put, "/api/v1/applications/{id}/imports", (id: String = "id"), ImportsValue, |_, body: ImportsValue| ApplicationEdit::Imports(body.value));
edit_endpoint!(set_application_jobs, put, "/api/v1/applications/{id}/jobs", (id: String = "id"), JobsValue, |_, body: JobsValue| ApplicationEdit::Jobs(body.value));
edit_endpoint!(set_application_name, put, "/api/v1/applications/{id}/name", (id: String = "id"), StringValue, |_, body: StringValue| ApplicationEdit::Name(body.value));
edit_endpoint!(set_application_variables, put, "/api/v1/applications/{id}/variables", (id: String = "id"), VariablesValue, |_, body: VariablesValue| ApplicationEdit::Variables(body.value));
edit_endpoint!(set_environment_visibility, put, "/api/v1/applications/{id}/environments/{environment}/visibility", (id: String = "id", environment: String = "environment"), VisibilityValue, |(_, environment), body: VisibilityValue| ApplicationEdit::EnvironmentVisibility { environment, visibility: body.value });
edit_endpoint!(set_manifest_repository, put, "/api/v1/applications/{id}/repository", (id: String = "id"), RepositoryValue, |_, body: RepositoryValue| ApplicationEdit::Repository(body.value));
edit_endpoint!(
    /// Disconnects the manifest repository.
    ///
    /// The saved configuration becomes directly editable. Same query options,
    /// idempotency, and 200/202 responses as the other editing endpoints.
    disconnect_manifest_repository, delete, "/api/v1/applications/{id}/repository", (id: String = "id"), remove, |_| ApplicationEdit::Repository(None));
edit_endpoint!(set_manifest_repository_url, put, "/api/v1/applications/{id}/repository/url", (id: String = "id"), StringValue, |_, body: StringValue| ApplicationEdit::RepositoryUrl(body.value));
edit_endpoint!(set_manifest_repository_sync, put, "/api/v1/applications/{id}/repository/sync", (id: String = "id"), RepositorySyncValue, |_, body: RepositorySyncValue| ApplicationEdit::RepositorySync(body.value));
edit_endpoint!(set_manifest_repository_path, put, "/api/v1/applications/{id}/repository/path", (id: String = "id"), StringValue, |_, body: StringValue| ApplicationEdit::RepositoryPath(body.value));
edit_endpoint!(add_application_service, post, "/api/v1/applications/{id}/services", (id: String = "id"), Service, |_, body: Service| ApplicationEdit::AddService(Box::new(body)));
edit_endpoint!(add_application_volume, post, "/api/v1/applications/{id}/volumes", (id: String = "id"), Volume, |_, body: Volume| ApplicationEdit::AddVolume(body));
edit_endpoint!(
    /// Removes a service from the saved configuration.
    ///
    /// Pass `deploy=true` to also deploy the result (202 instead of 200).
    remove_application_service, delete, "/api/v1/applications/{id}/services/{service}", (id: String = "id", service: String = "service"), remove, |(_, service)| ApplicationEdit::RemoveService(service));
edit_endpoint!(
    /// Removes a named volume from the saved configuration.
    ///
    /// Pass `deploy=true` to also deploy the result (202 instead of 200).
    remove_application_volume, delete, "/api/v1/applications/{id}/volumes/{volume}", (id: String = "id", volume: String = "volume"), remove, |(_, volume)| ApplicationEdit::RemoveVolume(volume));
edit_endpoint!(set_service_name, put, "/api/v1/applications/{id}/services/{service}/name", (id: String = "id", service: String = "service"), StringValue, |(_, service), body: StringValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::Name(body.value) });
edit_endpoint!(set_service_source, put, "/api/v1/applications/{id}/services/{service}/source", (id: String = "id", service: String = "service"), SourceValue, |(_, service), body: SourceValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::Source(body.value) });
edit_endpoint!(set_service_image, put, "/api/v1/applications/{id}/services/{service}/source/image", (id: String = "id", service: String = "service"), TemplateValue, |(_, service), body: TemplateValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::Image(body.value) });
edit_endpoint!(set_service_git_url, put, "/api/v1/applications/{id}/services/{service}/source/git/url", (id: String = "id", service: String = "service"), StringValue, |(_, service), body: StringValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::GitUrl(body.value) });
edit_endpoint!(set_service_git_branch, put, "/api/v1/applications/{id}/services/{service}/source/git/branch", (id: String = "id", service: String = "service"), StringValue, |(_, service), body: StringValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::GitBranch(body.value) });
edit_endpoint!(set_service_git_commit, put, "/api/v1/applications/{id}/services/{service}/source/git/commit", (id: String = "id", service: String = "service"), OptionalStringValue, |(_, service), body: OptionalStringValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::GitCommit(body.value) });
edit_endpoint!(set_service_dockerfile, put, "/api/v1/applications/{id}/services/{service}/source/git/dockerfile", (id: String = "id", service: String = "service"), TemplateValue, |(_, service), body: TemplateValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::Dockerfile(body.value) });
edit_endpoint!(set_service_context, put, "/api/v1/applications/{id}/services/{service}/source/git/context", (id: String = "id", service: String = "service"), TemplateValue, |(_, service), body: TemplateValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::Context(body.value) });
edit_endpoint!(set_service_replicas, put, "/api/v1/applications/{id}/services/{service}/replicas", (id: String = "id", service: String = "service"), ReplicasValue, |(_, service), body: ReplicasValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::Replicas(body.value) });
edit_endpoint!(set_service_environment, put, "/api/v1/applications/{id}/services/{service}/environment", (id: String = "id", service: String = "service"), EnvironmentValue, |(_, service), body: EnvironmentValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::Environment(body.value) });
edit_endpoint!(set_service_command, put, "/api/v1/applications/{id}/services/{service}/command", (id: String = "id", service: String = "service"), TemplatesValue, |(_, service), body: TemplatesValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::Command(body.value) });
edit_endpoint!(set_service_arguments, put, "/api/v1/applications/{id}/services/{service}/arguments", (id: String = "id", service: String = "service"), TemplatesValue, |(_, service), body: TemplatesValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::Arguments(body.value) });
edit_endpoint!(set_service_mounts, put, "/api/v1/applications/{id}/services/{service}/mounts", (id: String = "id", service: String = "service"), MountsValue, |(_, service), body: MountsValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::Mounts(body.value) });
edit_endpoint!(set_service_files, put, "/api/v1/applications/{id}/services/{service}/files", (id: String = "id", service: String = "service"), FilesValue, |(_, service), body: FilesValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::Files(body.value) });
edit_endpoint!(set_service_depends_on, put, "/api/v1/applications/{id}/services/{service}/depends-on", (id: String = "id", service: String = "service"), StringsValue, |(_, service), body: StringsValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::DependsOn(body.value) });
edit_endpoint!(set_service_secrets, put, "/api/v1/applications/{id}/services/{service}/secrets", (id: String = "id", service: String = "service"), SecretsValue, |(_, service), body: SecretsValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::Secrets(body.value) });
edit_endpoint!(set_service_rollout, put, "/api/v1/applications/{id}/services/{service}/rollout", (id: String = "id", service: String = "service"), RolloutValue, |(_, service), body: RolloutValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::Rollout(body.value) });
edit_endpoint!(set_service_healthcheck, put, "/api/v1/applications/{id}/services/{service}/healthcheck", (id: String = "id", service: String = "service"), HealthValue, |(_, service), body: HealthValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::Healthcheck(body.value) });
edit_endpoint!(set_service_health_port, put, "/api/v1/applications/{id}/services/{service}/healthcheck/port", (id: String = "id", service: String = "service"), ReplicasValue, |(_, service), body: ReplicasValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::HealthPort(body.value) });
edit_endpoint!(set_service_health_path, put, "/api/v1/applications/{id}/services/{service}/healthcheck/path", (id: String = "id", service: String = "service"), TemplateValue, |(_, service), body: TemplateValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::HealthPath(body.value) });
edit_endpoint!(set_service_health_command, put, "/api/v1/applications/{id}/services/{service}/healthcheck/command", (id: String = "id", service: String = "service"), TemplatesValue, |(_, service), body: TemplatesValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::HealthCommand(body.value) });
edit_endpoint!(set_service_health_interval, put, "/api/v1/applications/{id}/services/{service}/healthcheck/interval", (id: String = "id", service: String = "service"), SecondsValue, |(_, service), body: SecondsValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::HealthInterval(body.value) });
edit_endpoint!(set_service_health_timeout, put, "/api/v1/applications/{id}/services/{service}/healthcheck/timeout", (id: String = "id", service: String = "service"), SecondsValue, |(_, service), body: SecondsValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::HealthTimeout(body.value) });
edit_endpoint!(set_service_resources, put, "/api/v1/applications/{id}/services/{service}/resources", (id: String = "id", service: String = "service"), ResourcesValue, |(_, service), body: ResourcesValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::Resources(body.value) });
edit_endpoint!(set_service_cpu, put, "/api/v1/applications/{id}/services/{service}/resources/cpu", (id: String = "id", service: String = "service"), CpuValue, |(_, service), body: CpuValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::Cpu(body.value) });
edit_endpoint!(set_service_memory, put, "/api/v1/applications/{id}/services/{service}/resources/memory", (id: String = "id", service: String = "service"), MemoryValue, |(_, service), body: MemoryValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::Memory(body.value) });
edit_endpoint!(set_service_general, put, "/api/v1/applications/{id}/services/{service}/general", (id: String = "id", service: String = "service"), ServiceGeneral, |(_, service), body: ServiceGeneral| ApplicationEdit::Service { name: service, edit: ServiceEdit::General(body) });
edit_endpoint!(set_service_process, put, "/api/v1/applications/{id}/services/{service}/process", (id: String = "id", service: String = "service"), ServiceProcess, |(_, service), body: ServiceProcess| ApplicationEdit::Service { name: service, edit: ServiceEdit::Process(body) });
edit_endpoint!(set_service_environment_entry, put, "/api/v1/applications/{id}/services/{service}/environment/{key}", (id: String = "id", service: String = "service", key: String = "key"), TemplateValue, |(_, service, key), body: TemplateValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::EnvironmentEntry((key, Some(body.value))) });
edit_endpoint!(
    /// Removes an environment variable from a service.
    ///
    /// Pass `deploy=true` to also deploy the result (202 instead of 200).
    remove_service_environment_entry, delete, "/api/v1/applications/{id}/services/{service}/environment/{key}", (id: String = "id", service: String = "service", key: String = "key"), remove, |(_, service, key)| ApplicationEdit::Service { name: service, edit: ServiceEdit::EnvironmentEntry((key, None)) });
edit_endpoint!(set_service_mount, put, "/api/v1/applications/{id}/services/{service}/mount", (id: String = "id", service: String = "service"), Mount, |(_, service), body: Mount| ApplicationEdit::Service { name: service, edit: ServiceEdit::Mount(body) });
edit_endpoint!(remove_service_mount, delete, "/api/v1/applications/{id}/services/{service}/mount", (id: String = "id", service: String = "service"), StringValue, |(_, service), body: StringValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::RemoveMount(body.value) });
edit_endpoint!(set_service_file, put, "/api/v1/applications/{id}/services/{service}/file", (id: String = "id", service: String = "service"), ServiceFile, |(_, service), body: ServiceFile| ApplicationEdit::Service { name: service, edit: ServiceEdit::File(body) });
edit_endpoint!(remove_service_file, delete, "/api/v1/applications/{id}/services/{service}/file", (id: String = "id", service: String = "service"), StringValue, |(_, service), body: StringValue| ApplicationEdit::Service { name: service, edit: ServiceEdit::RemoveFile(body.value) });

/// Query options for `create_application`; `deploy` defaults to false.
#[derive(Default, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
struct CreateQuery {
    deploy: bool,
}

impl QueryParams for CreateQuery {
    const INVALID_CODE: &'static str = "query_invalid";
    const INVALID_MESSAGE: &'static str = "deploy must be true or false";
}

/// Creates an empty application.
///
/// The body holds only the name, which must not already be in use (409
/// otherwise). Pass `deploy=true` to also deploy it (202 instead of 200).
#[utoipa::path(post, path = "/api/v1/applications", operation_id = "create_application",
    params(("deploy" = Option<bool>, Query), ("Idempotency-Key" = Option<String>, Header)), request_body = StringValue,
    responses((status = 200, description = "Empty application saved", body = Envelope<SavedApplication>),
        (status = 202, description = "Deployment accepted", body = Envelope<SavedApplication>),
        (status = 400, response = inline(ApiErrorResponse)), (status = 409, response = inline(ApiErrorResponse)),
        (status = 413, response = inline(ApiErrorResponse)), (status = 415, response = inline(ApiErrorResponse)),
        (status = 422, response = inline(ApiErrorResponse)), (status = 500, response = inline(ApiErrorResponse)), (status = 503, response = inline(ApiErrorResponse))))]
async fn create_application(
    State(state): State<ApiState>,
    Extension(identity): Extension<Identity>,
    ApiQuery(query): ApiQuery<CreateQuery>,
    headers: HeaderMap,
    ApiJson(name): ApiJson<StringValue>,
) -> Result<Response, ApiError> {
    let manifest = piqueld_core::manifest::ApplicationManifest {
        api_version: piqueld_core::manifest::APPLICATION_API_VERSION.into(),
        kind: piqueld_core::manifest::APPLICATION_KIND.into(),
        metadata: piqueld_core::manifest::Metadata { name: name.value },
        spec: piqueld_core::manifest::ApplicationSpec::default(),
    }
    .validate_template()?;
    accept_mutation(
        &state,
        &identity,
        Mutation::save(manifest, None, query.deploy),
        Some(0),
        false,
        &headers,
    )
    .await
}

/// Registers every typed editing endpoint, which requires `apps:write` on its
/// application, plus `create_application`.
pub(super) fn router() -> OpenApiRouter<ApiState> {
    OpenApiRouter::new()
        .routes(granted!(Global(AppsCreate) => create_application))
        .routes(granted!(App(Write) => set_application_volumes))
        .routes(granted!(App(Write) => set_application_routes))
        .routes(granted!(App(Write) => set_application_jobs))
        .routes(granted!(App(Write) => set_application_exports))
        .routes(granted!(App(Write) => set_application_imports))
        .routes(granted!(App(Write) => set_application_name))
        .routes(granted!(App(Write) => set_application_variables))
        .routes(granted!(App(Write) => set_environment_visibility))
        .routes(granted!(App(Write) => set_manifest_repository))
        .routes(granted!(App(Write) => disconnect_manifest_repository))
        .routes(granted!(App(Write) => set_manifest_repository_url))
        .routes(granted!(App(Write) => set_manifest_repository_sync))
        .routes(granted!(App(Write) => set_manifest_repository_path))
        .routes(granted!(App(Write) => add_application_service))
        .routes(granted!(App(Write) => add_application_volume))
        .routes(granted!(App(Write) => remove_application_service))
        .routes(granted!(App(Write) => remove_application_volume))
        .routes(granted!(App(Write) => set_service_name))
        .routes(granted!(App(Write) => set_service_source))
        .routes(granted!(App(Write) => set_service_image))
        .routes(granted!(App(Write) => set_service_git_url))
        .routes(granted!(App(Write) => set_service_git_branch))
        .routes(granted!(App(Write) => set_service_git_commit))
        .routes(granted!(App(Write) => set_service_dockerfile))
        .routes(granted!(App(Write) => set_service_context))
        .routes(granted!(App(Write) => set_service_replicas))
        .routes(granted!(App(Write) => set_service_environment))
        .routes(granted!(App(Write) => set_service_command))
        .routes(granted!(App(Write) => set_service_arguments))
        .routes(granted!(App(Write) => set_service_mounts))
        .routes(granted!(App(Write) => set_service_secrets))
        .routes(granted!(App(Write) => set_service_files))
        .routes(granted!(App(Write) => set_service_depends_on))
        .routes(granted!(App(Write) => set_service_rollout))
        .routes(granted!(App(Write) => set_service_healthcheck))
        .routes(granted!(App(Write) => set_service_health_port))
        .routes(granted!(App(Write) => set_service_health_path))
        .routes(granted!(App(Write) => set_service_health_command))
        .routes(granted!(App(Write) => set_service_health_interval))
        .routes(granted!(App(Write) => set_service_health_timeout))
        .routes(granted!(App(Write) => set_service_resources))
        .routes(granted!(App(Write) => set_service_cpu))
        .routes(granted!(App(Write) => set_service_memory))
        .routes(granted!(App(Write) => set_service_general))
        .routes(granted!(App(Write) => set_service_process))
        .routes(granted!(App(Write) => set_service_environment_entry))
        .routes(granted!(App(Write) => remove_service_environment_entry))
        .routes(granted!(App(Write) => set_service_mount))
        .routes(granted!(App(Write) => remove_service_mount))
        .routes(granted!(App(Write) => set_service_file))
        .routes(granted!(App(Write) => remove_service_file))
}
