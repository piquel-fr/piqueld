//! Typed values owned by validated applications, exposed through immutable accessors.

use super::input::{
    self, GitRepository, JobRun, JobVolumeAccess, RepositoryManifest, SourceRepository,
};
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
    /// Services other applications may reach over private links.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub exports: Vec<super::ValidatedExport>,
    /// Services of other applications this application reaches.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub imports: Vec<super::ValidatedImport>,
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
    /// Environment variables added to the service's. Omitted when empty so
    /// existing specification hashes stay stable.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub environment: BTreeMap<String, String>,
    /// Secrets added to the service's. Omitted when empty so existing
    /// specification hashes stay stable.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<ValidatedSecretMount>,
    /// Deployment point at which the job runs.
    pub run: JobRun,
    /// Seconds the job may run before the deployment fails.
    pub timeout_seconds: u32,
    /// How the job may use the service's volumes. The default is omitted, so
    /// spec hashes of applications that never set it stay unchanged.
    #[serde(skip_serializing_if = "JobVolumeAccess::is_default")]
    pub volume_access: JobVolumeAccess,
    /// Whether the job's service stops before the job runs. Omitted when
    /// unset, so spec hashes of applications that never set it stay
    /// unchanged.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub stop_service: bool,
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
        /// Startup grace in seconds, during which failed probes do not count.
        /// Omitted when zero, so targets keep their encoding.
        #[serde(default, skip_serializing_if = "ValidatedHealthCheck::is_zero")]
        start_period_seconds: u32,
        /// Consecutive failed probes that make the container unhealthy.
        /// Omitted when the default.
        #[serde(
            default = "ValidatedHealthCheck::default_retries",
            skip_serializing_if = "ValidatedHealthCheck::is_default_retries"
        )]
        retries: u32,
    },
    /// Executable command health check.
    Command {
        /// Command and arguments to execute.
        command: Vec<String>,
        /// Probe interval in seconds.
        interval_seconds: u32,
        /// Probe timeout in seconds.
        timeout_seconds: u32,
        /// Startup grace in seconds, during which failed probes do not count.
        /// Omitted when zero, so targets keep their encoding.
        #[serde(default, skip_serializing_if = "ValidatedHealthCheck::is_zero")]
        start_period_seconds: u32,
        /// Consecutive failed probes that make the container unhealthy.
        /// Omitted when the default.
        #[serde(
            default = "ValidatedHealthCheck::default_retries",
            skip_serializing_if = "ValidatedHealthCheck::is_default_retries"
        )]
        retries: u32,
    },
}

/// What the container runtime executes for a health check. Two checks with the
/// same execution behave identically, whichever variant declared them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HealthExecution {
    /// Executable and arguments, run directly without a shell.
    pub command: Vec<String>,
    /// When the command runs and how its failures count.
    pub timing: HealthTiming,
}

/// When a health check probes and how many failures it tolerates, whichever
/// variant declared it: values, or references to a manifest's settings.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HealthTiming<T = u32> {
    /// Probe interval in seconds.
    pub interval_seconds: T,
    /// Probe timeout in seconds.
    pub timeout_seconds: T,
    /// Startup grace in seconds, during which failed probes do not count.
    pub start_period_seconds: T,
    /// Consecutive failed probes that make the container unhealthy.
    pub retries: T,
}

impl<T> HealthTiming<T> {
    /// Each setting with its manifest field name.
    pub fn fields(self) -> [(&'static str, T); 4] {
        [
            ("interval_seconds", self.interval_seconds),
            ("timeout_seconds", self.timeout_seconds),
            ("start_period_seconds", self.start_period_seconds),
            ("retries", self.retries),
        ]
    }
}

impl HealthExecution {
    /// Whether the check runs `piqueld-probe`, which must then be mounted at
    /// [`piqueld_probe::CONTAINER_PATH`].
    #[must_use]
    pub fn runs_probe(&self) -> bool {
        self.command.first().map(String::as_str) == Some(piqueld_probe::CONTAINER_PATH)
    }
}

impl ValidatedHealthCheck {
    /// Probe interval applied when a check declares none.
    pub const DEFAULT_INTERVAL_SECONDS: u32 = 10;
    /// Probe timeout applied when a check declares none.
    pub const DEFAULT_TIMEOUT_SECONDS: u32 = 3;
    /// Startup grace applied when a check declares none.
    pub const DEFAULT_START_PERIOD_SECONDS: u32 = 0;
    /// Consecutive failures tolerated when a check declares no retries.
    pub const DEFAULT_RETRIES: u32 = 3;

