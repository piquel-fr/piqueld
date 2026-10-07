//! Typed values owned by validated applications, exposed through immutable accessors.

use super::input::{self, GitRepository, JobRun, RepositoryManifest, SourceRepository};
use super::variables::{Template, Typed};
use super::{RolloutPolicy, ValidatedRollout, ValidationError, ValidationErrors};
use crate::{ApplicationName, JobName, ServiceName, VolumeName, codes};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use utoipa::ToSchema;

/// Validated application metadata.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, ToSchema)]
pub struct ValidatedMetadata {
    /// User-facing application name.
    pub name: ApplicationName,
}

/// Validated application resource lists.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, ToSchema)]
pub struct ValidatedSpec {
    /// Optional repository that supplies this application's manifest on Deploy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest: Option<RepositoryManifest>,
    /// Declared services.
    pub services: Vec<ValidatedService>,
    /// Declared named volumes.
    pub volumes: Vec<ValidatedVolume>,
    /// Exact public HTTP routes.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<super::ValidatedRoute>,
    /// Secrets whose values piqueld generates once.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<input::SecretDeclaration>,
    /// One-shot jobs in declared execution order.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub jobs: Vec<ValidatedJob>,
}

/// Validated one-shot job.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, ToSchema)]
pub struct ValidatedJob {
    /// Logical job name.
    pub name: JobName,
    /// Service whose prepared container settings the job reuses.
    pub service: ServiceName,
    /// Command replacing the service's command and arguments.
    pub command: Vec<String>,
    /// Deployment point at which the job runs.
    pub run: JobRun,
    /// Seconds the job may run before the deployment fails.
    pub timeout_seconds: u32,
}

/// Validated service source.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum ValidatedSource {
    /// Pull a prebuilt image from a registry.
    Image {
        /// Image reference.
        image: String,
    },
    /// Build a checked-out Git revision.
    Git {
        /// Repository and revision to resolve.
        repository: SourceRepository,
        /// Explicit build instructions.
        build: ValidatedBuild,
    },
}

impl ValidatedSource {
    /// Replaces a `"self"` repository with `manifest`, the repository it refers to.
    pub(crate) fn resolve_manifest_repository(&mut self, manifest: &GitRepository) {
        if let Self::Git { repository, .. } = self {
            repository.resolve_manifest(manifest);
        }
    }
}

/// Validated build instructions.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum ValidatedBuild {
    /// Build a local container image using Docker.
    Docker {
        /// Dockerfile path relative to the repository root.
        dockerfile: String,
        /// Build context relative to the repository root.
        context: String,
        /// Values passed as `--build-arg`.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        args: BTreeMap<String, String>,
        /// Multi-stage build target; the final stage when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
    },
}

/// Validated container health check.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum ValidatedHealthCheck {
    /// HTTP health endpoint check.
    Http {
        /// Container port to probe.
        #[schema(maximum = 65_535)]
        port: u16,
        /// HTTP path to probe.
        path: String,
        /// Probe interval in seconds.
        interval_seconds: u32,
        /// Probe timeout in seconds.
        timeout_seconds: u32,
    },
    /// Executable command health check.
    Command {
        /// Command and arguments to execute.
        command: Vec<String>,
        /// Probe interval in seconds.
        interval_seconds: u32,
        /// Probe timeout in seconds.
        timeout_seconds: u32,
    },
}

/// What the container runtime executes for a health check. Two checks with the
/// same execution behave identically, whichever variant declared them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HealthExecution {
    /// Executable and arguments, run directly without a shell.
    pub command: Vec<String>,
    /// Probe interval in seconds.
    pub interval_seconds: u32,
    /// Probe timeout in seconds.
    pub timeout_seconds: u32,
}

impl ValidatedHealthCheck {
    /// Probe interval applied when a check declares none.
    pub const DEFAULT_INTERVAL_SECONDS: u32 = 10;
    /// Probe timeout applied when a check declares none.
    pub const DEFAULT_TIMEOUT_SECONDS: u32 = 3;

