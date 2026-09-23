//! Typed changes to saved application configuration. No edit performs runtime work.
use crate::manifest::{
    ApplicationManifest, Build, GitRepository, HealthCheck, Mount, RepositoryManifest,
    ResourceLimits, Route, Service, Source, Volume,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use utoipa::ToSchema;

macro_rules! value_request {
    ($($name:ident: $ty:ty;)*) => {$ (
        #[doc = concat!("Typed replacement value for `", stringify!($name), "`.")]
        #[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
        #[serde(deny_unknown_fields)]
        pub struct $name {
            /// New value; null explicitly clears an optional setting.
            #[serde(deserialize_with = "Deserialize::deserialize")]
            #[schema(required = true)]
            pub value: $ty,
        }
    )*};
}
value_request! {
    StringValue: String;
    OptionalStringValue: Option<String>;
    ReplicasValue: u16;
    SecondsValue: u32;
    CpuValue: Option<u32>;
    MemoryValue: Option<u64>;
    StringsValue: Vec<String>;
    SourceValue: Source;
    HealthValue: Option<HealthCheck>;
    ResourcesValue: Option<ResourceLimits>;
    EnvironmentValue: BTreeMap<String, String>;
    MountsValue: Vec<Mount>;
    VolumesValue: Vec<Volume>;
    RoutesValue: Vec<Route>;
    RepositoryValue: Option<RepositoryManifest>;
}

/// Source and scaling settings saved together by the dashboard form.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ServiceGeneral {
    /// Explicit source variant.
    pub source: Source,
    /// Desired replicas.
    pub replicas: u16,
}
/// Container entrypoint and arguments saved together.
#[derive(Clone, Debug, Deserialize, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ServiceProcess {
    /// Entrypoint elements.
    pub command: Vec<String>,
    /// Argument elements.
    pub arguments: Vec<String>,
}

/// An edit to an existing application, addressed by its stable identity.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "field", content = "value", rename_all = "snake_case")]
pub enum ApplicationEdit {
    /// Change application display name.
    Name(String),
    /// Connect or disconnect a repository manifest.
    Repository(Option<RepositoryManifest>),
    /// Change the repository URL.
    RepositoryUrl(String),
    /// Change the repository branch.
    RepositoryBranch(String),
    /// Pin or unpin the repository commit.
    RepositoryCommit(Option<String>),
    /// Change the manifest path.
    RepositoryPath(String),
    /// Add a new service, rejecting duplicate names.
    AddService(Service),
    /// Remove a service declaration.
    RemoveService(String),
    /// Edit one existing service.
    Service {
        /// Existing logical service name.
        name: String,
        /// Typed setting change.
        edit: ServiceEdit,
    },
    /// Add a named volume declaration.
    AddVolume(Volume),
    /// Replace the named volume declarations.
    Volumes(Vec<Volume>),
    /// Replace public routes owned by this application.
    Routes(Vec<Route>),
    /// Remove a volume declaration; validation rejects remaining mounts.
    RemoveVolume(String),
}

/// A change to one service. Nested edits require the corresponding source/check variant.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "field", content = "value", rename_all = "snake_case")]
pub enum ServiceEdit {
    /// Logical service name.
    Name(String),
    /// Switch source variant.
    Source(Source),
    /// Set an image source.
    Image(String),
    /// Git repository URL.
    GitUrl(String),
    /// Git branch.
    GitBranch(String),
    /// Pinned Git commit, or none.
    GitCommit(Option<String>),
    /// Dockerfile path.
    Dockerfile(String),
    /// Docker build context.
    Context(String),
    /// Desired replicas.
    Replicas(u16),
    /// Replace environment entries.
    Environment(BTreeMap<String, String>),
    /// Set or remove one environment variable.
    EnvironmentEntry((String, Option<String>)),
    /// Entrypoint elements.
    Command(Vec<String>),
    /// Argument elements.
    Arguments(Vec<String>),
    /// Replace mount declarations.
    Mounts(Vec<Mount>),
    /// Add or replace a mount at its target.
    Mount(Mount),
    /// Remove a mount by container target.
    RemoveMount(String),
    /// Switch or clear health check.
    Healthcheck(Option<HealthCheck>),
    /// HTTP health port.
    HealthPort(u16),
    /// HTTP health path.
    HealthPath(String),
    /// Health command elements.
    HealthCommand(Vec<String>),
    /// Health interval seconds.
    HealthInterval(u32),
    /// Health timeout seconds.
    HealthTimeout(u32),
    /// Replace resource limits.
    Resources(Option<ResourceLimits>),
    /// Set or clear CPU millicores.
    Cpu(Option<u32>),
    /// Set or clear memory bytes.
    Memory(Option<u64>),
    /// Source and replicas saved together.
    General(ServiceGeneral),
    /// Entrypoint and arguments saved together.
    Process(ServiceProcess),
}

