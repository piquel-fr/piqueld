//! Backend-neutral desired, resolved, and observed Docker resource contracts.

use crate::{
    ApplicationId, ResourceKind, docker_resource_name,
    manifest::{
        HealthCheck, Mount, NormalizedApplication, ResourceLimits, Service, Source,
        valid_image_reference,
    },
};
use serde::{Deserialize, Deserializer, Serialize, de};
use std::{collections::BTreeMap, fmt, str::FromStr};
use thiserror::Error;
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

/// Error returned when an instance identifier violates its storage invariant.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("instance IDs must be 1-64 lowercase ASCII letters, digits, or internal hyphens")]
pub struct InstanceIdError;

/// Stable control-plane instance identity.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
#[serde(transparent)]
pub struct InstanceId(String);

impl InstanceId {
    /// Parses a lowercase control-plane instance identifier.
    ///
    /// Identifiers must contain 1–64 ASCII letters, digits, and internal hyphens,
    /// and must begin and end with a letter or digit.
    ///
    /// # Errors
    ///
    /// Returns [`InstanceIdError`] if the value is empty, malformed, too long, or
    /// begins or ends with a hyphen.
    ///
    /// # Examples
    ///
    /// ```
    /// let id = InstanceId::parse("control-plane-1").unwrap();
    /// assert_eq!(id.to_string(), "control-plane-1");
    /// ```
    pub fn parse(value: impl Into<String>) -> Result<Self, InstanceIdError> {
        let value = value.into();
        if (1..=64).contains(&value.len())
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
        {
            Ok(Self(value))
        } else {
            Err(InstanceIdError)
        }
    }

    /// Returns the canonical string representation used for persistence.
    ///
    /// # Examples
    ///
    /// ```
    /// let id: InstanceId = "worker-1".parse().unwrap();
    /// assert_eq!(id.as_str(), "worker-1");
    /// ```
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for InstanceId {
    /// Deserializes an instance identifier from a string and validates its format.
    ///
    /// # Examples
    ///
    /// ```
    /// let instance: InstanceId = serde_json::from_str("\"worker-1\"").unwrap();
    /// assert_eq!(instance.to_string(), "worker-1");
    /// ```
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(de::Error::custom)
    }
}

impl fmt::Display for InstanceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for InstanceId {
    type Err = InstanceIdError;
    /// Parses an instance identifier from its string representation.
    ///
    /// # Examples
    ///
    /// ```
    /// let id: InstanceId = "web-1".parse().unwrap();
    /// assert_eq!(id.to_string(), "web-1");
    /// ```
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

/// Error returned when a digest is malformed.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("SHA-256 digests must use the sha256:<64 lowercase hexadecimal digits> format")]
pub struct Sha256DigestError;

/// Explicitly tagged lowercase SHA-256 digest.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
#[serde(transparent)]
pub struct Sha256Digest(String);

impl Sha256Digest {
    /// Parses a lowercase, explicitly tagged SHA-256 digest.
    ///
    /// # Errors
    ///
    /// Returns [`Sha256DigestError`] if the value does not contain the `sha256:`
    /// prefix followed by exactly 64 lowercase hexadecimal digits.
    ///
    /// # Examples
    ///
    /// ```
    /// let digest = Sha256Digest::parse(
    ///     "sha256:0000000000000000000000000000000000000000000000000000000000000000",
    /// )?;
    /// # Ok::<(), Sha256DigestError>(())
    /// ```
    pub fn parse(value: impl Into<String>) -> Result<Self, Sha256DigestError> {
        let value = value.into();
        if valid_sha256(&value) {
            Ok(Self(value))
        } else {
            Err(Sha256DigestError)
        }
    }