    /// Returns the direct command and timing Docker runs for this check.
    /// HTTP checks become the equivalent `wget` probe.
    #[must_use]
    pub fn execution(&self) -> HealthExecution {
        match self {
            Self::Command {
                command,
                interval_seconds,
                timeout_seconds,
            } => HealthExecution {
                command: command.clone(),
                interval_seconds: *interval_seconds,
                timeout_seconds: *timeout_seconds,
            },
            Self::Http {
                port,
                path,
                interval_seconds,
                timeout_seconds,
            } => HealthExecution {
                command: vec![
                    "wget".into(),
                    "-q".into(),
                    "-T".into(),
                    timeout_seconds.to_string(),
                    "-O".into(),
                    "/dev/null".into(),
                    format!("http://127.0.0.1:{port}{path}"),
                ],
                interval_seconds: *interval_seconds,
                timeout_seconds: *timeout_seconds,
            },
        }
    }
}

/// Validated CPU and memory limits.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ValidatedResourceLimits {
    /// CPU limit in millicores.
    pub cpu_millis: Option<u32>,
    /// Memory limit in bytes.
    #[schema(minimum = 1, maximum = 9_223_372_036_854_775_807_u64)]
    pub memory_bytes: Option<u64>,
}

/// Validated application service.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, ToSchema)]
pub struct ValidatedService {
    /// Logical service name.
    pub name: ServiceName,
    /// Explicit image or build source.
    pub source: ValidatedSource,
    /// Desired replica count.
    pub replicas: u16,
    /// Environment variables keyed by name.
    pub environment: BTreeMap<String, String>,
    /// Container entrypoint command.
    pub command: Vec<String>,
    /// Arguments passed to the command.
    pub arguments: Vec<String>,
    /// Persistent volume mounts.
    pub mounts: Vec<ValidatedMount>,
    /// Application-scoped secret file mounts.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<input::SecretMount>,
    /// Optional container health check.
    pub healthcheck: Option<ValidatedHealthCheck>,
    /// Optional CPU and memory limits.
    pub resources: Option<ValidatedResourceLimits>,
    /// Services in this application that must be healthy before this one rolls out.
    /// Omitted when empty so existing specification hashes stay stable.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<ServiceName>,
    /// Rollout settings. Omitted when default so existing specification hashes stay stable.
    #[serde(skip_serializing_if = "ValidatedRollout::is_default")]
    pub rollout: ValidatedRollout,
}

/// Validated named volume.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
pub struct ValidatedVolume {
    /// Logical volume name.
    pub name: VolumeName,
}

/// A persistent volume mount in a service.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
pub struct ValidatedMount {
    /// Referenced logical volume name.
    pub volume: VolumeName,
    /// Container target path.
    pub target: String,
    /// Whether the mount is read-only.
    pub read_only: bool,
}

impl ValidationErrors {
    /// Wraps a typed name parse failure as a single `NAME_INVALID` error at `path`.
    pub(super) fn invalid_name(path: impl Into<String>, source: impl std::fmt::Display) -> Self {
        Self(vec![ValidationError {
            code: codes::NAME_INVALID.into(),
            path: path.into(),
            message: source.to_string(),
        }])
    }

    /// A value at `path` that still references variables.
    fn unresolved(path: impl Into<String>) -> Self {
        Self(vec![ValidationError {
            code: codes::VARIABLE_UNRESOLVED.into(),
            path: path.into(),
            message: "references must be rendered for an environment first".into(),
        }])
    }
}

impl Template {
    /// The literal text, or an unresolved-reference error at `path`.
    fn into_text(self, path: impl FnOnce() -> String) -> Result<String, ValidationErrors> {
        self.as_literal()
            .ok_or_else(|| ValidationErrors::unresolved(path()))
    }

    /// Converts each element; `base` locates error paths.
    fn into_texts(values: Vec<Self>, base: &str) -> Result<Vec<String>, ValidationErrors> {
        values
            .into_iter()
            .enumerate()
            .map(|(index, value)| value.into_text(|| format!("{base}[{index}]")))
            .collect()
    }
}

impl<T> Typed<T> {
    /// The literal value, or an unresolved-reference error at `path`.
    fn into_value(self, path: impl FnOnce() -> String) -> Result<T, ValidationErrors> {
        match self {
            Self::Literal(value) => Ok(value),
            Self::Template(_) => Err(ValidationErrors::unresolved(path())),
        }
    }
}

