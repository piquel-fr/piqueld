//! Backend-neutral desired, resolved, and observed Docker resource contracts.

use crate::{
    ApplicationId, ResourceKind, docker_resource_name,
    manifest::{
        HealthCheck, Mount, NormalizedApplication, ResourceLimits, SecretReference, Service,
        Source, valid_image_reference,
    },
};
use serde::{Deserialize, Deserializer, Serialize, de};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    str::FromStr,
};
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
/// Label carrying the logical secret identity.
pub const SECRET_LABEL: &str = "io.piqueld.secret";
/// Shared overlay network used by the managed Traefik ingress service.
pub const INGRESS_NETWORK: &str = "piqueld-ingress";

/// Error returned when an instance identifier violates its storage invariant.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("instance IDs must be 1-64 lowercase ASCII letters, digits, or internal hyphens")]
pub struct InstanceIdError;

/// Stable control-plane instance identity.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
#[serde(transparent)]
pub struct InstanceId(String);

impl InstanceId {
    /// Parses a safe, stable control-plane instance identifier.
    ///
    /// # Errors
    ///
    /// Returns [`InstanceIdError`] when the value is empty, malformed, or
    /// outside the bounded identifier format.
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

    /// Returns the persisted representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for InstanceId {
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
    /// Parses an explicitly tagged lowercase SHA-256 digest.
    ///
    /// # Errors
    ///
    /// Returns [`Sha256DigestError`] when the value is not a lowercase
    /// `sha256:` digest with exactly 64 hexadecimal digits.
    pub fn parse(value: impl Into<String>) -> Result<Self, Sha256DigestError> {
        let value = value.into();
        if valid_sha256(&value) {
            Ok(Self(value))
        } else {
            Err(Sha256DigestError)
        }
    }

    /// Returns the persisted representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for Sha256Digest {
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
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

/// Immutable source resolution used by the Docker runtime.
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
    /// A Git source built and verified as an immutable registry image.
    Git {
        /// HTTPS repository URL selected by the manifest.
        repository: String,
        /// Git reference selected by the manifest.
        requested_reference: String,
        /// Commit checked out by the builder.
        commit: String,
        /// Relative build context selected by the manifest.
        context: String,
        /// Relative Dockerfile selected by the manifest.
        dockerfile: String,
        /// Mutable tag used to publish the build.
        registry_reference: String,
        /// Registry-verified immutable image reference.
        digest_reference: String,
    },
}

impl ResolvedSource {
    /// Returns the immutable image reference used by Docker.
    #[must_use]
    pub fn digest_reference(&self) -> &str {
        match self {
            Self::Image {
                digest_reference, ..
            }
            | Self::Git {
                digest_reference, ..
            } => digest_reference,
        }
    }
}

/// Immutable content generation for a logical secret.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SecretGeneration {
    /// Logical secret name.
    pub logical_name: String,
    /// Digest identifying the secret content.
    pub generation: Sha256Digest,
    /// Randomized immutable Swarm secret name.
    pub swarm_name: String,
}

/// Immutable resolutions supplied to application compilation.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ResolutionSet {
    /// Resolved service sources keyed by logical service name.
    pub sources: BTreeMap<String, ResolvedSource>,
    /// Resolved secret generations keyed by logical secret name.
    #[serde(default)]
    pub secrets: BTreeMap<String, SecretGeneration>,
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
    /// Resolve a Git source before building it.
    ResolveGit {
        /// Logical service requesting resolution.
        service: String,
        /// HTTPS repository URL.
        repository: String,
        /// Git reference.
        reference: String,
    },
    /// Build and push a resolved Git checkout.
    BuildAndPush {
        /// Logical service requesting a build.
        service: String,
    },
    /// Provide a generation for a referenced logical secret.
    ProvideSecretGeneration {
        /// Logical secret name.
        logical_name: String,
    },
}