/// Errors applying a structurally valid edit to the current saved configuration.
#[derive(Debug, thiserror::Error)]
pub enum EditError {
    /// The named service, volume, environment entry, or mount does not exist.
    #[error("{kind} {name:?} was not found")]
    NotFound {
        /// Resource kind.
        kind: &'static str,
        /// Requested resource name.
        name: String,
    },
    /// A new resource already has this name.
    #[error("{kind} {name:?} already exists")]
    AlreadyExists {
        /// Resource kind.
        kind: &'static str,
        /// Conflicting resource name.
        name: String,
    },
    /// A nested setting is unavailable for the currently selected variant.
    #[error("{0}")]
    Incompatible(&'static str),
}

impl ApplicationEdit {
    /// Whether this edit changes only repository connection settings.
    #[must_use]
    pub const fn is_repository_setting(&self) -> bool {
        matches!(
            self,
            Self::Repository(_)
                | Self::RepositoryUrl(_)
                | Self::RepositoryBranch(_)
                | Self::RepositoryCommit(_)
                | Self::RepositoryPath(_)
        )
    }

    /// Edits input in memory. The caller must validate the resulting manifest before saving.
    /// # Errors
    /// Rejects missing/duplicate resources and incompatible nested fields.
    pub fn apply(self, manifest: &mut ApplicationManifest) -> Result<(), EditError> {
        match self {
            Self::Name(name) => manifest.metadata.name = name,
            Self::Repository(value) => manifest.spec.manifest = value,
            Self::RepositoryUrl(value) => Self::repository(manifest)?.repository.url = value,
            Self::RepositoryBranch(value) => Self::repository(manifest)?.repository.branch = value,
            Self::RepositoryCommit(value) => Self::repository(manifest)?.repository.commit = value,
            Self::RepositoryPath(value) => Self::repository(manifest)?.path = value,
            Self::AddService(service) => {
                if manifest
                    .spec
                    .services
                    .iter()
                    .any(|s| s.name == service.name)
                {
                    return Err(EditError::AlreadyExists {
                        kind: "service",
                        name: service.name,
                    });
                }
                manifest.spec.services.push(service);
            }
            Self::RemoveService(name) => {
                let index = manifest
                    .spec
                    .services
                    .iter()
                    .position(|s| s.name == name)
                    .ok_or(EditError::NotFound {
                        kind: "service",
                        name: name.clone(),
                    })?;
                manifest.spec.services.remove(index);
                manifest.spec.routes.retain(|route| route.service != name);
            }
            Self::Service { name, edit } => {
                let renamed_to = match &edit {
                    ServiceEdit::Name(value) => Some(value.clone()),
                    _ => None,
                };
                let service = manifest
                    .spec
                    .services
                    .iter_mut()
                    .find(|s| s.name == name)
                    .ok_or(EditError::NotFound {
                        kind: "service",
                        name: name.clone(),
                    })?;
                edit.apply(service)?;
                if let Some(new_name) = renamed_to {
                    for route in &mut manifest.spec.routes {
                        if route.service == name {
                            route.service.clone_from(&new_name);
                        }
                    }
                }
            }
            Self::Volumes(volumes) => manifest.spec.volumes = volumes,
            Self::Routes(routes) => manifest.spec.routes = routes,
            Self::AddVolume(volume) => {
                if manifest.spec.volumes.iter().any(|v| v.name == volume.name) {
                    return Err(EditError::AlreadyExists {
                        kind: "volume",
                        name: volume.name,
                    });
                }
                manifest.spec.volumes.push(volume);
            }
            Self::RemoveVolume(name) => {
                let index = manifest
                    .spec
                    .volumes
                    .iter()
                    .position(|v| v.name == name)
                    .ok_or(EditError::NotFound {
                        kind: "volume",
                        name,
                    })?;
                manifest.spec.volumes.remove(index);
            }
        }
        Ok(())
    }
    fn repository(
        manifest: &mut ApplicationManifest,
    ) -> Result<&mut RepositoryManifest, EditError> {
        manifest
            .spec
            .manifest
            .as_mut()
            .ok_or(EditError::Incompatible(
                "connect a manifest repository before editing its settings",
            ))
    }
}
impl ServiceEdit {
    fn apply(self, service: &mut Service) -> Result<(), EditError> {
        match self {
            Self::Name(value) => service.name = value,
            Self::Source(value) => service.source = value,
            Self::Image(image) => service.source = Source::Image { image },
            Self::GitUrl(value) => Self::git(service)?.url = value,
            Self::GitBranch(value) => Self::git(service)?.branch = value,
            Self::GitCommit(value) => Self::git(service)?.commit = value,
            Self::Dockerfile(value) => {
                let Build::Docker { dockerfile, .. } = Self::build(service)?;
                *dockerfile = value;
            }
            Self::Context(value) => {
                let Build::Docker { context, .. } = Self::build(service)?;
                *context = value;
            }
            Self::Replicas(value) => service.replicas = value,
            Self::Environment(value) => service.environment = value,
            Self::EnvironmentEntry((key, value)) => match value {
                Some(value) => {
                    service.environment.insert(key, value);
                }
                None => {
                    service
                        .environment
                        .remove(&key)
                        .ok_or(EditError::NotFound {
                            kind: "environment variable",
                            name: key,
                        })?;
                }
            },
            Self::Command(value) => service.command = value,
            Self::Arguments(value) => service.arguments = value,
            Self::Mounts(value) => service.mounts = value,
            Self::Mount(value) => Self::set_mount(service, value),
            Self::RemoveMount(target) => Self::remove_mount(service, target)?,
            Self::Healthcheck(value) => service.healthcheck = value,
            Self::HealthPort(value) => match Self::health(service)? {
                HealthCheck::Http { port, .. } => *port = value,
                HealthCheck::Command { .. } => {
                    return Err(EditError::Incompatible(
                        "port requires an HTTP health check",
                    ));
                }
            },
            Self::HealthPath(value) => match Self::health(service)? {
                HealthCheck::Http { path, .. } => *path = value,
                HealthCheck::Command { .. } => {
                    return Err(EditError::Incompatible(
                        "path requires an HTTP health check",
                    ));
                }
            },
            Self::HealthCommand(value) => match Self::health(service)? {
                HealthCheck::Command { command, .. } => *command = value,
                HealthCheck::Http { .. } => {
                    return Err(EditError::Incompatible(
                        "command requires a command health check",
                    ));
                }
            },
            Self::HealthInterval(value) => match Self::health(service)? {
                HealthCheck::Http {
                    interval_seconds, ..
                }
                | HealthCheck::Command {
                    interval_seconds, ..
                } => *interval_seconds = value,
            },
            Self::HealthTimeout(value) => match Self::health(service)? {
                HealthCheck::Http {
                    timeout_seconds, ..
                }
                | HealthCheck::Command {
                    timeout_seconds, ..
                } => *timeout_seconds = value,
            },
            Self::Resources(value) => service.resources = value,
            Self::Cpu(value) => {
                Self::resources(service).cpu_millis = value;
                Self::clear_empty_resources(service);
            }
            Self::Memory(value) => {
                Self::resources(service).memory_bytes = value;
                Self::clear_empty_resources(service);
            }
            Self::General(value) => {
                service.source = value.source;
                service.replicas = value.replicas;
            }
            Self::Process(value) => {
                service.command = value.command;
                service.arguments = value.arguments;
            }
        }
        Ok(())
    }
    fn resources(service: &mut Service) -> &mut ResourceLimits {
        service.resources.get_or_insert(ResourceLimits {
            cpu_millis: None,
            memory_bytes: None,
        })
    }
    fn set_mount(service: &mut Service, value: Mount) {
        if let Some(mount) = service.mounts.iter_mut().find(|m| m.target == value.target) {
            *mount = value;
        } else {
            service.mounts.push(value);
        }
    }
    fn remove_mount(service: &mut Service, target: String) -> Result<(), EditError> {
        let index = service
            .mounts
            .iter()
            .position(|m| m.target == target)
            .ok_or(EditError::NotFound {
                kind: "mount",
                name: target,
            })?;
        service.mounts.remove(index);
        Ok(())
    }
    fn git(service: &mut Service) -> Result<&mut GitRepository, EditError> {
        match &mut service.source {
            Source::Git { repository, .. } => Ok(repository),
            Source::Image { .. } => Err(EditError::Incompatible(
                "select a Git source before editing Git settings",
            )),
        }
    }
    fn build(service: &mut Service) -> Result<&mut Build, EditError> {
        match &mut service.source {
            Source::Git { build, .. } => Ok(build),
            Source::Image { .. } => Err(EditError::Incompatible(
                "select a Git source before editing build settings",
            )),
        }
    }
    fn health(service: &mut Service) -> Result<&mut HealthCheck, EditError> {
        service.healthcheck.as_mut().ok_or(EditError::Incompatible(
            "configure a health check before editing its settings",
        ))
    }
    fn clear_empty_resources(service: &mut Service) {
        if service
            .resources
            .as_ref()
            .is_some_and(|r| r.cpu_millis.is_none() && r.memory_bytes.is_none())
        {
            service.resources = None;
        }
    }
}

/// Revision protection and deployment policy for a field edit.
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, utoipa::IntoParams)]
#[serde(default, deny_unknown_fields)]
#[into_params(parameter_in = Query)]
pub struct EditOptions {
    /// Required inspected generation unless force is set.
    pub expected_generation: Option<u64>,
    /// Explicitly bypass the revision check.
    pub force: bool,
    /// Deploy the changed configuration; omitted/false saves only.
    pub deploy: bool,
}

