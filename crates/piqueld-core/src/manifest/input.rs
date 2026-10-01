//! Public manifest input and export shapes, before semantic validation.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use utoipa::ToSchema;

/// Strict public application manifest request and export shape.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ApplicationManifest {
    /// API version string.
    pub api_version: String,
    /// Resource kind string.
    pub kind: String,
    /// User-provided metadata.
    pub metadata: Metadata,
    /// Desired application resources.
    pub spec: ApplicationSpec,
}

/// User-provided application metadata.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Metadata {
    /// User-facing application name.
    pub name: String,
}

/// User-provided application resource lists.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ApplicationSpec {
    /// Optional repository that supplies this application's manifest on Deploy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest: Option<RepositoryManifest>,
    /// Declared services.
    pub services: Vec<Service>,
    /// Declared named volumes.
    pub volumes: Vec<Volume>,
    /// Exact public HTTP routes, activated on deployment.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<Route>,
    /// One-shot jobs, run in declared order at their deployment point.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub jobs: Vec<Job>,
}

/// Independently selects the manifest used by a manual deployment.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RepositoryManifest {
    /// Repository and revision containing the manifest.
    pub repository: GitRepository,
    /// Exact TOML or JSON file path relative to the repository root.
    pub path: String,
}

/// User-declared application service.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Service {
    /// Logical service name.
    pub name: String,
    /// Explicit image or build source.
    pub source: Source,
    /// Desired replica count.
    #[serde(default = "default_replicas")]
    pub replicas: u16,
    /// Environment variables keyed by name.
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    /// Container entrypoint command.
    #[serde(default)]
    pub command: Vec<String>,
    /// Arguments passed to the command.
    #[serde(default)]
    pub arguments: Vec<String>,
    /// Persistent volume mounts.
    #[serde(default)]
    pub mounts: Vec<Mount>,
    /// Application-scoped secrets mounted as files.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<SecretMount>,
    /// Optional container health check.
    pub healthcheck: Option<HealthCheck>,
    /// Optional CPU and memory limits.
    pub resources: Option<ResourceLimits>,
}

fn default_replicas() -> u16 {
    1
}

/// A container that runs to completion at a defined point in each deployment.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Job {
    /// Logical job name.
    pub name: String,
    /// Service whose prepared image, environment, secrets, and mounts the job reuses.
    pub service: String,
    /// Command replacing the service's command and arguments.
    pub command: Vec<String>,
    /// Deployment point at which the job runs.
    pub run: JobRun,
    /// Seconds the job may run before the deployment fails.
    #[serde(default = "default_job_timeout")]
    pub timeout_seconds: u32,
}

/// Deployment point at which a job runs.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum JobRun {
    /// After sources are prepared, before the deployment changes any service.
    BeforeRollout,
}

fn default_job_timeout() -> u32 {
    300
}

/// The exhaustive set of deployable service sources.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum Source {
    /// Pull a prebuilt image from a registry.
    Image {
        /// Image reference.
        image: String,
    },
    /// Build a checked-out Git revision.
    Git {
        /// Repository and revision to resolve.
        repository: GitRepository,
        /// Explicit build instructions.
        build: Build,
    },
}

/// Git checkout configuration. Credentials come from the host's Git configuration.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct GitRepository {
    /// Git clone URL or local repository path.
    pub url: String,
    /// Branch to fetch when no commit is pinned.
    pub branch: String,
    /// Optional full commit hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

/// Explicit build backend, extensible independently from source selection.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum Build {
    /// Build a local container image using Docker.
    Docker {
        /// Dockerfile path relative to the repository root.
        dockerfile: String,
        /// Build context relative to the repository root.
        #[serde(default = "default_build_context")]
        context: String,
    },
}

fn default_build_context() -> String {
    ".".into()
}

/// User-declared named volume.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Volume {
    /// Logical volume name.
    pub name: String,
}

/// A persistent volume mount in a service.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Mount {
    /// Referenced logical volume name.
    pub volume: String,
    /// Container target path.
    pub target: String,
    /// Whether the mount is read-only.
    #[serde(default)]
    pub read_only: bool,
}

/// User-declared container health check.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum HealthCheck {
    /// HTTP health endpoint check.
    Http {
        /// Container port to probe.
        port: u16,
        /// HTTP path to probe.
        #[serde(default = "default_health_path")]
        path: String,
        /// Probe interval in seconds.
        #[serde(default = "default_interval")]
        interval_seconds: u32,
        /// Probe timeout in seconds.
        #[serde(default = "default_timeout")]
        timeout_seconds: u32,
    },
    /// Executable command health check.
    Command {
        /// Command and arguments to execute.
        command: Vec<String>,
        /// Probe interval in seconds.
        #[serde(default = "default_interval")]
        interval_seconds: u32,
        /// Probe timeout in seconds.
        #[serde(default = "default_timeout")]
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

impl HealthCheck {
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

fn default_health_path() -> String {
    "/health".into()
}

fn default_interval() -> u32 {
    10
}

fn default_timeout() -> u32 {
    3
}

/// Optional CPU and memory limits for a service.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ResourceLimits {
    /// CPU limit in millicores.
    pub cpu_millis: Option<u32>,
    /// Memory limit in bytes.
    #[schema(minimum = 1, maximum = 9_223_372_036_854_775_807_u64)]
    pub memory_bytes: Option<u64>,
}

/// A logical application secret exposed only as a container file.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Ord, PartialOrd, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SecretMount {
    /// Application-scoped logical secret name.
    pub name: String,
    /// Absolute normalized destination under /run/secrets.
    pub target: String,
}

/// Public HTTP route input, validated independently of ingress enablement.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Route {
    /// Exact public DNS hostname.
    pub hostname: String,
    /// Logical service in this application.
    pub service: String,
    /// Internal HTTP backend port.
    #[schema(minimum = 1)]
    pub port: u16,
}
