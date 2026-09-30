//! Backend-neutral desired, resolved, and observed Docker resource contracts.

use crate::manifest::domain::{ValidatedMount as Mount, ValidatedService as Service};
use crate::names::validated_string;
use crate::{
    ApplicationId, ApplicationName, DockerNetworkName, DockerServiceName, DockerVolumeName,
    ResourceKind, ServiceName, VolumeName, docker_resource_name,
    manifest::{HealthCheck, NormalizedApplication, ResourceLimits, Source, valid_image_reference},
};
use crate::{ImageReference, ImmutableImage, RepositoryDigest};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use utoipa::ToSchema;

/// Label marking a resource as managed by piqueld.
pub const MANAGED_LABEL: &str = "io.piqueld.managed";
/// Label carrying the control-plane instance identity.
pub const INSTANCE_LABEL: &str = "io.piqueld.instance";
/// Label carrying the application identity.
pub const APPLICATION_LABEL: &str = "io.piqueld.application";
/// Label carrying the logical service identity.
pub const SERVICE_LABEL: &str = "io.piqueld.service";
/// Label carrying the normalized application spec hash.
pub const SPEC_HASH_LABEL: &str = "io.piqueld.spec-hash";

validated_string!(
    /// Stable control-plane instance identity.
    InstanceId, InstanceIdError,
    "instance IDs must be 1-64 lowercase ASCII letters, digits, or internal hyphens",
    |value: &str| (1..=64).contains(&value.len())
            && value
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            && value
                .bytes()
                .next()
                .is_some_and(|byte| byte.is_ascii_alphanumeric())
            && value
                .bytes()
                .last()
                .is_some_and(|byte| byte.is_ascii_alphanumeric())
);

validated_string!(
    /// Explicitly tagged lowercase SHA-256 digest.
    Sha256Digest, Sha256DigestError,
    "SHA-256 digests must use the sha256:<64 lowercase hexadecimal digits> format",
    valid_sha256
);

/// Immutable image resolution used by the Docker runtime.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResolvedSource {
    /// A requested image resolved to an immutable repository digest.
    Image {
        /// The image reference requested by the user.
        requested: ImageReference,
        /// The immutable image reference used at runtime.
        digest_reference: RepositoryDigest,
    },
    /// A Git revision built into an immutable local image.
    Git {
        /// Exact source and build inputs requested.
        requested: Source,
        /// Resolved full commit hash.
        commit: String,
        /// Docker's content-addressed local image ID.
        image_id: Sha256Digest,
    },
}

/// Invalid raw image-resolution result received at a runtime boundary.
#[derive(Debug, thiserror::Error)]
pub enum ResolvedImageError {
    /// Requested image syntax is invalid.
    #[error("invalid requested image reference")]
    Reference(#[from] crate::images::ImageReferenceError),
    /// The runtime did not return a repository digest.
    #[error("invalid resolved repository digest")]
    Digest(#[from] crate::images::RepositoryDigestError),
}

impl ResolvedSource {
    /// Checks raw image-resolution input and the runtime's repository digest.
    ///
    /// # Errors
    /// Returns the invalid reference or digest cause.
    pub fn parse_image(
        requested: impl Into<String>,
        digest: impl Into<String>,
    ) -> Result<Self, ResolvedImageError> {
        Ok(Self::Image {
            requested: ImageReference::parse(requested)?,
            digest_reference: RepositoryDigest::parse(digest)?,
        })
    }

    /// Returns the checked immutable image used by Docker.
    #[must_use]
    pub fn image(&self) -> ImmutableImage {
        match self {
            Self::Image {
                digest_reference, ..
            } => digest_reference.clone().into(),
            Self::Git { image_id, .. } => image_id.clone().into(),
        }
    }

    /// Returns the immutable image reference used by Docker.
    #[must_use]
    pub fn digest_reference(&self) -> &str {
        match self {
            Self::Image {
                digest_reference, ..
            } => digest_reference.as_str(),
            Self::Git { image_id, .. } => image_id.as_str(),
        }
    }
}

/// A Docker secret name and its file destination, with no secret value.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Ord, PartialOrd, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SecretFile {
    /// Immutable version name in Docker.
    pub secret_name: String,
    /// Container file destination.
    pub target: String,
}

/// Immutable resolutions supplied to application compilation.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ResolutionSet {
    /// Resolved service sources keyed by logical service name.
    pub sources: BTreeMap<ServiceName, ResolvedSource>,
    /// Logical secrets pinned to immutable Docker names.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub secret_names: BTreeMap<String, String>,
}

