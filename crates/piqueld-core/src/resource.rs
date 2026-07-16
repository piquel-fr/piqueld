//! Backend-neutral desired, resolved, and observed resource contracts.
#![allow(missing_docs)]

use crate::{
    ApplicationId, ResourceKind, docker_resource_name,
    manifest::{
        HealthCheck, Mount, NormalizedApplication, ResourceLimits, SecretReference, Service,
        Source, valid_image_reference,
    },
    router_name,
};
use serde::{Deserialize, Deserializer, Serialize, de};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    str::FromStr,
};
use thiserror::Error;

pub const MANAGED_LABEL: &str = "io.piqueld.managed";
pub const INSTANCE_LABEL: &str = "io.piqueld.instance";
pub const APPLICATION_LABEL: &str = "io.piqueld.application";
pub const SERVICE_LABEL: &str = "io.piqueld.service";
pub const SPEC_HASH_LABEL: &str = "io.piqueld.spec-hash";

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("instance IDs must be 1-64 lowercase ASCII letters, digits, or internal hyphens")]
pub struct InstanceIdError;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct InstanceId(String);

impl InstanceId {
    /// Parses a safe, stable control-plane instance identifier.
    ///
    /// # Errors
    /// Returns an error when the identifier is outside the safe label alphabet.
    pub fn parse(value: impl Into<String>) -> Result<Self, InstanceIdError> {
        let value = value.into();
        if (1..=64).contains(&value.len())
            && value
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
            && value
                .bytes()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric())
            && value
                .bytes()
                .last()
                .is_some_and(|c| c.is_ascii_alphanumeric())
        {
            Ok(Self(value))
        } else {
            Err(InstanceIdError)
        }
    }
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
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl FromStr for InstanceId {
    type Err = InstanceIdError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("SHA-256 digests must use the sha256:<64 lowercase hexadecimal digits> format")]
pub struct Sha256DigestError;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct Sha256Digest(String);