    /// Returns the canonical string representation used for persistence.
    ///
    /// # Examples
    ///
    /// ```
    /// let id: InstanceId = "worker-1".parse().unwrap();
    /// assert_eq!(id.as_str(), "worker-1");
    /// ```
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for Sha256Digest {
    /// Deserializes an instance identifier from a string and validates its format.
    ///
    /// # Examples
    ///
    /// ```
    /// let instance: InstanceId = serde_json::from_str("\"worker-1\"").unwrap();
    /// assert_eq!(instance.to_string(), "worker-1");
    /// ```
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(de::Error::custom)
    }
}

impl fmt::Display for Sha256Digest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl FromStr for Sha256Digest {
    type Err = Sha256DigestError;
    /// Parses an instance identifier from its string representation.
    ///
    /// # Examples
    ///
    /// ```
    /// let id: InstanceId = "web-1".parse().unwrap();
    /// assert_eq!(id.to_string(), "web-1");
    /// ```
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

/// Immutable image resolution used by the Docker runtime.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResolvedSource {
    /// A requested image resolved to an immutable repository digest.
    Image {
        /// The image reference requested by the user.
        requested: String,
        /// The immutable image reference used at runtime.
        digest_reference: String,
    },
}

impl ResolvedSource {
    /// Provides the immutable image reference resolved for Docker.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn check(source: &ResolvedSource) {
    /// let reference = source.digest_reference();
    /// assert!(!reference.is_empty());
    /// # }
    /// ```
    pub fn digest_reference(&self) -> &str {
        match self {
            Self::Image {
                digest_reference, ..
            } => digest_reference,
        }
    }
}

/// Immutable resolutions supplied to application compilation.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ResolutionSet {
    /// Resolved service sources keyed by logical service name.
    pub sources: BTreeMap<String, ResolvedSource>,
}

/// Resolution work still required before compilation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResolutionRequirement {
    /// Resolve an image source to an immutable digest.
    ResolveImage {
        /// Logical service requesting resolution.
        service: String,
        /// Requested image reference.
        reference: String,
    },
}

/// Identifies image sources that do not yet have resolved immutable references.
///
/// # Examples
///
/// ```
/// # let app: NormalizedApplication = todo!();
/// let resolutions = ResolutionSet::default();
/// let requirements = preview_resolution(&app, &resolutions);
/// assert!(requirements.is_empty() || !requirements.is_empty());
/// ```
pub fn preview_resolution(
    app: &NormalizedApplication,
    resolutions: &ResolutionSet,
) -> Vec<ResolutionRequirement> {
    app.spec
        .services
        .iter()
        .filter_map(|service| {
            if resolutions.sources.contains_key(&service.name) {
                None
            } else {
                let Source::Image { image } = &service.source;
                Some(ResolutionRequirement::ResolveImage {
                    service: service.name.clone(),
                    reference: image.clone(),
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
    pub service: Option<String>,
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
            labels.insert(SERVICE_LABEL.into(), service.clone());
        }
        labels
    }
}

/// Desired private overlay network state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DesiredNetwork {
    /// Canonical Docker resource name.
    pub name: String,
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
            && self.name == docker_resource_name(&application, ResourceKind::Network, None)
    }
}

/// Desired persistent volume state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DesiredVolume {
    /// Manifest-level volume name.
    pub logical_name: String,
    /// Canonical Docker resource name.
    pub name: String,
    /// Expected ownership labels.
    pub labels: BTreeMap<String, String>,
}

impl DesiredVolume {
    /// Determines whether the volume has a valid logical identity and canonical Docker name.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// assert!(volume.has_valid_identity());
    /// ```
    ///
    /// # Returns
    ///
    /// `true` if the volume has valid ownership labels, a valid logical name, no
    /// service label, and its name matches the canonical volume name; `false`
    /// otherwise.
    ///
    /// # Panics
    ///
    /// This function does not panic.
    pub fn has_valid_identity(&self) -> bool {
        let Some((application, _)) = desired_application_from_labels(&self.labels) else {
            return false;
        };
        valid_logical_name(&self.logical_name)
            && !self.labels.contains_key(SERVICE_LABEL)
            && self.name
                == docker_resource_name(
                    &application,
                    ResourceKind::Volume,
                    Some(&self.logical_name),
                )
    }
}

/// Desired persistent volume mount in a service.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DesiredMount {
    /// Canonical Docker volume name.
    pub volume_name: String,
    /// Container target path.
    pub target: String,
    /// Whether the mount is read-only.
    pub read_only: bool,
}

/// Desired Docker service state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DesiredService {
    /// Manifest-level service name.
    pub logical_name: String,
    /// Canonical Docker service name.
    pub name: String,
    /// Immutable source resolution used by the service.
    pub source: ResolvedSource,
    /// Digest-pinned image reference.
    pub image: String,
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
    /// Optional health check.
    pub healthcheck: Option<HealthCheck>,
    /// Optional CPU and memory limits.
    pub resources: Option<ResourceLimits>,
    /// Canonical private network names attached to the service.
    pub networks: Vec<String>,
    /// Ownership labels.
    pub labels: BTreeMap<String, String>,
}