/// Returns the image resolutions still needed before compilation.
#[must_use]
pub fn preview_resolution(
    app: &NormalizedApplication,
    resolutions: &ResolutionSet,
) -> Vec<ResolutionRequirement> {
    let mut requirements = Vec::new();
    for service in &app.spec.services {
        if !resolutions.sources.contains_key(&service.name) {
            match &service.source {
                Source::Image { image } => requirements.push(ResolutionRequirement::ResolveImage {
                    service: service.name.clone(),
                    reference: image.clone(),
                }),
                Source::Git {
                    repository,
                    reference,
                    ..
                } => {
                    requirements.push(ResolutionRequirement::ResolveGit {
                        service: service.name.clone(),
                        repository: repository.clone(),
                        reference: reference.clone(),
                    });
                    requirements.push(ResolutionRequirement::BuildAndPush {
                        service: service.name.clone(),
                    });
                }
            }
        }
        for secret in &service.secrets {
            if !resolutions.secrets.contains_key(&secret.source)
                && !requirements.iter().any(|requirement| {
                    matches!(requirement, ResolutionRequirement::ProvideSecretGeneration { logical_name } if logical_name == &secret.source)
                })
            {
                requirements.push(ResolutionRequirement::ProvideSecretGeneration {
                    logical_name: secret.source.clone(),
                });
            }
        }
    }
    requirements
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
    /// Whether this is the shared ingress network.
    #[serde(default)]
    pub ingress: bool,
    /// Expected ownership labels.
    pub labels: BTreeMap<String, String>,
}

impl DesiredNetwork {
    /// Returns whether the network has a canonical name and identity.
    #[must_use]
    pub fn has_valid_identity(&self) -> bool {
        if self.ingress {
            return self.name == INGRESS_NETWORK
                && self.labels.get(MANAGED_LABEL).map(String::as_str) == Some("true")
                && self
                    .labels
                    .get(INSTANCE_LABEL)
                    .is_some_and(|instance| InstanceId::parse(instance.clone()).is_ok())
                && !self.labels.contains_key(APPLICATION_LABEL)
                && !self.labels.contains_key(SERVICE_LABEL)
                && !self.labels.contains_key(SPEC_HASH_LABEL);
        }
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
    /// Returns whether the volume has a canonical name and identity.
    #[must_use]
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

/// Desired Swarm secret state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DesiredSecret {
    /// Logical secret name.
    pub logical_name: String,
    /// Desired secret content generation.
    pub generation: Sha256Digest,
    /// Randomized immutable Swarm secret name.
    pub name: String,
    /// Expected ownership labels.
    pub labels: BTreeMap<String, String>,
}

impl DesiredSecret {
    /// Returns whether the desired secret has a canonical runtime identity.
    #[must_use]
    pub fn has_valid_identity(&self) -> bool {
        let Some((application, _)) = desired_application_from_labels(&self.labels) else {
            return false;
        };
        valid_logical_name(&self.logical_name)
            && Self::is_valid_runtime_name(&application, &self.logical_name, &self.name)
            && self.labels.get(SECRET_LABEL).map(String::as_str) == Some(self.logical_name.as_str())
            && !self.labels.contains_key(SERVICE_LABEL)
    }

    /// Checks the bounded randomized Swarm name format for a logical secret.
    #[must_use]
    pub fn is_valid_runtime_name(
        application: &ApplicationId,
        logical_name: &str,
        value: &str,
    ) -> bool {
        valid_swarm_secret_name(logical_name, value, application)
    }
}

/// Desired secret mount in a service.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct DesiredSecretMount {
    /// Logical secret name.
    pub logical_name: String,
    /// Randomized Docker Swarm secret name.
    pub swarm_name: String,
    /// Container target path.
    pub target: String,
    /// File mode presented in the container.
    pub mode: String,
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
    /// Ports exposed to the ingress controller.
    pub ports: Vec<u16>,
    /// Persistent volume mounts.
    pub mounts: Vec<DesiredMount>,
    /// Swarm secret mounts.
    pub secrets: Vec<DesiredSecretMount>,
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
    /// Returns whether the service has a canonical name and identity.
    #[must_use]
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
    /// Desired Swarm secrets.
    pub secrets: Vec<DesiredSecret>,
    /// Desired services.
    pub services: Vec<DesiredService>,
}