/// Resolution work still required before compilation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResolutionRequirement {
    /// Resolve an image source to an immutable digest.
    ResolveImage {
        /// Logical service requesting resolution.
        service: ServiceName,
        /// Requested image reference.
        reference: String,
    },
    /// Resolve a Git revision and build its image.
    BuildGit {
        /// Logical service requesting a build.
        service: ServiceName,
        /// Explicit Git source configuration.
        source: Source,
    },
}

/// Returns the image resolutions still needed before compilation.
#[must_use]
pub fn preview_resolution(
    app: &NormalizedApplication,
    resolutions: &ResolutionSet,
) -> Vec<ResolutionRequirement> {
    app.spec()
        .services
        .iter()
        .filter_map(|service| {
            if resolutions.sources.contains_key(&service.name) {
                None
            } else {
                Some(match &service.source {
                    Source::Image { image } => ResolutionRequirement::ResolveImage {
                        service: service.name.clone(),
                        reference: image.clone(),
                    },
                    Source::Git { .. } => ResolutionRequirement::BuildGit {
                        service: service.name.clone(),
                        source: service.source.clone(),
                    },
                })
            }
        })
        .collect()
}

/// Ownership metadata used to label runtime resources.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Ownership {
    /// The control-plane instance that owns the resource.
    pub instance_id: InstanceId,
    /// The application that owns the resource.
    pub application_id: ApplicationId,
    /// Logical service name when the resource belongs to one service.
    pub service: Option<ServiceName>,
    /// Normalized application spec hash.
    pub spec_hash: String,
}

impl Ownership {
    /// Produces the labels used to identify an owned Docker resource.
    #[must_use]
    pub fn labels(&self) -> BTreeMap<String, String> {
        let mut labels = BTreeMap::from([
            (MANAGED_LABEL.into(), "true".into()),
            (INSTANCE_LABEL.into(), self.instance_id.to_string()),
            (APPLICATION_LABEL.into(), self.application_id.to_string()),
            (SPEC_HASH_LABEL.into(), self.spec_hash.clone()),
        ]);
        if let Some(service) = &self.service {
            labels.insert(SERVICE_LABEL.into(), service.to_string());
        }
        labels
    }
}

/// Desired private overlay network state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DesiredNetwork {
    /// Canonical Docker resource name.
    pub name: DockerNetworkName,
    /// Expected ownership labels.
    pub labels: BTreeMap<String, String>,
}

impl DesiredNetwork {
    /// Returns whether the network has a canonical name and identity.
    #[must_use]
    pub fn has_valid_identity(&self) -> bool {
        let Some((application, _)) = desired_application_from_labels(&self.labels) else {
            return false;
        };
        !self.labels.contains_key(SERVICE_LABEL)
            && self.name == DockerNetworkName::for_application(&application)
    }
}

/// Desired persistent volume state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DesiredVolume {
    /// Manifest-level volume name.
    pub logical_name: VolumeName,
    /// Canonical Docker resource name.
    pub name: DockerVolumeName,
    /// Expected ownership labels.
    pub labels: BTreeMap<String, String>,
}

impl DesiredVolume {
    /// Returns whether the volume has a canonical name and identity.
    #[must_use]
    pub fn has_valid_identity(&self) -> bool {
        let Some((application, _)) = desired_application_from_labels(&self.labels) else {
            return false;
        };
        valid_logical_name(self.logical_name.as_str())
            && !self.labels.contains_key(SERVICE_LABEL)
            && self.name == DockerVolumeName::for_volume(&application, &self.logical_name)
    }
}

/// Desired persistent volume mount in a service.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DesiredMount {
    /// Canonical Docker volume name.
    pub volume_name: DockerVolumeName,
    /// Container target path.
    pub target: String,
    /// Whether the mount is read-only.
    pub read_only: bool,
}