impl ValidatedSource {
    /// Requires literal source settings; `base` locates error paths.
    fn from_input(value: input::Source, base: &str) -> Result<Self, ValidationErrors> {
        Ok(match value {
            input::Source::Image { image } => Self::Image {
                image: image.into_text(|| format!("{base}.image"))?,
            },
            input::Source::Git {
                repository,
                build:
                    input::Build::Docker {
                        dockerfile,
                        context,
                        args,
                        target,
                    },
            } => Self::Git {
                repository,
                build: ValidatedBuild::Docker {
                    dockerfile: dockerfile.into_text(|| format!("{base}.build.dockerfile"))?,
                    context: context.into_text(|| format!("{base}.build.context"))?,
                    args: args
                        .into_iter()
                        .map(|(key, value)| {
                            let text = value.into_text(|| format!("{base}.build.args.{key}"))?;
                            Ok((key, text))
                        })
                        .collect::<Result<_, ValidationErrors>>()?,
                    target: target
                        .map(|target| target.into_text(|| format!("{base}.build.target")))
                        .transpose()?,
                },
            },
        })
    }

    /// Converts back to the editable input shape used for export.
    #[must_use]
    pub fn to_input(&self) -> input::Source {
        match self {
            Self::Image { image } => input::Source::Image {
                image: Template::literal(image),
            },
            Self::Git {
                repository,
                build:
                    ValidatedBuild::Docker {
                        dockerfile,
                        context,
                        args,
                        target,
                    },
            } => input::Source::Git {
                repository: repository.clone(),
                build: input::Build::Docker {
                    dockerfile: Template::literal(dockerfile),
                    context: Template::literal(context),
                    args: args
                        .iter()
                        .map(|(key, value)| (key.clone(), Template::literal(value)))
                        .collect(),
                    target: target.as_deref().map(Template::literal),
                },
            },
        }
    }
}

impl ValidatedHealthCheck {
    /// Requires literal settings; `base` locates error paths.
    fn from_input(value: input::HealthCheck, base: &str) -> Result<Self, ValidationErrors> {
        let path = |field: &'static str| move || format!("{base}.{field}");
        Ok(match value {
            input::HealthCheck::Http {
                port,
                path: request_path,
                interval_seconds,
                timeout_seconds,
            } => Self::Http {
                port: port.into_value(path("port"))?,
                path: request_path.into_text(path("path"))?,
                interval_seconds: interval_seconds.into_value(path("interval_seconds"))?,
                timeout_seconds: timeout_seconds.into_value(path("timeout_seconds"))?,
            },
            input::HealthCheck::Command {
                command,
                interval_seconds,
                timeout_seconds,
            } => Self::Command {
                command: Template::into_texts(command, &format!("{base}.command"))?,
                interval_seconds: interval_seconds.into_value(path("interval_seconds"))?,
                timeout_seconds: timeout_seconds.into_value(path("timeout_seconds"))?,
            },
        })
    }

    /// Converts back to the editable input shape used for export.
    #[must_use]
    pub fn to_input(&self) -> input::HealthCheck {
        match self {
            Self::Http {
                port,
                path,
                interval_seconds,
                timeout_seconds,
            } => input::HealthCheck::Http {
                port: (*port).into(),
                path: Template::literal(path),
                interval_seconds: (*interval_seconds).into(),
                timeout_seconds: (*timeout_seconds).into(),
            },
            Self::Command {
                command,
                interval_seconds,
                timeout_seconds,
            } => input::HealthCheck::Command {
                command: command
                    .iter()
                    .map(|value| Template::literal(value))
                    .collect(),
                interval_seconds: (*interval_seconds).into(),
                timeout_seconds: (*timeout_seconds).into(),
            },
        }
    }
}

impl ValidatedResourceLimits {
    /// Requires literal limits; `base` locates error paths.
    fn from_input(value: input::ResourceLimits, base: &str) -> Result<Self, ValidationErrors> {
        Ok(Self {
            cpu_millis: value
                .cpu_millis
                .map(|cpu| cpu.into_value(|| format!("{base}.cpu_millis")))
                .transpose()?,
            memory_bytes: value
                .memory_bytes
                .map(|memory| memory.into_value(|| format!("{base}.memory_bytes")))
                .transpose()?,
        })
    }

    /// Converts back to the editable input shape used for export.
    #[must_use]
    pub fn to_input(&self) -> input::ResourceLimits {
        input::ResourceLimits {
            cpu_millis: self.cpu_millis.map(Typed::Literal),
            memory_bytes: self.memory_bytes.map(Typed::Literal),
        }
    }
}