impl Sha256Digest {
    /// Parses an explicitly tagged lowercase SHA-256 digest.
    ///
    /// # Errors
    /// Returns an error unless the value is `sha256:` followed by 64 lowercase hex digits.
    pub fn parse(value: impl Into<String>) -> Result<Self, Sha256DigestError> {
        let value = value.into();
        if valid_sha256(&value) {
            Ok(Self(value))
        } else {
            Err(Sha256DigestError)
        }
    }

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
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl FromStr for Sha256Digest {
    type Err = Sha256DigestError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResolvedSource {
    Image {
        requested: String,
        digest_reference: String,
    },
    Git {
        repository: String,
        requested_reference: String,
        commit: String,
        context: String,
        dockerfile: String,
        registry_reference: String,
        digest_reference: String,
    },
}
impl ResolvedSource {
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SecretGeneration {
    pub logical_name: String,
    pub generation: Sha256Digest,
    pub swarm_name: String,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResolutionSet {
    pub sources: BTreeMap<String, ResolvedSource>,
    pub secrets: BTreeMap<String, SecretGeneration>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResolutionRequirement {
    ResolveImage {
        service: String,
        reference: String,
    },
    ResolveGit {
        service: String,
        repository: String,
        reference: String,
    },
    BuildAndPush {
        service: String,
    },
    ProvideSecretGeneration {
        logical_name: String,
    },
}

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
                && !requirements.iter().any(|r| matches!(r, ResolutionRequirement::ProvideSecretGeneration { logical_name } if logical_name == &secret.source))
            {
                requirements.push(ResolutionRequirement::ProvideSecretGeneration { logical_name: secret.source.clone() });
            }
        }
    }
    requirements
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Ownership {
    pub instance_id: InstanceId,
    pub application_id: ApplicationId,
    pub service: Option<String>,
    pub spec_hash: String,
}
impl Ownership {
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

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DesiredNetwork {
    pub name: String,
    pub ingress: bool,
    pub labels: BTreeMap<String, String>,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DesiredVolume {
    pub logical_name: String,
    pub name: String,
    pub labels: BTreeMap<String, String>,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DesiredSecret {
    pub logical_name: String,
    pub generation: Sha256Digest,
    pub name: String,
    pub labels: BTreeMap<String, String>,
}
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DesiredSecretMount {
    pub logical_name: String,
    pub swarm_name: String,
    pub target: String,
    pub mode: String,
}
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DesiredMount {
    pub volume_name: String,
    pub target: String,
    pub read_only: bool,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DesiredService {
    pub logical_name: String,
    pub name: String,
    pub source: ResolvedSource,
    pub image: String,
    pub replicas: u16,
    pub environment: BTreeMap<String, String>,
    pub command: Vec<String>,
    pub arguments: Vec<String>,
    pub ports: Vec<u16>,
    pub mounts: Vec<DesiredMount>,
    pub secrets: Vec<DesiredSecretMount>,
    pub healthcheck: Option<HealthCheck>,
    pub resources: Option<ResourceLimits>,
    pub networks: Vec<String>,
    pub labels: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DesiredApplication {
    pub id: ApplicationId,
    pub name: String,
    pub instance_id: InstanceId,
    pub spec_hash: String,
    pub networks: Vec<DesiredNetwork>,
    pub volumes: Vec<DesiredVolume>,
    pub secrets: Vec<DesiredSecret>,
    pub services: Vec<DesiredService>,
}
pub type ResolvedApplication = DesiredApplication;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CompileError {
    pub code: String,
    pub resource: String,
    pub message: String,
}

/// Compiles normalized intent after all mutable inputs have immutable resolutions.
///
/// # Errors
/// Returns all missing source and secret-generation resolutions.
pub fn compile_application(
    app: &NormalizedApplication,
    instance_id: InstanceId,
    ingress_network: impl Into<String>,
    resolutions: &ResolutionSet,
) -> Result<DesiredApplication, Vec<CompileError>> {
    let errors = validate_application(app, resolutions);
    if !errors.is_empty() {
        return Err(errors);
    }

    let ingress_network = ingress_network.into();
    let spec_hash = app.spec_hash();
    let ownership = Ownership {
        instance_id: instance_id.clone(),
        application_id: app.id.clone(),
        service: None,
        spec_hash: spec_hash.clone(),
    };
    let private_network = docker_resource_name(&app.id, ResourceKind::Network, None);
    let referenced_secrets = referenced_secrets(app);

    Ok(DesiredApplication {
        id: app.id.clone(),
        name: app.metadata.name.clone(),
        instance_id,
        spec_hash,
        networks: compile_networks(&ingress_network, &private_network, &ownership),
        volumes: compile_volumes(app, &ownership),
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
                    &ingress_network,
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
    if !errors.is_empty() {
        return errors;
    }

    for service in &app.spec.services {
        let resolved = &resolutions.sources[&service.name];
        if !resolved_source_matches(&service.source, resolved) {
            errors.push(CompileError {
                code: "source_resolution_mismatch".into(),
                resource: service.name.clone(),
                message: "resolved source does not immutably resolve the normalized service source"
                    .into(),
            });
        }
    }

    let referenced_secrets = referenced_secrets(app);
    for (logical_name, secret) in resolutions
        .secrets
        .iter()
        .filter(|(logical_name, _)| referenced_secrets.contains(logical_name.as_str()))
    {
        let generation_identity = format!("{logical_name}-{}", secret.generation);
        let expected_name =
            docker_resource_name(&app.id, ResourceKind::Secret, Some(&generation_identity));
        if logical_name != &secret.logical_name || secret.swarm_name != expected_name {
            errors.push(CompileError {
                code: "secret_generation_invalid".into(),
                resource: logical_name.clone(),
                message: "secret generation metadata is inconsistent or its Swarm name is not the deterministic application-scoped name".into(),
            });
        }
    }
    errors
}

fn unresolved_errors(
    app: &NormalizedApplication,
    resolutions: &ResolutionSet,
) -> Vec<CompileError> {
    let mut seen = BTreeSet::new();
    preview_resolution(app, resolutions)
        .into_iter()
        .filter_map(|requirement| {
            let error = match requirement {
                ResolutionRequirement::ResolveImage { service, .. }
                | ResolutionRequirement::ResolveGit { service, .. }
                | ResolutionRequirement::BuildAndPush { service } => CompileError {
                    code: "source_unresolved".into(),
                    resource: service,
                    message: "service source has not been resolved to an immutable image".into(),
                },
                ResolutionRequirement::ProvideSecretGeneration { logical_name } => CompileError {
                    code: "secret_generation_unresolved".into(),
                    resource: logical_name,
                    message: "logical secret has no current runtime generation".into(),
                },
            };
            seen.insert((error.code.clone(), error.resource.clone()))
                .then_some(error)
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
            requested == image
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

fn referenced_secrets(app: &NormalizedApplication) -> BTreeSet<&str> {
    app.spec
        .services
        .iter()
        .flat_map(|service| service.secrets.iter().map(|secret| secret.source.as_str()))
        .collect()
}

fn compile_networks(
    ingress_network: &str,
    private_network: &str,
    ownership: &Ownership,
) -> Vec<DesiredNetwork> {
    vec![
        DesiredNetwork {
            name: ingress_network.into(),
            ingress: true,
            labels: BTreeMap::from([
                (MANAGED_LABEL.into(), "true".into()),
                (INSTANCE_LABEL.into(), ownership.instance_id.to_string()),
            ]),
        },
        DesiredNetwork {
            name: private_network.into(),
            ingress: false,
            labels: ownership.labels(),
        },
    ]
}

fn compile_volumes(app: &NormalizedApplication, ownership: &Ownership) -> Vec<DesiredVolume> {
    app.spec
        .volumes
        .iter()
        .map(|volume| DesiredVolume {
            logical_name: volume.name.clone(),
            name: docker_resource_name(&app.id, ResourceKind::Volume, Some(&volume.name)),
            labels: ownership.labels(),
        })
        .collect()
}

fn compile_secrets(
    resolutions: &ResolutionSet,
    referenced_secrets: &BTreeSet<&str>,
    ownership: &Ownership,
) -> Vec<DesiredSecret> {
    let mut secrets = resolutions
        .secrets
        .values()
        .filter(|secret| referenced_secrets.contains(secret.logical_name.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    secrets.sort_by(|a, b| a.logical_name.cmp(&b.logical_name));
    secrets
        .into_iter()
        .map(|secret| DesiredSecret {
            logical_name: secret.logical_name,
            generation: secret.generation,
            name: secret.swarm_name,
            labels: ownership.labels(),
        })
        .collect()
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
    let mut networks = vec![private_network.into()];
    if routed {
        networks.push(ingress_network.into());
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
    valid_image_reference(reference)
        && !reference.contains('@')
        && image_repository(reference).is_some()
}

fn same_image_repository(reference: &str, digest_reference: &str) -> bool {
    image_repository(reference)
        .zip(image_repository(digest_reference))
        .is_some_and(|(requested, resolved)| requested == resolved)
}

fn image_repository(reference: &str) -> Option<String> {
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
    let explicit_registry =
        repository.contains('/') && (first.contains(['.', ':']) || first == "localhost");
    if explicit_registry {
        return Some(repository.to_owned());
    }
    if repository.contains('/') {
        Some(format!("docker.io/{repository}"))
    } else {
        Some(format!("docker.io/library/{repository}"))
    }
}

fn valid_commit(commit: &str) -> bool {
    matches!(commit.len(), 40 | 64)
        && commit
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn compile_traefik_labels(
    app: &NormalizedApplication,
    service: &str,
    ingress_network: &str,
    labels: &mut BTreeMap<String, String>,
) {
    let service_routes = app
        .spec
        .routes
        .iter()
        .filter(|r| r.service == service)
        .collect::<Vec<_>>();
    if service_routes.is_empty() {
        return;
    }
    labels.insert("traefik.enable".into(), "true".into());
    labels.insert("traefik.swarm.network".into(), ingress_network.into());
    for route in service_routes {
        let router = router_name(&app.id, &route.host, service, route.port);
        let backend = format!("{router}-backend");
        labels.insert(
            format!("traefik.http.routers.{router}.rule"),
            format!("Host(`{}`)", route.host),
        );
        labels.insert(
            format!("traefik.http.routers.{router}.entrypoints"),
            "web".into(),
        );
        labels.insert(
            format!("traefik.http.routers.{router}.service"),
            backend.clone(),
        );
        labels.insert(
            format!("traefik.http.services.{backend}.loadbalancer.server.port"),
            route.port.to_string(),
        );
    }
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    New,
    Pending,
    Assigned,
    Accepted,
    Preparing,
    Starting,
    Running,
    Complete,
    Failed,
    Rejected,
    Shutdown,
    #[default]
    Unknown,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedTask {
    pub state: TaskState,
    pub healthy: Option<bool>,
    pub desired_running: bool,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Convergence {
    Converged,
    Updating,
    Degraded,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedNetwork {
    pub name: String,
    pub ingress: bool,
    pub labels: BTreeMap<String, String>,
}
impl ObservedNetwork {
    #[must_use]
    pub fn matches_ownership(
        &self,
        desired_network: &DesiredNetwork,
        desired_application: &DesiredApplication,
    ) -> bool {
        if desired_network.ingress {
            self.labels.get(MANAGED_LABEL).map(String::as_str) == Some("true")
                && self.labels.get(INSTANCE_LABEL).map(String::as_str)
                    == Some(desired_application.instance_id.as_str())
        } else {
            OwnershipState::from_labels(
                &self.labels,
                &desired_application.instance_id,
                &desired_application.id,
            ) == OwnershipState::Owned
        }
    }
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedVolume {
    pub name: String,
    pub labels: BTreeMap<String, String>,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedSecret {
    pub name: String,
    pub labels: BTreeMap<String, String>,
    pub in_use: bool,
}
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedService {
    pub name: String,
    pub image: String,
    pub replicas: u16,
    pub environment: BTreeMap<String, String>,
    pub command: Vec<String>,
    pub arguments: Vec<String>,
    pub ports: Vec<u16>,
    pub mounts: Vec<DesiredMount>,
    pub secrets: Vec<DesiredSecretMount>,
    pub healthcheck: Option<HealthCheck>,
    pub resources: Option<ResourceLimits>,
    pub networks: Vec<String>,
    pub labels: BTreeMap<String, String>,
    pub tasks: Vec<ObservedTask>,
    pub convergence: Convergence,
}

impl ObservedService {
    #[must_use]
    pub fn matches(&self, desired: &DesiredService) -> bool {
        self.image == desired.image
            && self.replicas == desired.replicas
            && self.environment == desired.environment
            && self.command == desired.command
            && self.arguments == desired.arguments
            && unordered_eq(&self.ports, &desired.ports)
            && unordered_eq(&self.mounts, &desired.mounts)
            && unordered_eq(&self.secrets, &desired.secrets)
            && self.healthcheck == desired.healthcheck
            && self.resources == desired.resources
            && sorted(&self.networks) == sorted(&desired.networks)
            && owned_label_subset(&self.labels, &desired.labels)
    }

    #[must_use]
    pub fn matches_ownership(
        &self,
        desired_service: &DesiredService,
        desired_application: &DesiredApplication,
    ) -> bool {
        OwnershipState::from_labels(
            &self.labels,
            &desired_application.instance_id,
            &desired_application.id,
        ) == OwnershipState::Owned
            && self.labels.get(SERVICE_LABEL).map(String::as_str)
                == Some(desired_service.logical_name.as_str())
    }

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
fn unordered_eq<T: Ord>(observed: &[T], desired: &[T]) -> bool {
    let mut observed = observed.iter().collect::<Vec<_>>();
    let mut desired = desired.iter().collect::<Vec<_>>();
    observed.sort_unstable();
    desired.sort_unstable();
    observed == desired
}
fn sorted(values: &[String]) -> Vec<&str> {
    let mut v = values.iter().map(String::as_str).collect::<Vec<_>>();
    v.sort_unstable();
    v.dedup();
    v
}
fn owned_label_subset(
    observed: &BTreeMap<String, String>,
    desired: &BTreeMap<String, String>,
) -> bool {
    let relevant = |key: &str| key.starts_with("io.piqueld.") || key.starts_with("traefik.");
    desired.iter().all(|(k, v)| observed.get(k) == Some(v))
        && observed
            .iter()
            .filter(|(k, _)| relevant(k))
            .all(|(k, v)| desired.get(k) == Some(v))
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedApplication {
    pub networks: Vec<ObservedNetwork>,
    pub volumes: Vec<ObservedVolume>,
    pub secrets: Vec<ObservedSecret>,
    pub services: Vec<ObservedService>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OwnershipState {
    Owned,
    Foreign,
    Invalid,
}
impl OwnershipState {
    #[must_use]
    pub fn from_labels(
        labels: &BTreeMap<String, String>,
        instance: &InstanceId,
        application: &ApplicationId,
    ) -> Self {
        if labels.get(MANAGED_LABEL).map(String::as_str) != Some("true") {
            return Self::Invalid;
        }
        if !labels
            .get(SPEC_HASH_LABEL)
            .is_some_and(|hash| valid_sha256(hash))
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
    fn instance_ids_preserve_their_invariant_at_parse_and_deserialization_boundaries() {
        assert_eq!(InstanceId::parse("UPPERCASE").unwrap_err(), InstanceIdError);
        assert!(serde_json::from_str::<InstanceId>(r#""home-1""#).is_ok());
        assert!(serde_json::from_str::<InstanceId>(r#""-invalid""#).is_err());
    }

    #[test]
    fn sha256_digests_are_explicitly_typed_and_validated() {
        let valid = format!("sha256:{}", "a".repeat(64));
        assert_eq!(Sha256Digest::parse(&valid).unwrap().as_str(), valid);
        assert_eq!(Sha256Digest::parse("g1").unwrap_err(), Sha256DigestError);
        assert!(serde_json::from_str::<Sha256Digest>(r#""sha256:abc""#).is_err());
    }
}