/// Untrusted mount observation from Docker, including foreign volume names.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ObservedMount {
    /// Docker-reported volume name.
    pub volume_name: String,
    /// Container target path.
    pub target: String,
    /// Whether the mount is read-only.
    pub read_only: bool,
}

impl From<&DesiredMount> for ObservedMount {
    fn from(value: &DesiredMount) -> Self {
        Self {
            volume_name: value.volume_name.to_string(),
            target: value.target.clone(),
            read_only: value.read_only,
        }
    }
}

/// Desired Docker service state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DesiredService {
    /// Manifest-level service name.
    pub logical_name: ServiceName,
    /// Canonical Docker service name.
    pub name: DockerServiceName,
    /// Immutable source resolution used by the service.
    pub source: ResolvedSource,
    /// Digest-pinned image reference.
    pub image: ImmutableImage,
    /// Desired replica count.
    pub replicas: u16,
    /// Environment variables keyed by name.
    pub environment: BTreeMap<String, String>,
    /// Container entrypoint command.
    pub command: Vec<String>,
    /// Arguments passed to the command.
    pub arguments: Vec<String>,
    /// Persistent volume mounts.
    pub mounts: Vec<DesiredMount>,
    /// Immutable secret file bindings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<SecretFile>,
    /// Optional health check.
    pub healthcheck: Option<HealthCheck>,
    /// Optional CPU and memory limits.
    pub resources: Option<ResourceLimits>,
    /// Canonical private network names attached to the service.
    pub networks: Vec<DockerNetworkName>,
    /// Ownership labels.
    pub labels: BTreeMap<String, String>,
}

impl DesiredService {
    /// Returns whether the service has a canonical name and identity.
    #[must_use]
    pub fn has_valid_identity(&self) -> bool {
        let Some((application, _)) = desired_application_from_labels(&self.labels) else {
            return false;
        };
        valid_logical_name(self.logical_name.as_str())
            && self.labels.get(SERVICE_LABEL).map(String::as_str)
                == Some(self.logical_name.as_str())
            && self.name == DockerServiceName::for_service(&application, &self.logical_name)
    }
}

/// Desired state for an application and its resources.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ResolvedApplication {
    /// Immutable secret versions selected during effective input preparation.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub secret_names: BTreeMap<String, String>,
    /// Stable application identity.
    pub id: ApplicationId,
    /// User-facing application name.
    pub name: ApplicationName,
    /// Current control-plane instance identity.
    pub instance_id: InstanceId,
    /// Normalized application spec hash.
    pub spec_hash: String,
    /// Desired private network.
    pub networks: Vec<DesiredNetwork>,
    /// Desired persistent volumes.
    pub volumes: Vec<DesiredVolume>,
    /// Desired services.
    pub services: Vec<DesiredService>,
}

impl ResolvedApplication {
    /// Reuses immutable sources for services whose requested image is unchanged.
    #[must_use]
    pub fn reusable_resolutions(&self, application: &NormalizedApplication) -> ResolutionSet {
        ResolutionSet {
            secret_names: self.secret_names.clone(),
            sources: application
                .spec()
                .services
                .iter()
                .filter_map(|service| {
                    let prior = self
                        .services
                        .iter()
                        .find(|prior| prior.logical_name == service.name)?;
                    resolved_source_matches(&service.source, &prior.source)
                        .then(|| (service.name.clone(), prior.source.clone()))
                })
                .collect(),
        }
    }
}

fn desired_application_from_labels(
    labels: &BTreeMap<String, String>,
) -> Option<(ApplicationId, InstanceId)> {
    if labels.get(MANAGED_LABEL).map(String::as_str) != Some("true")
        || labels.get(INSTANCE_LABEL).is_none_or(String::is_empty)
        || labels
            .get(SPEC_HASH_LABEL)
            .is_none_or(|hash| !valid_sha256(hash))
    {
        return None;
    }
    Some((
        ApplicationId::parse(labels.get(APPLICATION_LABEL)?.clone()).ok()?,
        InstanceId::parse(labels.get(INSTANCE_LABEL)?.clone()).ok()?,
    ))
}

/// Returns whether a logical resource name is safe for Docker naming.
#[must_use]
pub fn valid_logical_name(value: &str) -> bool {
    (1..=63).contains(&value.len())
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !value.ends_with('-')
}