impl ValidatedRollout {
    /// Requires literal settings; `base` locates error paths.
    fn from_input(value: super::Rollout, base: &str) -> Result<Self, ValidationErrors> {
        Ok(Self {
            order: value
                .order
                .map(|order| order.into_value(|| format!("{base}.order")))
                .transpose()?,
            monitor_seconds: value
                .monitor_seconds
                .map(|seconds| seconds.into_value(|| format!("{base}.monitor_seconds")))
                .transpose()?,
        })
    }
}

impl ValidatedMetadata {
    /// Parses the application name into its typed form.
    pub(super) fn from_input(value: input::Metadata) -> Result<Self, ValidationErrors> {
        Ok(Self {
            name: ApplicationName::parse(value.name)
                .map_err(|source| ValidationErrors::invalid_name("metadata.name", source))?,
        })
    }
}

impl ValidatedSpec {
    /// Converts semantically validated input into typed domain values.
    ///
    /// Runs after `ApplicationManifest::validate` has accepted the input, so
    /// failures here are a backstop; the first failing field is returned.
    pub(super) fn from_input(value: input::ApplicationSpec) -> Result<Self, ValidationErrors> {
        Ok(Self {
            routes: value
                .routes
                .into_iter()
                .enumerate()
                .map(|(index, route)| super::ValidatedRoute::from_input(route, index))
                .collect::<Result<_, _>>()?,
            jobs: value
                .jobs
                .into_iter()
                .enumerate()
                .map(|(index, job)| ValidatedJob::from_input(job, index))
                .collect::<Result<_, _>>()?,
            manifest: value.manifest,
            secrets: value.secrets,
            services: value
                .services
                .into_iter()
                .enumerate()
                .map(|(index, service)| ValidatedService::from_input(service, index))
                .collect::<Result<_, _>>()?,
            volumes: value
                .volumes
                .into_iter()
                .enumerate()
                .map(|(index, volume)| {
                    Ok(ValidatedVolume {
                        name: VolumeName::parse(volume.name).map_err(|source| {
                            ValidationErrors::invalid_name(
                                format!("spec.volumes[{index}].name"),
                                source,
                            )
                        })?,
                    })
                })
                .collect::<Result<_, ValidationErrors>>()?,
        })
    }

    /// Names of the application secrets that at least one service mounts.
    #[must_use]
    pub fn mounted_secret_names(&self) -> BTreeSet<&str> {
        self.services
            .iter()
            .flat_map(|service| service.secrets.iter().map(|secret| secret.name.as_str()))
            .collect()
    }

    /// Whether any service builds from `"self"`, the manifest's own repository.
    #[must_use]
    pub fn builds_from_manifest(&self) -> bool {
        self.services.iter().any(|service| {
            matches!(
                &service.source,
                ValidatedSource::Git {
                    repository: SourceRepository::Manifest(_),
                    ..
                }
            )
        })
    }

    /// Converts back to the editable input shape used for export and plans.
    #[must_use]
    pub fn to_input(&self) -> input::ApplicationSpec {
        input::ApplicationSpec {
            routes: self
                .routes
                .iter()
                .map(super::ValidatedRoute::to_input)
                .collect(),
            jobs: self.jobs.iter().map(ValidatedJob::to_input).collect(),
            manifest: self.manifest.clone(),
            secrets: self.secrets.clone(),
            variables: std::collections::BTreeMap::new(),
            environments: std::collections::BTreeMap::new(),
            services: self
                .services
                .iter()
                .map(ValidatedService::to_input)
                .collect(),
            volumes: self
                .volumes
                .iter()
                .map(|volume| input::Volume {
                    name: volume.name.to_string(),
                })
                .collect(),
        }
    }
}

impl ValidatedJob {
    /// Parses the job and referenced service names; `index` locates error paths.
    fn from_input(value: input::Job, index: usize) -> Result<Self, ValidationErrors> {
        let path = format!("spec.jobs[{index}]");
        Ok(Self {
            name: JobName::parse(value.name)
                .map_err(|source| ValidationErrors::invalid_name(format!("{path}.name"), source))?,
            service: ServiceName::parse(value.service).map_err(|source| {
                ValidationErrors::invalid_name(format!("{path}.service"), source)
            })?,
            command: Template::into_texts(value.command, &format!("{path}.command"))?,
            run: value.run,
            timeout_seconds: value.timeout_seconds,
        })
    }

    /// Converts back to the editable input shape used for export.
    fn to_input(&self) -> input::Job {
        input::Job {
            name: self.name.to_string(),
            service: self.service.to_string(),
            command: self
                .command
                .iter()
                .map(|value| Template::literal(value))
                .collect(),
            run: self.run,
            timeout_seconds: self.timeout_seconds,
        }
    }
}

