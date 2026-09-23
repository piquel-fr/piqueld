//! Thin convenience methods over generated field editing operations.
use crate::{Client, ClientError, SavedApplication, client::generated_result};
use piqueld_core::{
    edit::{
        ApplicationEdit, CpuValue, EditOptions, EnvironmentValue, HealthValue, MemoryValue,
        MountsValue, OptionalStringValue, ReplicasValue, RepositoryValue, ResourcesValue,
        RoutesValue, SecondsValue, ServiceEdit, ServiceGeneral, ServiceProcess, SourceValue,
        StringValue, StringsValue, VolumesValue,
    },
    manifest::{Mount, Service, Volume},
};

// The generated client owns paths, encoding, request bodies, and response types.
macro_rules! edit_method {
    ($name:ident, ($($path:ident),+) $(, $request:ident: $body:ty)?) => {
        impl Client {
            #[doc = concat!("Calls the generated `", stringify!($name), "` endpoint.")]
            /// # Errors
            /// Returns transport, validation, revision, or resource errors.
            pub async fn $name(&self, $($path: &str,)+ $($request: &$body,)? options: &EditOptions) -> Result<SavedApplication, ClientError> {
                generated_result(self.generated.$name($($path,)+ Some(options.deploy), options.expected_generation, Some(options.force), None $(, $request)?).await)
                    .await.map(|response| response.data)
            }
        }
    };
}
edit_method!(set_application_volumes, (id), request: VolumesValue);
edit_method!(set_application_routes, (id), request: RoutesValue);
edit_method!(set_application_name, (id), request: StringValue);
edit_method!(set_manifest_repository, (id), request: RepositoryValue);
edit_method!(disconnect_manifest_repository, (id));
edit_method!(set_manifest_repository_url, (id), request: StringValue);
edit_method!(set_manifest_repository_branch, (id), request: StringValue);
edit_method!(set_manifest_repository_commit, (id), request: OptionalStringValue);
edit_method!(set_manifest_repository_path, (id), request: StringValue);
edit_method!(add_application_service, (id), request: Service);
edit_method!(add_application_volume, (id), request: Volume);
edit_method!(remove_application_service, (id, service));
edit_method!(remove_application_volume, (id, volume));
edit_method!(set_service_name, (id, service), request: StringValue);
edit_method!(set_service_source, (id, service), request: SourceValue);
edit_method!(set_service_image, (id, service), request: StringValue);
edit_method!(set_service_git_url, (id, service), request: StringValue);
edit_method!(set_service_git_branch, (id, service), request: StringValue);
edit_method!(set_service_git_commit, (id, service), request: OptionalStringValue);
edit_method!(set_service_dockerfile, (id, service), request: StringValue);
edit_method!(set_service_context, (id, service), request: StringValue);
edit_method!(set_service_replicas, (id, service), request: ReplicasValue);
edit_method!(set_service_environment, (id, service), request: EnvironmentValue);
edit_method!(set_service_command, (id, service), request: StringsValue);
edit_method!(set_service_arguments, (id, service), request: StringsValue);
edit_method!(set_service_mounts, (id, service), request: MountsValue);
edit_method!(set_service_healthcheck, (id, service), request: HealthValue);
edit_method!(set_service_health_port, (id, service), request: ReplicasValue);
edit_method!(set_service_health_path, (id, service), request: StringValue);
edit_method!(set_service_health_command, (id, service), request: StringsValue);
edit_method!(set_service_health_interval, (id, service), request: SecondsValue);
edit_method!(set_service_health_timeout, (id, service), request: SecondsValue);
edit_method!(set_service_resources, (id, service), request: ResourcesValue);
edit_method!(set_service_cpu, (id, service), request: CpuValue);
edit_method!(set_service_memory, (id, service), request: MemoryValue);
edit_method!(set_service_general, (id, service), request: ServiceGeneral);
edit_method!(set_service_process, (id, service), request: ServiceProcess);
edit_method!(set_service_environment_entry, (id, service, key), request: StringValue);
edit_method!(remove_service_environment_entry, (id, service, key));
edit_method!(set_service_mount, (id, service), request: Mount);
edit_method!(remove_service_mount, (id, service), request: StringValue);