#[cfg(test)]
mod tests {
    use super::{ApplicationEdit, ServiceEdit};
    use crate::manifest::{ApplicationManifest, ApplicationSpec, Metadata, Route, Service, Source};

    #[test]
    fn service_edits_keep_public_routes_attached_to_existing_services() {
        let mut manifest = ApplicationManifest {
            api_version: crate::manifest::APPLICATION_API_VERSION.into(),
            kind: crate::manifest::APPLICATION_KIND.into(),
            metadata: Metadata { name: "app".into() },
            spec: ApplicationSpec {
                services: vec![Service {
                    name: "web".into(),
                    source: Source::Image {
                        image: "nginx:alpine".into(),
                    },
                    replicas: 1,
                    environment: std::collections::BTreeMap::default(),
                    command: vec![],
                    arguments: vec![],
                    mounts: vec![],
                    healthcheck: None,
                    resources: None,
                }],
                ..ApplicationSpec::default()
            },
        };
        ApplicationEdit::Routes(vec![Route {
            hostname: "app.example.com".into(),
            service: "web".into(),
            port: 3000,
        }])
        .apply(&mut manifest)
        .unwrap();
        ApplicationEdit::Service {
            name: "web".into(),
            edit: ServiceEdit::Name("frontend".into()),
        }
        .apply(&mut manifest)
        .unwrap();
        assert_eq!(manifest.spec.routes[0].service, "frontend");
        manifest.clone().validate().unwrap();
        ApplicationEdit::RemoveService("frontend".into())
            .apply(&mut manifest)
            .unwrap();
        assert!(manifest.spec.routes.is_empty());
    }
}