/// Sanitized compilation error for unresolved runtime inputs.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompileError {
    /// Stable machine-readable error code.
    pub code: String,
    /// Resource associated with the error.
    pub resource: String,
    /// Safe human-readable explanation.
    pub message: String,
}

/// Compiles normalized intent after all image references have immutable resolutions.
///
/// # Errors
///
/// Returns bounded compilation diagnostics when a service has no matching image
/// resolution or its resolved image is not an immutable reference to the
/// requested repository.
///
/// # Panics
///
/// Panics only if the domain hasher produced a malformed spec hash or a
/// validated resolution is missing, both of which indicate internal bugs.
pub fn compile_application(
    app: &NormalizedApplication,
    instance_id: InstanceId,
    resolutions: &ResolutionSet,
) -> Result<ResolvedApplication, Vec<CompileError>> {
    let errors = validate_application(app, resolutions);
    if !errors.is_empty() {
        return Err(errors);
    }

    let spec_hash = app.spec_hash();
    let digest =
        Sha256Digest::parse(spec_hash.clone()).expect("spec_hash is produced by the domain hasher");
    let ownership = Ownership {
        instance_id: instance_id.clone(),
        application_id: app.id().clone(),
        service: None,
        spec_hash: digest.as_str().to_owned(),
    };
    let private_network = DockerNetworkName::for_application(app.id());
    Ok(ResolvedApplication {
        secret_names: resolutions.secret_names.clone(),
        id: app.id().clone(),
        name: app.metadata().name.clone(),
        instance_id,
        spec_hash,
        networks: if app.spec().services.is_empty() {
            Vec::new()
        } else {
            vec![DesiredNetwork {
                name: private_network.clone(),
                labels: ownership.labels(),
            }]
        },
        volumes: app
            .spec()
            .volumes
            .iter()
            .map(|volume| DesiredVolume {
                logical_name: volume.name.clone(),
                name: DockerVolumeName::for_volume(app.id(), &volume.name),
                labels: ownership.labels(),
            })
            .collect(),
        services: app
            .spec()
            .services
            .iter()
            .map(|service| compile_service(service, app, resolutions, &ownership, &private_network))
            .collect(),
    })
}

fn validate_application(
    app: &NormalizedApplication,
    resolutions: &ResolutionSet,
) -> Vec<CompileError> {
    let mut errors = unresolved_errors(app, resolutions);
    for service in &app.spec().services {
        for secret in &service.secrets {
            if !resolutions.secret_names.contains_key(&secret.name) {
                errors.push(CompileError {
                    code: "secret_unresolved".into(),
                    resource: service.name.to_string(),
                    message: format!("secret {} has not been pinned", secret.name),
                });
            }
        }
    }
    for service in &app.spec().services {
        let Some(resolved) = resolutions.sources.get(&service.name) else {
            continue;
        };
        if !resolved_source_matches(&service.source, resolved) {
            errors.push(CompileError {
                code: crate::codes::SOURCE_RESOLUTION_MISMATCH.into(),
                resource: service.name.to_string(),
                message: "resolved source does not immutably resolve the normalized service source"
                    .into(),
            });
        }
    }
    errors
}

fn unresolved_errors(
    app: &NormalizedApplication,
    resolutions: &ResolutionSet,
) -> Vec<CompileError> {
    preview_resolution(app, resolutions)
        .into_iter()
        .map(|requirement| match requirement {
            ResolutionRequirement::ResolveImage { service, .. }
            | ResolutionRequirement::BuildGit { service, .. } => CompileError {
                code: crate::codes::SOURCE_UNRESOLVED.into(),
                resource: service.to_string(),
                message: "service source has not been resolved to an immutable image".into(),
            },
        })
        .collect()
}

fn resolved_source_matches(source: &Source, resolved: &ResolvedSource) -> bool {
    match (source, resolved) {
        (
            Source::Image { image },
            ResolvedSource::Image {
                requested,
                digest_reference,
            },
        ) => image == requested.as_str() && same_image_repository(image, digest_reference.as_str()),
        (
            Source::Git { repository, .. },
            ResolvedSource::Git {
                requested, commit, ..
            },
        ) => {
            source == requested
                && crate::manifest::valid_git_commit(commit)
                && repository
                    .commit
                    .as_ref()
                    .is_none_or(|pinned| pinned == commit)
        }
        _ => false,
    }
}