    /// Returns the direct command and timing Docker runs for this check.
    /// HTTP checks run `piqueld-probe`, which piqueld mounts into the container.
    #[must_use]
    pub fn execution(&self) -> HealthExecution {
        let timing = self.timing();
        let command = match self {
            Self::Command { command, .. } => command.clone(),
            Self::Http { port, path, .. } => piqueld_probe::HttpProbe {
                port: *port,
                path: path.clone(),
                timeout_seconds: timing.timeout_seconds,
            }
            .command(),
        };
        HealthExecution { command, timing }
    }

    /// The check's timing, whichever variant declared it.
    #[must_use]
    pub const fn timing(&self) -> HealthTiming {
        match *self {
            Self::Http {
                interval_seconds,
                timeout_seconds,
                start_period_seconds,
                retries,
                ..
            }
            | Self::Command {
                interval_seconds,
                timeout_seconds,
                start_period_seconds,
                retries,
                ..
            } => HealthTiming {
                interval_seconds,
                timeout_seconds,
                start_period_seconds,
                retries,
            },
        }
    }

    /// Serde default for `retries`.
    const fn default_retries() -> u32 {
        Self::DEFAULT_RETRIES
    }

    /// Whether `retries` is the default, which is not serialized.
    #[expect(
        clippy::trivially_copy_pass_by_ref,
        reason = "serde's skip_serializing_if passes a reference"
    )]
    const fn is_default_retries(retries: &u32) -> bool {
        *retries == Self::DEFAULT_RETRIES
    }

    /// Whether `seconds` is zero, which is not serialized.
    #[expect(
        clippy::trivially_copy_pass_by_ref,
        reason = "serde's skip_serializing_if passes a reference"
    )]
    const fn is_zero(seconds: &u32) -> bool {
        *seconds == 0
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
    /// Secret file mounts.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<ValidatedSecretMount>,
    /// Read-only files. Omitted when empty so existing specification hashes stay stable.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<super::ServiceFile>,
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
    /// Whether the service receives the daemon's telemetry once its
    /// environment is granted it. Omitted when false so existing
    /// specification hashes stay stable.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub receives_telemetry: bool,
}

/// A rendered secret file mount.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
pub struct ValidatedSecretMount {
    /// Logical secret name.
    pub name: String,
    /// Absolute normalized destination under /run/secrets.
    pub target: String,
}

impl ValidatedSecretMount {
    /// Converts the secret mounts of the service or job at `base`, whose
    /// names must be rendered.
    fn from_inputs(
        values: Vec<input::SecretMount>,
        base: &str,
    ) -> Result<Vec<Self>, ValidationErrors> {
        values
            .into_iter()
            .enumerate()
            .map(|(index, secret)| {
                Ok(Self {
                    name: secret
                        .name
                        .into_text(|| format!("{base}.secrets[{index}].name"))?,
                    target: secret.target,
                })
            })
            .collect()
    }

    /// Converts back to the editable input shape used for export.
    fn to_inputs(values: &[Self]) -> Vec<input::SecretMount> {
        values
            .iter()
            .map(|secret| input::SecretMount {
                name: Template::literal(&secret.name),
                target: secret.target.clone(),
            })
            .collect()
    }
}

/// Where a mounted secret's value comes from.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum SecretSource {
    /// Generated for each environment, because `spec.secrets` declares it.
    Generated,
    /// Set manually in the application's secret store, subject to its access list.
    Stored,
}

impl SecretSource {
    /// Where the value of the secret `name` comes from, given the secrets
    /// `spec.secrets` declares.
    pub(super) fn of(name: &str, declared: &[input::SecretDeclaration]) -> Self {
        if declared.iter().any(|secret| secret.name == name) {
            Self::Generated
        } else {
            Self::Stored
        }
    }
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
        ValidationError::new(codes::NAME_INVALID, path, source.to_string()).into()
    }
}

