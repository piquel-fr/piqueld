//! Backend-neutral desired, resolved, and observed resource contracts.
#![allow(missing_docs)]

use crate::{
    ApplicationId, ResourceKind, docker_resource_name,
    manifest::{
        HealthCheck, Mount, NormalizedApplication, ResourceLimits, SecretReference, Source,
        valid_image_reference,
    },
    router_name,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fmt, str::FromStr};

pub const MANAGED_LABEL: &str = "io.piqueld.managed";
pub const INSTANCE_LABEL: &str = "io.piqueld.instance";
pub const APPLICATION_LABEL: &str = "io.piqueld.application";
pub const SERVICE_LABEL: &str = "io.piqueld.service";
pub const SPEC_HASH_LABEL: &str = "io.piqueld.spec-hash";

#[derive(
    Clone, Debug, Deserialize, Eq, Hash, JsonSchema, Ord, PartialEq, PartialOrd, Serialize,
)]
#[serde(transparent)]
pub struct InstanceId(String);

impl InstanceId {
    /// Parses a safe, stable control-plane instance identifier.
    ///
    /// # Errors
    /// Returns an error when the identifier is outside the safe label alphabet.
    pub fn parse(value: impl Into<String>) -> Result<Self, &'static str> {
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
            Err("instance IDs must be 1-64 lowercase ASCII letters, digits, or internal hyphens")
        }
    }
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl fmt::Display for InstanceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl FromStr for InstanceId {
    type Err = &'static str;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
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

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SecretGeneration {
    pub logical_name: String,
    pub generation: String,
    pub swarm_name: String,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResolutionSet {
    pub sources: BTreeMap<String, ResolvedSource>,
    pub secrets: BTreeMap<String, SecretGeneration>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
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

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
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

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DesiredNetwork {
    pub name: String,
    pub ingress: bool,
    pub labels: BTreeMap<String, String>,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DesiredVolume {
    pub logical_name: String,
    pub name: String,
    pub labels: BTreeMap<String, String>,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DesiredSecret {
    pub logical_name: String,
    pub generation: String,
    pub name: String,
    pub labels: BTreeMap<String, String>,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DesiredSecretMount {
    pub logical_name: String,
    pub swarm_name: String,
    pub target: String,
    pub mode: String,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DesiredMount {
    pub volume_name: String,
    pub target: String,
    pub read_only: bool,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
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

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
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

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
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
#[allow(clippy::too_many_lines)]
pub fn compile_application(
    app: &NormalizedApplication,
    instance_id: InstanceId,
    ingress_network: impl Into<String>,
    resolutions: &ResolutionSet,
) -> Result<DesiredApplication, Vec<CompileError>> {
    let requirements = preview_resolution(app, resolutions);
    if !requirements.is_empty() {
        let mut errors = requirements
            .into_iter()
            .map(|r| match r {
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
            })
            .collect::<Vec<_>>();
        errors.dedup_by(|left, right| left.code == right.code && left.resource == right.resource);
        return Err(errors);
    }
    let mut invalid = Vec::new();
    let referenced_secrets = app
        .spec
        .services
        .iter()
        .flat_map(|service| service.secrets.iter().map(|secret| secret.source.as_str()))
        .collect::<std::collections::BTreeSet<_>>();
    for service in &app.spec.services {
        let resolved = &resolutions.sources[&service.name];
        let matches_intent = match (&service.source, resolved) {
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
        };
        if !matches_intent {
            invalid.push(CompileError {
                code: "source_resolution_mismatch".into(),
                resource: service.name.clone(),
                message: "resolved source does not immutably resolve the normalized service source"
                    .into(),
            });
        }
    }
    for (logical_name, secret) in resolutions
        .secrets
        .iter()
        .filter(|(logical_name, _)| referenced_secrets.contains(logical_name.as_str()))
    {
        let generation_identity = format!("{logical_name}-{}", secret.generation);
        let expected_name =
            docker_resource_name(&app.id, ResourceKind::Secret, Some(&generation_identity));
        if logical_name != &secret.logical_name
            || secret.generation.is_empty()
            || secret.swarm_name != expected_name
        {
            invalid.push(CompileError {
                code: "secret_generation_invalid".into(),
                resource: logical_name.clone(),
                message: "secret generation metadata is inconsistent or its Swarm name is not the deterministic application-scoped name".into(),
            });
        }
    }
    if !invalid.is_empty() {
        return Err(invalid);
    }
    let spec_hash = app.spec_hash();
    let base = Ownership {
        instance_id: instance_id.clone(),
        application_id: app.id.clone(),
        service: None,
        spec_hash: spec_hash.clone(),
    };
    let private_name = docker_resource_name(&app.id, ResourceKind::Network, None);
    let ingress_network = ingress_network.into();
    let networks = vec![
        DesiredNetwork {
            name: ingress_network.clone(),
            ingress: true,
            labels: BTreeMap::from([
                (MANAGED_LABEL.into(), "true".into()),
                (INSTANCE_LABEL.into(), instance_id.to_string()),
            ]),
        },
        DesiredNetwork {
            name: private_name.clone(),
            ingress: false,
            labels: base.labels(),
        },
    ];
    let volumes = app
        .spec
        .volumes
        .iter()
        .map(|v| DesiredVolume {
            logical_name: v.name.clone(),
            name: docker_resource_name(&app.id, ResourceKind::Volume, Some(&v.name)),
            labels: base.labels(),
        })
        .collect::<Vec<_>>();
    let mut secrets = resolutions
        .secrets
        .values()
        .filter(|secret| referenced_secrets.contains(secret.logical_name.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    secrets.sort_by(|a, b| a.logical_name.cmp(&b.logical_name));
    let secrets = secrets
        .into_iter()
        .map(|s| DesiredSecret {
            logical_name: s.logical_name,
            generation: s.generation,
            name: s.swarm_name,
            labels: base.labels(),
        })
        .collect();
    let services = app
        .spec
        .services
        .iter()
        .map(|service| {
            let source = resolutions.sources[&service.name].clone();
            let routed = app.spec.routes.iter().any(|r| r.service == service.name);
            let mut network_names = vec![private_name.clone()];
            if routed {
                network_names.push(ingress_network.clone());
            }
            let mut ownership = base.clone();
            ownership.service = Some(service.name.clone());
            let mut labels = ownership.labels();
            compile_traefik_labels(app, &service.name, &ingress_network, &mut labels);
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
                    .map(|m: &Mount| DesiredMount {
                        volume_name: docker_resource_name(
                            &app.id,
                            ResourceKind::Volume,
                            Some(&m.volume),
                        ),
                        target: m.target.clone(),
                        read_only: m.read_only,
                    })
                    .collect(),
                secrets: service
                    .secrets
                    .iter()
                    .map(|s: &SecretReference| DesiredSecretMount {
                        logical_name: s.source.clone(),
                        swarm_name: resolutions.secrets[&s.source].swarm_name.clone(),
                        target: s.target.clone(),
                        mode: s.mode.clone(),
                    })
                    .collect(),
                healthcheck: service.healthcheck.clone(),
                resources: service.resources.clone(),
                networks: network_names,
                labels,
            }
        })
        .collect();
    Ok(DesiredApplication {
        id: app.id.clone(),
        name: app.metadata.name.clone(),
        instance_id,
        spec_hash,
        networks,
        volumes,
        secrets,
        services,
    })
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

#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
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
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedTask {
    pub state: TaskState,
    pub healthy: Option<bool>,
    pub desired_running: bool,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Convergence {
    Converged,
    Updating,
    Degraded,
    Failed,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedNetwork {
    pub name: String,
    pub ingress: bool,
    pub labels: BTreeMap<String, String>,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedVolume {
    pub name: String,
    pub labels: BTreeMap<String, String>,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedSecret {
    pub name: String,
    pub labels: BTreeMap<String, String>,
    pub in_use: bool,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
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
    pub fn semantically_matches(&self, desired: &DesiredService) -> bool {
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

#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
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
#[must_use]
pub fn ownership_state(
    labels: &BTreeMap<String, String>,
    instance: &InstanceId,
    application: &ApplicationId,
) -> OwnershipState {
    if labels.get(MANAGED_LABEL).map(String::as_str) != Some("true") {
        return OwnershipState::Invalid;
    }
    if !labels
        .get(SPEC_HASH_LABEL)
        .is_some_and(|hash| valid_sha256(hash))
    {
        return OwnershipState::Invalid;
    }
    if labels.get(INSTANCE_LABEL).map(String::as_str) != Some(instance.as_str())
        || labels.get(APPLICATION_LABEL).map(String::as_str) != Some(application.as_str())
    {
        return OwnershipState::Foreign;
    }
    OwnershipState::Owned
}

fn valid_sha256(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}