fn compile_service(
    service: &Service,
    app: &NormalizedApplication,
    resolutions: &ResolutionSet,
    application_ownership: &Ownership,
    private_network: &DockerNetworkName,
) -> DesiredService {
    let source = resolutions.sources[&service.name].clone();
    let mut ownership = application_ownership.clone();
    ownership.service = Some(service.name.clone());
    DesiredService {
        logical_name: service.name.clone(),
        name: DockerServiceName::for_service(app.id(), &service.name),
        image: source.image(),
        source,
        replicas: service.replicas,
        environment: service.environment.clone(),
        command: service.command.clone(),
        arguments: service.arguments.clone(),
        mounts: service
            .mounts
            .iter()
            .map(|mount: &Mount| DesiredMount {
                volume_name: DockerVolumeName::for_volume(app.id(), &mount.volume),
                target: mount.target.clone(),
                read_only: mount.read_only,
            })
            .collect(),
        secrets: service
            .secrets
            .iter()
            .map(|secret| SecretFile {
                secret_name: resolutions.secret_names[&secret.name].clone(),
                target: secret.target.clone(),
            })
            .collect(),
        healthcheck: service.healthcheck.clone(),
        resources: service.resources.clone(),
        networks: vec![private_network.clone()],
        labels: ownership.labels(),
    }
}

pub(crate) fn immutable_digest_reference(reference: &str) -> bool {
    valid_image_reference(reference)
        && reference
            .split_once("@sha256:")
            .is_some_and(|(name, digest)| {
                !name.contains('@')
                    && digest.len() == 64
                    && digest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
}

fn same_image_repository(requested: &str, resolved: &str) -> bool {
    image_repository(requested)
        .zip(image_repository(resolved))
        .is_some_and(|(left, right)| left == right)
}

/// Returns the canonical repository portion of a valid image reference.
#[must_use]
pub fn image_repository(reference: &str) -> Option<String> {
    if !valid_image_reference(reference) {
        return None;
    }
    let without_digest = reference
        .split_once('@')
        .map_or(reference, |(name, _)| name);
    let last_slash = without_digest.rfind('/');
    let repository = match without_digest.rfind(':') {
        Some(colon) if last_slash.is_none_or(|slash| colon > slash) => &without_digest[..colon],
        _ => without_digest,
    };
    if repository.is_empty() {
        return None;
    }
    let mut components = repository.split('/');
    let first = components.next()?;
    let explicit_registry = repository.contains('/')
        && (first.contains(['.', ':']) || first.eq_ignore_ascii_case("localhost"));
    if repository.contains('/')
        && (first.eq_ignore_ascii_case("docker.io")
            || first.eq_ignore_ascii_case("index.docker.io"))
    {
        let path = &repository[first.len() + 1..];
        Some(if path.contains('/') {
            format!("docker.io/{path}")
        } else {
            format!("docker.io/library/{path}")
        })
    } else if explicit_registry {
        Some(format!(
            "{}{}",
            first.to_ascii_lowercase(),
            &repository[first.len()..]
        ))
    } else if repository.contains('/') {
        Some(format!("docker.io/{repository}"))
    } else {
        Some(format!("docker.io/library/{repository}"))
    }
}

/// Lifecycle state of an observed Docker task.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    /// The task has been created but not scheduled.
    New,
    /// The task is waiting for scheduling.
    Pending,
    /// The task has been assigned to a node.
    Assigned,
    /// The node accepted the task.
    Accepted,
    /// The task is preparing its container.
    Preparing,
    /// The container is starting.
    Starting,
    /// The container is running.
    Running,
    /// The task completed successfully.
    Complete,
    /// The task failed.
    Failed,
    /// The task was rejected before starting.
    Rejected,
    /// The task was shut down.
    Shutdown,
    /// Docker did not provide a recognized state.
    #[default]
    #[serde(other)]
    Unknown,
}

/// Sanitized observation of one Docker task.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedTask {
    /// Current Docker task state.
    pub state: TaskState,
    /// Backend health result, when available.
    pub healthy: Option<bool>,
    /// Whether the task is still desired by the service.
    pub desired_running: bool,
    /// Sanitized task failure information.
    pub diagnostic: Option<TaskDiagnostic>,
}