impl ValidationError {
    /// A value at `path` that still references variables.
    pub(super) fn unresolved(path: impl Into<String>) -> Self {
        Self::new(
            codes::VARIABLE_UNRESOLVED,
            path,
            "references must be rendered for an environment first",
        )
    }
}

impl Template {
    /// The literal text, or an unresolved-reference error at `path`.
    fn into_text(self, path: impl FnOnce() -> String) -> Result<String, ValidationErrors> {
        self.as_literal()
            .ok_or_else(|| ValidationError::unresolved(path()).into())
    }

    /// Converts each element; `base` locates error paths.
    fn into_texts(values: Vec<Self>, base: &str) -> Result<Vec<String>, ValidationErrors> {
        values
            .into_iter()
            .enumerate()
            .map(|(index, value)| value.into_text(|| format!("{base}[{index}]")))
            .collect()
    }

    /// Converts each map value, keeping its key; `base` locates error paths.
    fn into_text_map(
        values: BTreeMap<String, Self>,
        base: &str,
    ) -> Result<BTreeMap<String, String>, ValidationErrors> {
        values
            .into_iter()
            .map(|(key, value)| {
                let text = value.into_text(|| format!("{base}.{key}"))?;
                Ok((key, text))
            })
            .collect()
    }

    /// The literal template of each value, as exports write them.
    fn literals(values: &[String]) -> Vec<Self> {
        values.iter().map(|value| Self::literal(value)).collect()
    }

    /// The literal template of each map value, keeping its key.
    fn literal_map(values: &BTreeMap<String, String>) -> BTreeMap<String, Self> {
        values
            .iter()
            .map(|(key, value)| (key.clone(), Self::literal(value)))
            .collect()
    }
}