impl DesiredService {
    /// Determines whether the service has valid logical identity and canonical ownership metadata.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn check(service: &DesiredService) {
    /// assert!(service.has_valid_identity());
    /// # }
    /// ```
    ///
    /// # Returns
    ///
    /// `true` if the service has a valid logical name, matching service label, and canonical resource name; `false` otherwise.
    pub fn has_valid_identity(&self) -> bool {
        let Some((application, _)) = desired_application_from_labels(&self.labels) else {
            return false;
        };
        valid_logical_name(&self.logical_name)
            && self.labels.get(SERVICE_LABEL).map(String::as_str)
                == Some(self.logical_name.as_str())
            && self.name
                == docker_resource_name(
                    &application,
                    ResourceKind::Service,
                    Some(&self.logical_name),
                )
    }
}

/// Desired state for an application and its resources.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DesiredApplication {
    /// Stable application identity.
    pub id: ApplicationId,
    /// User-facing application name.
    pub name: String,
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

/// Resolved application state used by the runtime reconciler.
pub type ResolvedApplication = DesiredApplication;

/// Extracts an application and instance identity from valid managed-resource labels.
///
/// # Examples
///
/// ```
/// use std::collections::BTreeMap;
///
/// let labels = BTreeMap::from([
///     (MANAGED_LABEL.to_owned(), "true".to_owned()),
///     (APPLICATION_LABEL.to_owned(), "my-app".to_owned()),
///     (INSTANCE_LABEL.to_owned(), "dev".to_owned()),
///     (SPEC_HASH_LABEL.to_owned(), format!("sha256:{}", "a".repeat(64))),
/// ]);
///
/// assert!(desired_application_from_labels(&labels).is_some());
/// ```
///
/// # Returns
///
/// `Some` containing the application and instance identifiers when all required
/// labels are valid, or `None` otherwise.
fn desired_application_from_labels(
    labels: &BTreeMap<String, String>,
) -> Option<(ApplicationId, InstanceId)> {
    if labels.get(MANAGED_LABEL).map(String::as_str) != Some("true")
        || labels.get(INSTANCE_LABEL).is_none_or(String::is_empty)
        || labels
            .get(SPEC_HASH_LABEL)
            .is_none_or(|hash| Sha256Digest::parse(hash.clone()).is_err())
    {
        return None;
    }
    Some((
        ApplicationId::parse(labels.get(APPLICATION_LABEL)?.clone()).ok()?,
        InstanceId::parse(labels.get(INSTANCE_LABEL)?.clone()).ok()?,
    ))
}

/// Validates a logical resource name for Docker naming requirements.
///
/// Names must contain 1–63 lowercase letters, digits, or hyphens, start with a
/// lowercase letter, and end with a letter or digit.
///
/// # Examples
///
/// ```
/// assert!(valid_logical_name("web-1"));
/// assert!(!valid_logical_name("Web-1"));
/// assert!(!valid_logical_name("web-"));
/// ```
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

/// Compiles normalized application intent into desired Docker resources using immutable image resolutions.
///
/// # Errors
///
/// Returns bounded compilation diagnostics when an image resolution is missing or does not
/// reference the requested repository with an immutable digest.
///
/// # Examples
///
/// ```
/// # let app: NormalizedApplication = todo!();
/// # let instance_id: InstanceId = todo!();
/// # let resolutions: ResolutionSet = todo!();
/// let desired = compile_application(&app, instance_id, &resolutions)?;
/// # Ok::<(), Vec<CompileError>>(())
/// ```
pub fn compile_application(
    app: &NormalizedApplication,
    instance_id: InstanceId,
    resolutions: &ResolutionSet,
) -> Result<DesiredApplication, Vec<CompileError>> {
    let errors = validate_application(app, resolutions);
    if !errors.is_empty() {
        return Err(errors);
    }

    let spec_hash = app.spec_hash();
    let ownership = Ownership {
        instance_id: instance_id.clone(),
        application_id: app.id.clone(),
        service: None,
        spec_hash: spec_hash.clone(),
    };
    let private_network = docker_resource_name(&app.id, ResourceKind::Network, None);
    Ok(DesiredApplication {
        id: app.id.clone(),
        name: app.metadata.name.clone(),
        instance_id,
        spec_hash,
        networks: vec![DesiredNetwork {
            name: private_network.clone(),
            labels: ownership.labels(),
        }],
        volumes: app
            .spec
            .volumes
            .iter()
            .map(|volume| DesiredVolume {
                logical_name: volume.name.clone(),
                name: docker_resource_name(&app.id, ResourceKind::Volume, Some(&volume.name)),
                labels: ownership.labels(),
            })
            .collect(),
        services: app
            .spec
            .services
            .iter()
            .map(|service| compile_service(service, app, resolutions, &ownership, &private_network))
            .collect(),
    })
}