/// Sanitized diagnostic for a failed task.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TaskDiagnostic {
    /// The container exited with an optional exit code.
    Failed {
        /// Exit code reported by Docker, when available.
        exit_code: Option<i64>,
    },
    /// Docker rejected the task before it could run.
    Rejected,
}

/// Aggregate health state derived from observed tasks.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum Convergence {
    /// All desired tasks are healthy and running.
    Converged,
    /// Docker is still applying an update.
    Updating,
    /// Some desired tasks are healthy but others are not.
    Degraded,
    /// No desired task is healthy or the update is paused.
    Failed,
}

/// Observed Docker network state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedNetwork {
    /// Observed Docker network name.
    pub name: String,
    /// Whether adapter-owned network settings remain canonical.
    pub runtime_configuration_matches: bool,
    /// Ownership labels observed on the network.
    pub labels: BTreeMap<String, String>,
}

impl ObservedNetwork {
    /// Returns whether ownership labels identify the desired network.
    #[must_use]
    pub fn matches_ownership(
        &self,
        desired: &DesiredNetwork,
        application: &ResolvedApplication,
    ) -> bool {
        OwnershipState::from_labels(&self.labels, &application.instance_id, &application.id)
            == OwnershipState::Owned
            && self.name == desired.name.as_str()
    }
}

/// Observed Docker volume state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedVolume {
    /// Observed Docker volume name.
    pub name: String,
    /// Whether the backend volume uses piqueld's supported local driver.
    pub runtime_configuration_matches: bool,
    /// Ownership labels observed on the volume.
    pub labels: BTreeMap<String, String>,
}

/// Observed Docker service state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedService {
    /// Observed Docker service name.
    pub name: String,
    /// Observed digest-pinned image.
    pub image: String,
    /// Observed replica count.
    pub replicas: u16,
    /// Environment variables observed in the container spec.
    pub environment: BTreeMap<String, String>,
    /// Observed container command.
    pub command: Vec<String>,
    /// Observed command arguments.
    pub arguments: Vec<String>,
    /// Persistent mounts observed on the service.
    pub mounts: Vec<ObservedMount>,
    /// Immutable secret file bindings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<SecretFile>,
    /// Observed health check.
    pub healthcheck: Option<HealthCheck>,
    /// Whether Docker has a health check, including an unsupported one.
    pub healthcheck_configured: bool,
    /// Observed resource limits.
    pub resources: Option<ResourceLimits>,
    /// Networks attached to the service.
    pub networks: Vec<String>,
    /// Ownership labels observed on the service.
    pub labels: BTreeMap<String, String>,
    /// Whether adapter-owned settings remain canonical.
    pub runtime_configuration_matches: bool,
    /// Task observations used to derive convergence.
    pub tasks: Vec<ObservedTask>,
    /// Aggregate convergence state.
    pub convergence: Convergence,
}

impl ObservedService {
    pub(crate) fn mounts_match(&self, desired: &DesiredService) -> bool {
        unordered_eq(
            self.mounts.iter().map(|mount| {
                (
                    mount.volume_name.as_str(),
                    mount.target.as_str(),
                    mount.read_only,
                )
            }),
            desired.mounts.iter().map(|mount| {
                (
                    mount.volume_name.as_str(),
                    mount.target.as_str(),
                    mount.read_only,
                )
            }),
        )
    }
    pub(crate) fn networks_match(&self, desired: &DesiredService) -> bool {
        unordered_eq(
            self.networks.iter().map(String::as_str),
            desired.networks.iter().map(DockerNetworkName::as_str),
        )
    }

    /// Returns whether all desired service fields match.
    #[must_use]
    pub fn matches(&self, desired: &DesiredService) -> bool {
        self.image == desired.image.as_str()
            && self.replicas == desired.replicas
            && self.environment == desired.environment
            && self.command == desired.command
            && self.arguments == desired.arguments
            && self.mounts_match(desired)
            && unordered_eq(&self.secrets, &desired.secrets)
            && self.healthcheck == desired.healthcheck
            && self.healthcheck_configured == desired.healthcheck.is_some()
            && self.resources == desired.resources
            && self.networks_match(desired)
            && owned_label_subset(&self.labels, &desired.labels)
            && self.runtime_configuration_matches
    }