impl Client {
    /// Creates an empty application, requiring the name to be absent.
    /// # Errors
    /// Returns transport, validation, or name collision errors.
    pub async fn create_application(
        &self,
        name: &str,
        deploy: bool,
    ) -> Result<SavedApplication, ClientError> {
        generated_result(
            self.generated
                .create_application(Some(deploy), None, &StringValue { value: name.into() })
                .await,
        )
        .await
        .map(|response| response.data)
    }
    /// Sends a typed change through its individual generated endpoint.
    /// # Errors
    /// Returns transport, validation, revision, or resource errors.
    pub async fn edit_application(
        &self,
        id: &str,
        edit: &ApplicationEdit,
        options: &EditOptions,
    ) -> Result<SavedApplication, ClientError> {
        macro_rules! send {
            ($method:ident, $body:ident, $value:expr) => {
                self.$method(id, &$body { value: $value }, options).await
            };
        }
        match edit {
            ApplicationEdit::Volumes(value) => {
                send!(set_application_volumes, VolumesValue, value.clone())
            }
            ApplicationEdit::Routes(value) => {
                send!(set_application_routes, RoutesValue, value.clone())
            }
            ApplicationEdit::Name(value) => send!(set_application_name, StringValue, value.clone()),
            ApplicationEdit::Repository(None) => {
                self.disconnect_manifest_repository(id, options).await
            }
            ApplicationEdit::Repository(value @ Some(_)) => {
                send!(set_manifest_repository, RepositoryValue, value.clone())
            }
            ApplicationEdit::RepositoryUrl(value) => {
                send!(set_manifest_repository_url, StringValue, value.clone())
            }
            ApplicationEdit::RepositoryBranch(value) => {
                send!(set_manifest_repository_branch, StringValue, value.clone())
            }
            ApplicationEdit::RepositoryCommit(value) => send!(
                set_manifest_repository_commit,
                OptionalStringValue,
                value.clone()
            ),
            ApplicationEdit::RepositoryPath(value) => {
                send!(set_manifest_repository_path, StringValue, value.clone())
            }
            ApplicationEdit::AddService(value) => {
                self.add_application_service(id, value, options).await
            }
            ApplicationEdit::AddVolume(value) => {
                self.add_application_volume(id, value, options).await
            }
            ApplicationEdit::RemoveVolume(value) => {
                self.remove_application_volume(id, value, options).await
            }
            ApplicationEdit::RemoveService(name) => {
                self.remove_application_service(id, name, options).await
            }
            ApplicationEdit::Service { name, edit } => {
                self.edit_service(id, name, edit, options).await
            }
        }
    }
    async fn edit_service(
        &self,
        id: &str,
        service: &str,
        edit: &ServiceEdit,
        options: &EditOptions,
    ) -> Result<SavedApplication, ClientError> {
        macro_rules! send {
            ($method:ident, $body:ident, $value:expr) => {
                self.$method(id, service, &$body { value: $value }, options)
                    .await
            };
        }
        match edit {
            ServiceEdit::Name(value) => send!(set_service_name, StringValue, value.clone()),
            ServiceEdit::Source(value) => send!(set_service_source, SourceValue, value.clone()),
            ServiceEdit::Image(value) => send!(set_service_image, StringValue, value.clone()),
            ServiceEdit::GitUrl(value) => send!(set_service_git_url, StringValue, value.clone()),
            ServiceEdit::GitBranch(value) => {
                send!(set_service_git_branch, StringValue, value.clone())
            }
            ServiceEdit::GitCommit(value) => {
                send!(set_service_git_commit, OptionalStringValue, value.clone())
            }
            ServiceEdit::Dockerfile(value) => {
                send!(set_service_dockerfile, StringValue, value.clone())
            }
            ServiceEdit::Context(value) => send!(set_service_context, StringValue, value.clone()),
            ServiceEdit::Replicas(value) => send!(set_service_replicas, ReplicasValue, *value),
            ServiceEdit::Environment(value) => {
                send!(set_service_environment, EnvironmentValue, value.clone())
            }
            ServiceEdit::Command(value) => send!(set_service_command, StringsValue, value.clone()),
            ServiceEdit::Arguments(value) => {
                send!(set_service_arguments, StringsValue, value.clone())
            }
            ServiceEdit::Mounts(value) => send!(set_service_mounts, MountsValue, value.clone()),
            ServiceEdit::Healthcheck(value) => {
                send!(set_service_healthcheck, HealthValue, value.clone())
            }
            ServiceEdit::HealthPort(value) => send!(set_service_health_port, ReplicasValue, *value),
            ServiceEdit::HealthPath(value) => {
                send!(set_service_health_path, StringValue, value.clone())
            }
            ServiceEdit::HealthCommand(value) => {
                send!(set_service_health_command, StringsValue, value.clone())
            }
            ServiceEdit::HealthInterval(value) => {
                send!(set_service_health_interval, SecondsValue, *value)
            }
            ServiceEdit::HealthTimeout(value) => {
                send!(set_service_health_timeout, SecondsValue, *value)
            }
            ServiceEdit::Resources(value) => {
                send!(set_service_resources, ResourcesValue, value.clone())
            }
            ServiceEdit::Cpu(value) => send!(set_service_cpu, CpuValue, *value),
            ServiceEdit::Memory(value) => send!(set_service_memory, MemoryValue, *value),
            ServiceEdit::General(value) => {
                self.set_service_general(id, service, value, options).await
            }
            ServiceEdit::Process(value) => {
                self.set_service_process(id, service, value, options).await
            }
            ServiceEdit::EnvironmentEntry((key, Some(value))) => {
                self.set_service_environment_entry(
                    id,
                    service,
                    key,
                    &StringValue {
                        value: value.clone(),
                    },
                    options,
                )
                .await
            }
            ServiceEdit::EnvironmentEntry((key, None)) => {
                self.remove_service_environment_entry(id, service, key, options)
                    .await
            }
            ServiceEdit::Mount(value) => self.set_service_mount(id, service, value, options).await,
            ServiceEdit::RemoveMount(value) => {
                send!(remove_service_mount, StringValue, value.clone())
            }
        }
    }
}