/// Validates that every normalized service has a matching immutable image resolution.
///
/// # Returns
///
/// A list of compilation errors for unresolved services or resolutions that do not match the
/// requested service sources.
///
/// # Examples
///
/// ```rust,ignore
/// let errors = validate_application(&app, &resolutions);
/// assert!(errors.is_empty());
/// ```
fn validate_application(
    app: &NormalizedApplication,
    resolutions: &ResolutionSet,
) -> Vec<CompileError> {
    let mut errors = unresolved_errors(app, resolutions);
    for service in &app.spec.services {
        let Some(resolved) = resolutions.sources.get(&service.name) else {
            continue;
        };
        if !resolved_source_matches(&service.source, resolved) {
            errors.push(CompileError {
                code: "source_resolution_mismatch".into(),
                resource: service.name.clone(),
                message: "resolved source does not immutably resolve the normalized service source"
                    .into(),
            });
        }
    }
    errors
}

/// Creates compilation diagnostics for services whose images lack immutable digest resolutions.
///
/// # Examples
///
/// ```ignore
/// let errors = unresolved_errors(&app, &resolutions);
/// assert!(errors.iter().all(|error| error.code == "source_unresolved"));
/// ```
///
/// # Returns
///
/// A compilation error for each service image that has not been resolved.
fn unresolved_errors(
    app: &NormalizedApplication,
    resolutions: &ResolutionSet,
) -> Vec<CompileError> {
    preview_resolution(app, resolutions)
        .into_iter()
        .map(|requirement| match requirement {
            ResolutionRequirement::ResolveImage { service, .. } => CompileError {
                code: "source_unresolved".into(),
                resource: service,
                message: "service image has not been resolved to an immutable digest".into(),
            },
        })
        .collect()
}

/// Determines whether a resolved image matches the requested image and uses an immutable digest from the same repository.
///
/// # Examples
///
/// ```
/// let source = Source::Image {
///     image: "nginx".into(),
/// };
/// let resolved = ResolvedSource::Image {
///     requested: "nginx".into(),
///     digest_reference: "nginx@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into(),
/// };
///
/// assert!(resolved_source_matches(&source, &resolved));
/// ```
fn resolved_source_matches(source: &Source, resolved: &ResolvedSource) -> bool {
    match (source, resolved) {
        (
            Source::Image { image },
            ResolvedSource::Image {
                requested,
                digest_reference,
            },
        ) => {
            image == requested
                && immutable_digest_reference(digest_reference)
                && same_image_repository(image, digest_reference)
        }
    }
}

/// Builds the desired Docker service from its normalized definition and resolved image.
///
/// # Examples
///
/// ```ignore
/// let desired = compile_service(
///     &service,
///     &application,
///     &resolutions,
///     &ownership,
///     "private-network",
/// );
/// assert_eq!(desired.logical_name, service.name);
/// ```
///
/// # Returns
///
/// A desired service with its resolved image, ownership labels, mounts, runtime settings, and private-network attachment.
fn compile_service(
    service: &Service,
    app: &NormalizedApplication,
    resolutions: &ResolutionSet,
    application_ownership: &Ownership,
    private_network: &str,
) -> DesiredService {
    let source = resolutions.sources[&service.name].clone();
    let mut ownership = application_ownership.clone();
    ownership.service = Some(service.name.clone());
    DesiredService {
        logical_name: service.name.clone(),
        name: docker_resource_name(&app.id, ResourceKind::Service, Some(&service.name)),
        image: source.digest_reference().into(),
        source,
        replicas: service.replicas,
        environment: service.environment.clone(),
        command: service.command.clone(),
        arguments: service.arguments.clone(),
        mounts: service
            .mounts
            .iter()
            .map(|mount: &Mount| DesiredMount {
                volume_name: docker_resource_name(
                    &app.id,
                    ResourceKind::Volume,
                    Some(&mount.volume),
                ),
                target: mount.target.clone(),
                read_only: mount.read_only,
            })
            .collect(),
        healthcheck: service.healthcheck.clone(),
        resources: service.resources.clone(),
        networks: vec![private_network.into()],
        labels: ownership.labels(),
    }
}