    /// Returns whether ownership labels identify the desired service.
    #[must_use]
    pub fn matches_ownership(
        &self,
        desired: &DesiredService,
        application: &ResolvedApplication,
    ) -> bool {
        OwnershipState::from_labels(&self.labels, &application.instance_id, &application.id)
            == OwnershipState::Owned
            && self.labels.get(SERVICE_LABEL).map(String::as_str)
                == Some(desired.logical_name.as_str())
            && self.name == desired.name.as_str()
    }

    /// Returns whether labels and the canonical name identify this service.
    #[must_use]
    pub fn is_owned_by(&self, instance: &InstanceId, application: &ApplicationId) -> bool {
        if OwnershipState::from_labels(&self.labels, instance, application) != OwnershipState::Owned
        {
            return false;
        }
        let Some(logical_name) = self
            .labels
            .get(SERVICE_LABEL)
            .filter(|name| !name.is_empty())
        else {
            return false;
        };
        self.name == docker_resource_name(application, ResourceKind::Service, Some(logical_name))
    }
}

/// Observed resources associated with an application.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedApplication {
    /// Networks observed for the application.
    pub networks: Vec<ObservedNetwork>,
    /// Volumes observed for the application.
    pub volumes: Vec<ObservedVolume>,
    /// Services observed for the application.
    pub services: Vec<ObservedService>,
}

/// Result of comparing runtime ownership labels with an expected owner.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OwnershipState {
    /// Labels identify the expected instance and application.
    Owned,
    /// Labels identify a different instance or application.
    Foreign,
    /// Required ownership labels are missing or malformed.
    Invalid,
}

impl OwnershipState {
    /// Classifies ownership labels without exposing raw backend data.
    #[must_use]
    pub fn from_labels(
        labels: &BTreeMap<String, String>,
        instance: &InstanceId,
        application: &ApplicationId,
    ) -> Self {
        if labels.get(MANAGED_LABEL).map(String::as_str) != Some("true")
            || labels
                .get(SPEC_HASH_LABEL)
                .is_none_or(|hash| !valid_sha256(hash))
        {
            return Self::Invalid;
        }
        if labels.get(INSTANCE_LABEL).map(String::as_str) != Some(instance.as_str())
            || labels.get(APPLICATION_LABEL).map(String::as_str) != Some(application.as_str())
        {
            return Self::Foreign;
        }
        Self::Owned
    }
}

pub(crate) fn unordered_eq<T: Ord>(
    observed: impl IntoIterator<Item = T>,
    desired: impl IntoIterator<Item = T>,
) -> bool {
    let mut observed: Vec<_> = observed.into_iter().collect();
    let mut desired: Vec<_> = desired.into_iter().collect();
    observed.sort();
    desired.sort();
    observed == desired
}

pub(crate) fn owned_label_subset(
    observed: &BTreeMap<String, String>,
    desired: &BTreeMap<String, String>,
) -> bool {
    desired
        .iter()
        .all(|(key, value)| observed.get(key) == Some(value))
        && observed
            .iter()
            .filter(|(key, _)| key.starts_with("io.piqueld."))
            .all(|(key, value)| desired.get(key) == Some(value))
}

fn valid_sha256(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}

#[cfg(test)]
mod tests {
    use super::{InstanceId, InstanceIdError, Sha256Digest, Sha256DigestError};

    #[test]
    fn identity_types_validate_at_boundaries() {
        assert_eq!(InstanceId::parse("UPPERCASE").unwrap_err(), InstanceIdError);
        assert!(serde_json::from_str::<InstanceId>(r#""instance-1""#).is_ok());
        assert!(serde_json::from_str::<InstanceId>(r#""-invalid""#).is_err());
        let digest = format!("sha256:{}", "a".repeat(64));
        assert_eq!(Sha256Digest::parse(&digest).unwrap().as_str(), digest);
        assert_eq!(
            Sha256Digest::parse("sha256:bad").unwrap_err(),
            Sha256DigestError
        );
    }
}