impl<T> Typed<T> {
    /// The literal value, or an unresolved-reference error at `path`.
    fn into_value(self, path: impl FnOnce() -> String) -> Result<T, ValidationErrors> {
        match self {
            Self::Literal(value) => Ok(value),
            Self::Template(_) => Err(ValidationError::unresolved(path()).into()),
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
                    args: Template::literal_map(args),
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
                start_period_seconds,
                retries,
            } => Self::Http {
                port: port.into_value(path("port"))?,
                path: request_path.into_text(path("path"))?,
                interval_seconds: interval_seconds.into_value(path("interval_seconds"))?,
                timeout_seconds: timeout_seconds.into_value(path("timeout_seconds"))?,
                start_period_seconds: start_period_seconds
                    .into_value(path("start_period_seconds"))?,
                retries: retries.into_value(path("retries"))?,
            },
            input::HealthCheck::Command {
                command,
                interval_seconds,
                timeout_seconds,
                start_period_seconds,
                retries,
            } => Self::Command {
                command: Template::into_texts(command, &format!("{base}.command"))?,
                interval_seconds: interval_seconds.into_value(path("interval_seconds"))?,
                timeout_seconds: timeout_seconds.into_value(path("timeout_seconds"))?,
                start_period_seconds: start_period_seconds
                    .into_value(path("start_period_seconds"))?,
                retries: retries.into_value(path("retries"))?,
            },
        })
    }

    /// Converts back to the editable input shape used for export.
    #[must_use]
    pub fn to_input(&self) -> input::HealthCheck {
        let timing = self.timing();
        match self {
            Self::Http { port, path, .. } => input::HealthCheck::Http {
                port: (*port).into(),
                path: Template::literal(path),
                interval_seconds: timing.interval_seconds.into(),
                timeout_seconds: timing.timeout_seconds.into(),
                start_period_seconds: timing.start_period_seconds.into(),
                retries: timing.retries.into(),
            },
            Self::Command { command, .. } => input::HealthCheck::Command {
                command: Template::literals(command),
                interval_seconds: timing.interval_seconds.into(),
                timeout_seconds: timing.timeout_seconds.into(),
                start_period_seconds: timing.start_period_seconds.into(),
                retries: timing.retries.into(),
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
            exports: value
                .exports
                .into_iter()
                .enumerate()
                .map(|(index, export)| export.into_validated(index))
                .collect::<Result<_, _>>()?,
            imports: value
                .imports
                .into_iter()
                .enumerate()
                .map(|(index, import)| import.into_validated(index))
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

    /// Names of the secrets that at least one service or job mounts.
    #[must_use]
    pub fn mounted_secret_names(&self) -> BTreeSet<&str> {
        self.services
            .iter()
            .flat_map(|service| &service.secrets)
            .chain(self.jobs.iter().flat_map(|job| &job.secrets))
            .map(|secret| secret.name.as_str())
            .collect()
    }

    /// Each mounted secret and where its value comes from: generated when
    /// `spec.secrets` declares it, stored otherwise.
    #[must_use]
    pub fn mounted_secrets(&self) -> BTreeMap<&str, SecretSource> {
        self.mounted_secret_names()
            .into_iter()
            .map(|name| (name, self.secret_source(name)))
            .collect()
    }

    /// Where the value of the secret `name` comes from.
    #[must_use]
    pub fn secret_source(&self, name: &str) -> SecretSource {
        SecretSource::of(name, &self.secrets)
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
            exports: self
                .exports
                .iter()
                .map(super::ValidatedExport::to_input)
                .collect(),
            imports: self
                .imports
                .iter()
                .map(super::ValidatedImport::to_input)
                .collect(),
            manifest: self.manifest.clone(),
            secrets: self.secrets.clone(),
            variables: BTreeMap::new(),
            environments: BTreeMap::new(),
            previews: input::PreviewConfig::default(),
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
            environment: Template::into_text_map(
                value.environment,
                &format!("{path}.environment"),
            )?,
            secrets: ValidatedSecretMount::from_inputs(value.secrets, &path)?,
            run: value.run,
            timeout_seconds: value.timeout_seconds,
            volume_access: value.volume_access,
            stop_service: value.stop_service,
        })
    }

    /// Converts back to the editable input shape used for export.
    fn to_input(&self) -> input::Job {
        input::Job {
            name: self.name.to_string(),
            service: self.service.to_string(),
            command: Template::literals(&self.command),
            environment: Template::literal_map(&self.environment),
            secrets: ValidatedSecretMount::to_inputs(&self.secrets),
            run: self.run,
            timeout_seconds: self.timeout_seconds,
            volume_access: self.volume_access,
            stop_service: self.stop_service,
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
            environment: Template::into_text_map(
                value.environment,
                &format!("{base}.environment"),
            )?,
            command: Template::into_texts(value.command, &format!("{base}.command"))?,
            arguments: Template::into_texts(value.arguments, &format!("{base}.arguments"))?,
            secrets: ValidatedSecretMount::from_inputs(value.secrets, &base)?,
            files: value.files,
            mounts: value
                .mounts
                .into_iter()
                .enumerate()
                .map(|(mount_index, mount)| {
                    Ok(ValidatedMount {
                        volume: VolumeName::parse(mount.volume).map_err(|source| {
                            ValidationErrors::invalid_name(
                                format!("{base}.mounts[{mount_index}].volume"),
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
                            format!("{base}.depends_on[{dependency_index}]"),
                            source,
                        )
                    })
                })
                .collect::<Result<_, ValidationErrors>>()?,
            rollout: ValidatedRollout::from_input(value.rollout, &format!("{base}.rollout"))?,
            receives_telemetry: value.receives_telemetry,
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

    /// Read-only flags of the mounts, from which the default rollout order
    /// derives. A telemetry receiver's socket counts as a writable mount:
    /// two collectors can't listen on it at once.
    fn read_only_mounts(&self) -> impl Iterator<Item = bool> + '_ {
        self.mounts
            .iter()
            .map(|mount| mount.read_only)
            .chain(self.receives_telemetry.then_some(false))
    }

    /// Converts back to the editable input shape used for export.
    fn to_input(&self) -> input::Service {
        input::Service {
            name: self.name.to_string(),
            source: self.source.to_input(),
            replicas: self.replicas.into(),
            environment: Template::literal_map(&self.environment),
            command: Template::literals(&self.command),
            arguments: Template::literals(&self.arguments),
            secrets: ValidatedSecretMount::to_inputs(&self.secrets),
            files: self.files.clone(),
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
            receives_telemetry: self.receives_telemetry,
        }
    }
}