/// Determines whether an image reference uses a valid immutable SHA-256 digest.
///
/// # Examples
///
/// ```
/// let reference = "registry.example.com/app@sha256:\
/// 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
///
/// assert!(immutable_digest_reference(reference));
/// assert!(!immutable_digest_reference("registry.example.com/app:latest"));
/// ```
///
/// # Parameters
///
/// * `reference` - The image reference to validate.
///
/// # Returns
///
/// `true` if the reference contains a valid lowercase 64-character SHA-256 digest, `false` otherwise.
fn immutable_digest_reference(reference: &str) -> bool {
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

/// Determines whether two image references belong to the same repository.
///
/// # Examples
///
/// ```
/// assert!(same_image_repository(
///     "docker.io/library/alpine:latest",
///     "alpine@sha256:abc",
/// ));
/// assert!(!same_image_repository(
///     "alpine:latest",
///     "ubuntu@sha256:abc",
/// ));
/// ```
///
/// # Returns
///
/// `true` if both references have the same normalized repository, `false` otherwise.
fn same_image_repository(requested: &str, resolved: &str) -> bool {
    image_repository(requested)
        .zip(image_repository(resolved))
        .is_some_and(|(left, right)| left == right)
}

/// Normalizes a valid image reference to its canonical repository name.
///
/// Docker Hub references are expanded to include the `docker.io` registry and
/// `library` namespace when applicable. Tags and digests are omitted from the
/// result.
///
/// # Examples
///
/// ```
/// assert_eq!(
///     image_repository("ubuntu:latest"),
///     Some("docker.io/library/ubuntu".to_owned())
/// );
/// ```
///
/// # Returns
///
/// The canonical repository name, or `None` if the image reference is invalid.
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
    let repository = repository
        .strip_prefix("index.docker.io/")
        .unwrap_or(repository);
    let mut components = repository.split('/');
    let first = components.next()?;
    let explicit_registry =
        repository.contains('/') && (first.contains(['.', ':']) || first == "localhost");
    if let Some(path) = repository.strip_prefix("docker.io/") {
        Some(if path.contains('/') {
            repository.to_owned()
        } else {
            format!("docker.io/library/{path}")
        })
    } else if explicit_registry {
        Some(repository.to_owned())
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
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
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
    /// Determines whether the observed network belongs to the desired application and has its canonical name.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// assert!(observed.matches_ownership(&desired, &application));
    /// ```
    ///
    /// # Returns
    ///
    /// `true` if the ownership labels identify the application and the observed network has the desired name, `false` otherwise.
    pub fn matches_ownership(
        &self,
        desired: &DesiredNetwork,
        application: &DesiredApplication,
    ) -> bool {
        OwnershipState::from_labels(&self.labels, &application.instance_id, &application.id)
            == OwnershipState::Owned
            && self.name == desired.name
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
    pub mounts: Vec<DesiredMount>,
    /// Observed health check.
    pub healthcheck: Option<HealthCheck>,
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
    /// Determines whether the observed service conforms to the desired service configuration.
    ///
    /// Compares runtime settings, image, replicas, environment, command, arguments, health
    /// check, resources, mounts, networks, and managed labels.
    ///
    /// # Examples
    ///
    /// ```
    /// # let observed = /* an observed service */ todo!();
    /// # let desired = /* its desired configuration */ todo!();
    /// assert!(observed.matches(&desired));
    /// ```
    ///
    /// Returns `true` when all compared fields match, `false` otherwise.
    pub fn matches(&self, desired: &DesiredService) -> bool {
        self.image == desired.image
            && self.replicas == desired.replicas
            && self.environment == desired.environment
            && self.command == desired.command
            && self.arguments == desired.arguments
            && unordered_eq(&self.mounts, &desired.mounts)
            && self.healthcheck == desired.healthcheck
            && self.resources == desired.resources
            && sorted(&self.networks) == sorted(&desired.networks)
            && owned_label_subset(&self.labels, &desired.labels)
            && self.runtime_configuration_matches
    }

    /// Determines whether an observed service belongs to the desired service.
    ///
    /// The service must have valid application and instance ownership labels, the
    /// expected service label, and the canonical desired resource name.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// assert!(observed.matches_ownership(&desired_service, &desired_application));
    /// ```
    ///
    /// # Returns
    ///
    /// `true` if the observed service matches the desired service, `false` otherwise.
    ///
    /// #[must_use]
    pub fn matches_ownership(
        &self,
        desired: &DesiredService,
        application: &DesiredApplication,
    ) -> bool {
        OwnershipState::from_labels(&self.labels, &application.instance_id, &application.id)
            == OwnershipState::Owned
            && self.labels.get(SERVICE_LABEL).map(String::as_str)
                == Some(desired.logical_name.as_str())
            && self.name == desired.name
    }

    /// Determines whether the service belongs to the specified application and instance.
    ///
    /// A service is owned when its ownership labels are valid and its name matches the
    /// canonical Docker resource name derived from its application and logical service name.
    ///
    /// # Examples
    ///
    /// ```
    /// let owned = service.is_owned_by(&instance, &application);
    /// assert!(owned);
    /// ```
    ///
    /// Returns `true` if the service has matching ownership metadata and a canonical name,
    /// `false` otherwise.
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
    /// Classifies a resource's ownership labels as owned, foreign, or invalid.
    ///
    /// # Examples
    ///
    /// ```
    /// use std::collections::BTreeMap;
    ///
    /// let labels = BTreeMap::new();
    /// let instance = "dev".parse().unwrap();
    /// let application = "web".parse().unwrap();
    ///
    /// assert!(matches!(
    ///     OwnershipState::from_labels(&labels, &instance, &application),
    ///     OwnershipState::Invalid
    /// ));
    /// ```
    ///
    /// Returns `Owned` when labels identify the expected resource, `Foreign` when
    /// they identify another resource, and `Invalid` when required labels are
    /// missing or malformed.
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

/// Compares two slices for equality without considering element order.
///
/// Duplicate elements are significant.
///
/// # Examples
///
/// ```
/// assert!(unordered_eq(&[3, 1, 2], &[1, 2, 3]));
/// assert!(!unordered_eq(&[1, 1, 2], &[1, 2]));
/// ```
pub(crate) fn unordered_eq<T: Ord>(observed: &[T], desired: &[T]) -> bool
pub(crate) fn unordered_eq<T: Ord>(observed: &[T], desired: &[T]) -> bool {
    let mut observed = observed.iter().collect::<Vec<_>>();
    let mut desired = desired.iter().collect::<Vec<_>>();
    observed.sort_unstable();
    desired.sort_unstable();
    observed == desired
}

/// Returns the unique strings in lexicographic order.
///
/// # Examples
///
/// ```
/// let values = ["beta".to_string(), "alpha".to_string(), "beta".to_string()];
/// assert_eq!(sorted(&values), vec!["alpha", "beta"]);
/// ```
fn sorted(values: &[String]) -> Vec<&str> {
    let mut values = values.iter().map(String::as_str).collect::<Vec<_>>();
    values.sort_unstable();
    values.dedup();
    values
}

/// Checks whether observed labels contain all desired labels and match every managed label.
///
/// # Examples
///
/// ```
/// use std::collections::BTreeMap;
///
/// let mut observed = BTreeMap::from([
///     ("app".to_string(), "example".to_string()),
///     ("io.piqueld.instance".to_string(), "test".to_string()),
/// ]);
/// let desired = BTreeMap::from([("io.piqueld.instance".to_string(), "test".to_string())]);
///
/// assert!(owned_label_subset(&observed, &desired));
///
/// observed.insert("io.piqueld.instance".to_string(), "other".to_string());
/// assert!(!owned_label_subset(&observed, &desired));
/// ```
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

/// Validates a lowercase, explicitly tagged SHA-256 digest.
///
/// # Examples
///
/// ```
/// assert!(valid_sha256(
///     "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
/// ));
/// assert!(!valid_sha256("sha256:invalid"));
/// ```
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