/// Resolved application state used by the runtime reconciler.
pub type ResolvedApplication = DesiredApplication;

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
    let ingress_network = INGRESS_NETWORK;
    let referenced_secrets = referenced_secrets(app);
    Ok(DesiredApplication {
        id: app.id.clone(),
        name: app.metadata.name.clone(),
        instance_id,
        spec_hash,
        networks: compile_networks(app, ingress_network, &private_network, &ownership),
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
        secrets: compile_secrets(resolutions, &referenced_secrets, &ownership),
        services: app
            .spec
            .services
            .iter()
            .map(|service| {
                compile_service(
                    service,
                    app,
                    resolutions,
                    &ownership,
                    &private_network,
                    ingress_network,
                )
            })
            .collect(),
    })
}

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
    let referenced = referenced_secrets(app);
    for (logical_name, secret) in resolutions
        .secrets
        .iter()
        .filter(|(name, _)| referenced.contains(name.as_str()))
    {
        if logical_name != &secret.logical_name
            || !valid_swarm_secret_name(logical_name, &secret.swarm_name, &app.id)
        {
            errors.push(CompileError {
                code: "secret_generation_invalid".into(),
                resource: logical_name.clone(),
                message: "secret generation metadata is inconsistent or its Swarm name is invalid"
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
            ResolutionRequirement::ResolveImage { service, .. } => CompileError {
                code: "source_unresolved".into(),
                resource: service,
                message: "service image has not been resolved to an immutable digest".into(),
            },
            ResolutionRequirement::ResolveGit { service, .. }
            | ResolutionRequirement::BuildAndPush { service } => CompileError {
                code: "source_unresolved".into(),
                resource: service,
                message: "Git source has not been built into a verified image".into(),
            },
            ResolutionRequirement::ProvideSecretGeneration { logical_name } => CompileError {
                code: "secret_generation_unresolved".into(),
                resource: logical_name,
                message: "logical secret has no current runtime generation".into(),
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
        ) => {
            image == requested
                && immutable_digest_reference(digest_reference)
                && same_image_repository(image, digest_reference)
        }
        (
            Source::Git {
                repository,
                reference,
                context,
                dockerfile,
            },
            ResolvedSource::Git {
                repository: resolved_repository,
                requested_reference,
                commit,
                context: resolved_context,
                dockerfile: resolved_dockerfile,
                registry_reference,
                digest_reference,
            },
        ) => {
            repository == resolved_repository
                && reference == requested_reference
                && context == resolved_context
                && dockerfile == resolved_dockerfile
                && valid_commit(commit)
                && mutable_image_reference(registry_reference)
                && immutable_digest_reference(digest_reference)
                && same_image_repository(registry_reference, digest_reference)
        }
        _ => false,
    }
}

fn compile_service(
    service: &Service,
    app: &NormalizedApplication,
    resolutions: &ResolutionSet,
    application_ownership: &Ownership,
    private_network: &str,
    ingress_network: &str,
) -> DesiredService {
    let source = resolutions.sources[&service.name].clone();
    let routed = app
        .spec
        .routes
        .iter()
        .any(|route| route.service == service.name);
    let mut networks = vec![private_network.to_owned()];
    if routed {
        networks.push(ingress_network.to_owned());
    }
    let mut ownership = application_ownership.clone();
    ownership.service = Some(service.name.clone());
    let mut labels = ownership.labels();
    compile_traefik_labels(app, &service.name, ingress_network, &mut labels);
    DesiredService {
        logical_name: service.name.clone(),
        name: docker_resource_name(&app.id, ResourceKind::Service, Some(&service.name)),
        image: source.digest_reference().into(),
        source,
        replicas: service.replicas,
        environment: service.environment.clone(),
        command: service.command.clone(),
        arguments: service.arguments.clone(),
        ports: service.ports.clone(),
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
        secrets: service
            .secrets
            .iter()
            .map(|secret: &SecretReference| DesiredSecretMount {
                logical_name: secret.source.clone(),
                swarm_name: resolutions.secrets[&secret.source].swarm_name.clone(),
                target: secret.target.clone(),
                mode: secret.mode.clone(),
            })
            .collect(),
        healthcheck: service.healthcheck.clone(),
        resources: service.resources.clone(),
        networks,
        labels,
    }
}

fn compile_networks(
    app: &NormalizedApplication,
    ingress_network: &str,
    private_network: &str,
    ownership: &Ownership,
) -> Vec<DesiredNetwork> {
    let private = DesiredNetwork {
        name: private_network.into(),
        ingress: false,
        labels: ownership.labels(),
    };
    if app.spec.routes.is_empty() {
        vec![private]
    } else {
        vec![
            DesiredNetwork {
                name: ingress_network.into(),
                ingress: true,
                labels: BTreeMap::from([
                    (MANAGED_LABEL.into(), "true".into()),
                    (INSTANCE_LABEL.into(), ownership.instance_id.to_string()),
                ]),
            },
            private,
        ]
    }
}

fn compile_traefik_labels(
    app: &NormalizedApplication,
    service: &str,
    ingress_network: &str,
    labels: &mut BTreeMap<String, String>,
) {
    let routes = app
        .spec
        .routes
        .iter()
        .filter(|route| route.service == service);
    let mut found = false;
    for route in routes {
        found = true;
        let router_name = router_name(&app.id, &route.host, service, route.port);
        let backend = format!("{router_name}-backend");
        labels.insert("traefik.enable".into(), "true".into());
        labels.insert("traefik.swarm.network".into(), ingress_network.into());
        labels.insert(
            format!("traefik.http.routers.{router_name}.rule"),
            format!("Host(`{}`)", route.host),
        );
        labels.insert(
            format!("traefik.http.routers.{router_name}.entrypoints"),
            "web".into(),
        );
        labels.insert(
            format!("traefik.http.routers.{router_name}.service"),
            backend.clone(),
        );
        labels.insert(
            format!("traefik.http.services.{backend}.loadbalancer.server.port"),
            route.port.to_string(),
        );
    }
    if !found {
        labels.retain(|key, _| !key.starts_with("traefik."));
    }
}

fn router_name(application: &ApplicationId, host: &str, service: &str, port: u16) -> String {
    let mut hasher = Sha256::new();
    hasher.update(application.as_str().as_bytes());
    hasher.update([0]);
    hasher.update(host.as_bytes());
    hasher.update([0]);
    hasher.update(service.as_bytes());
    hasher.update(port.to_be_bytes());
    format!("r{}", lower_hex(&hasher.finalize()[..8]))
}

fn compile_secrets(
    resolutions: &ResolutionSet,
    referenced: &BTreeSet<&str>,
    ownership: &Ownership,
) -> Vec<DesiredSecret> {
    let mut secrets = resolutions
        .secrets
        .values()
        .filter(|secret| referenced.contains(secret.logical_name.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    secrets.sort_by(|left, right| left.logical_name.cmp(&right.logical_name));
    secrets
        .into_iter()
        .map(|secret| {
            let mut labels = ownership.labels();
            labels.insert(SECRET_LABEL.into(), secret.logical_name.clone());
            DesiredSecret {
                logical_name: secret.logical_name,
                generation: secret.generation,
                name: secret.swarm_name,
                labels,
            }
        })
        .collect()
}

fn referenced_secrets(app: &NormalizedApplication) -> BTreeSet<&str> {
    app.spec
        .services
        .iter()
        .flat_map(|service| service.secrets.iter().map(|secret| secret.source.as_str()))
        .collect()
}

fn valid_swarm_secret_name(logical_name: &str, value: &str, application: &ApplicationId) -> bool {
    let readable = logical_name.chars().take(15).collect::<String>();
    let application_hash = lower_hex(&Sha256::digest(application.as_str().as_bytes())[..5]);
    let prefix = format!("piqueld-secret-{readable}-");
    let suffix = format!("-{application_hash}");
    value.len() <= 64
        && value
            .strip_prefix(&prefix)
            .and_then(|value| value.strip_suffix(&suffix))
            .is_some_and(|random| {
                random.len() == 22
                    && random
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
}

fn lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

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

fn mutable_image_reference(reference: &str) -> bool {
    valid_image_reference(reference) && !reference.contains('@')
}

fn valid_commit(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
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
    /// Returns whether ownership labels identify the desired network.
    #[must_use]
    pub fn matches_ownership(
        &self,
        desired: &DesiredNetwork,
        application: &DesiredApplication,
    ) -> bool {
        if desired.ingress {
            return self.name == desired.name
                && self.labels.get(MANAGED_LABEL).map(String::as_str) == Some("true")
                && self.labels.get(INSTANCE_LABEL).map(String::as_str)
                    == Some(application.instance_id.as_str())
                && !self.labels.contains_key(APPLICATION_LABEL)
                && !self.labels.contains_key(SERVICE_LABEL);
        }
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

/// Observed Docker Swarm secret state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedSecret {
    /// Observed Docker secret name.
    pub name: String,
    /// Ownership labels observed on the secret.
    pub labels: BTreeMap<String, String>,
    /// Whether a service currently references the secret.
    pub in_use: bool,
}

impl ObservedSecret {
    /// Returns whether ownership labels identify the desired secret.
    #[must_use]
    pub fn matches_ownership(
        &self,
        desired: &DesiredSecret,
        application: &DesiredApplication,
    ) -> bool {
        OwnershipState::from_labels(&self.labels, &application.instance_id, &application.id)
            == OwnershipState::Owned
            && self.labels.get(SECRET_LABEL).map(String::as_str)
                == Some(desired.logical_name.as_str())
            && !self.labels.contains_key(SERVICE_LABEL)
            && self.name == desired.name
    }

    /// Returns whether a runtime secret is safely owned by an application.
    #[must_use]
    pub fn is_owned_by(&self, instance: &InstanceId, application: &ApplicationId) -> bool {
        OwnershipState::from_labels(&self.labels, instance, application) == OwnershipState::Owned
            && self
                .labels
                .get(SECRET_LABEL)
                .is_some_and(|logical| !logical.is_empty())
            && self.name.starts_with("piqueld-secret-")
            && !self.labels.contains_key(SERVICE_LABEL)
    }
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
    /// Ports exposed by the service endpoint.
    pub ports: Vec<u16>,
    /// Persistent mounts observed on the service.
    pub mounts: Vec<DesiredMount>,
    /// Secret mounts observed on the service.
    pub secrets: Vec<DesiredSecretMount>,
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
    /// Returns whether all desired service fields and owned runtime settings match.
    #[must_use]
    pub fn semantically_matches(&self, desired: &DesiredService) -> bool {
        self.matches(desired)
    }

    /// Returns whether all desired service fields match.
    #[must_use]
    pub fn matches(&self, desired: &DesiredService) -> bool {
        self.image == desired.image
            && self.replicas == desired.replicas
            && self.environment == desired.environment
            && self.command == desired.command
            && self.arguments == desired.arguments
            && self.ports == desired.ports
            && unordered_eq(&self.mounts, &desired.mounts)
            && unordered_eq(&self.secrets, &desired.secrets)
            && self.healthcheck == desired.healthcheck
            && self.resources == desired.resources
            && sorted(&self.networks) == sorted(&desired.networks)
            && owned_label_subset(&self.labels, &desired.labels)
            && self.runtime_configuration_matches
    }

    /// Returns whether ownership labels identify the desired service.
    #[must_use]
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
    /// Secrets observed for the application.
    pub secrets: Vec<ObservedSecret>,
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

pub(crate) fn unordered_eq<T: Ord>(observed: &[T], desired: &[T]) -> bool {
    let mut observed = observed.iter().collect::<Vec<_>>();
    let mut desired = desired.iter().collect::<Vec<_>>();
    observed.sort_unstable();
    desired.sort_unstable();
    observed == desired
}

fn sorted(values: &[String]) -> Vec<&str> {
    let mut values = values.iter().map(String::as_str).collect::<Vec<_>>();
    values.sort_unstable();
    values.dedup();
    values
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