impl ValidatedService {
    /// Parses the service and mount volume names; `index` locates error paths.
    fn from_input(value: input::Service, index: usize) -> Result<Self, ValidationErrors> {
        let base = format!("spec.services[{index}]");
        Ok(Self {
            name: ServiceName::parse(value.name)
                .map_err(|source| ValidationErrors::invalid_name(format!("{base}.name"), source))?,
            source: ValidatedSource::from_input(value.source, &format!("{base}.source"))?,
            replicas: value.replicas.into_value(|| format!("{base}.replicas"))?,
            environment: value
                .environment
                .into_iter()
                .map(|(key, value)| {
                    let text = value.into_text(|| format!("{base}.environment.{key}"))?;
                    Ok((key, text))
                })
                .collect::<Result<_, ValidationErrors>>()?,
            command: Template::into_texts(value.command, &format!("{base}.command"))?,
            arguments: Template::into_texts(value.arguments, &format!("{base}.arguments"))?,
            secrets: value.secrets,
            mounts: value
                .mounts
                .into_iter()
                .enumerate()
                .map(|(mount_index, mount)| {
                    Ok(ValidatedMount {
                        volume: VolumeName::parse(mount.volume).map_err(|source| {
                            ValidationErrors::invalid_name(
                                format!("spec.services[{index}].mounts[{mount_index}].volume"),
                                source,
                            )
                        })?,
                        target: mount.target,
                        read_only: mount.read_only,
                    })
                })
                .collect::<Result<_, ValidationErrors>>()?,
            healthcheck: value
                .healthcheck
                .map(|check| {
                    ValidatedHealthCheck::from_input(check, &format!("{base}.healthcheck"))
                })
                .transpose()?,
            resources: value
                .resources
                .map(|limits| {
                    ValidatedResourceLimits::from_input(limits, &format!("{base}.resources"))
                })
                .transpose()?,
            depends_on: value
                .depends_on
                .into_iter()
                .enumerate()
                .map(|(dependency_index, name)| {
                    ServiceName::parse(name).map_err(|source| {
                        ValidationErrors::invalid_name(
                            format!("spec.services[{index}].depends_on[{dependency_index}]"),
                            source,
                        )
                    })
                })
                .collect::<Result<_, ValidationErrors>>()?,
            rollout: ValidatedRollout::from_input(value.rollout, &format!("{base}.rollout"))?,
        })
    }

    /// The effective rollout policy; see [`ValidatedRollout::policy`].
    #[must_use]
    pub fn rollout_policy(&self) -> RolloutPolicy {
        self.rollout.policy(self.read_only_mounts())
    }

    /// Whether an explicit start-first order lets two tasks share a writable volume.
    #[must_use]
    pub fn rollout_overlaps_writable_volume(&self) -> bool {
        self.rollout
            .overlaps_writable_volume(self.read_only_mounts())
    }

    /// Read-only flags of the mounts, from which the default rollout order derives.
    fn read_only_mounts(&self) -> impl Iterator<Item = bool> + '_ {
        self.mounts.iter().map(|mount| mount.read_only)
    }

    /// Converts back to the editable input shape used for export.
    fn to_input(&self) -> input::Service {
        input::Service {
            name: self.name.to_string(),
            source: self.source.to_input(),
            replicas: self.replicas.into(),
            environment: self
                .environment
                .iter()
                .map(|(key, value)| (key.clone(), Template::literal(value)))
                .collect(),
            command: self
                .command
                .iter()
                .map(|value| Template::literal(value))
                .collect(),
            arguments: self
                .arguments
                .iter()
                .map(|value| Template::literal(value))
                .collect(),
            secrets: self.secrets.clone(),
            mounts: self
                .mounts
                .iter()
                .map(|mount| input::Mount {
                    volume: mount.volume.to_string(),
                    target: mount.target.clone(),
                    read_only: mount.read_only,
                })
                .collect(),
            healthcheck: self
                .healthcheck
                .as_ref()
                .map(ValidatedHealthCheck::to_input),
            resources: self
                .resources
                .as_ref()
                .map(ValidatedResourceLimits::to_input),
            depends_on: self.depends_on.iter().map(ToString::to_string).collect(),
            rollout: self.rollout.into(),
        }
    }
}
