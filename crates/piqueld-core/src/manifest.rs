//! Public manifests and their validated, canonical domain representation.
#![allow(missing_docs)]

use crate::ApplicationId;
use schemars::{JsonSchema, schema::RootSchema};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};

/// Only application API version accepted by this prototype.
pub const APPLICATION_API_VERSION: &str = "piqueld.dev/v1alpha1";
/// Only resource kind accepted by this prototype.
pub const APPLICATION_KIND: &str = "Application";

/// Strict public application request and export shape.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicationManifest {
    pub api_version: String,
    pub kind: String,
    pub metadata: MetadataInput,
    pub spec: ApplicationSpecInput,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MetadataInput {
    pub name: String,
}

#[derive(Clone, Debug, Default, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct ApplicationSpecInput {
    pub services: Vec<ServiceInput>,
    pub volumes: Vec<VolumeInput>,
    pub routes: Vec<RouteInput>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceInput {
    pub name: String,
    pub source: SourceInput,
    #[serde(default = "default_replicas")]
    pub replicas: u16,
    #[serde(default)]
    pub environment: BTreeMap<String, String>,
    #[serde(default)]
    pub command: Vec<String>,
    #[serde(default)]
    pub arguments: Vec<String>,
    #[serde(default)]
    pub ports: Vec<u16>,
    #[serde(default)]
    pub mounts: Vec<MountInput>,
    #[serde(default)]
    pub secrets: Vec<SecretReferenceInput>,
    pub healthcheck: Option<HealthCheckInput>,
    pub resources: Option<ResourceLimitsInput>,
}

fn default_replicas() -> u16 {
    1
}

/// Exactly one supported service source. The tag makes image/Git exhaustive.
#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum SourceInput {
    Image {
        image: String,
    },
    Git {
        repository: String,
        #[serde(default = "default_git_reference")]
        reference: String,
        #[serde(default = "default_context")]
        context: String,
        #[serde(default = "default_dockerfile")]
        dockerfile: String,
    },
}
fn default_git_reference() -> String {
    "main".into()
}
fn default_context() -> String {
    ".".into()
}
fn default_dockerfile() -> String {
    "Dockerfile".into()
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeInput {
    pub name: String,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MountInput {
    pub volume: String,
    pub target: String,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SecretReferenceInput {
    pub source: String,
    pub target: Option<String>,
    #[serde(default = "default_secret_mode")]
    pub mode: String,
}
fn default_secret_mode() -> String {
    "0400".into()
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RouteInput {
    pub host: String,
    pub service: String,
    pub port: u16,
}

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum HealthCheckInput {
    Http {
        port: u16,
        #[serde(default = "default_health_path")]
        path: String,
        #[serde(default = "default_interval")]
        interval_seconds: u32,
        #[serde(default = "default_timeout")]
        timeout_seconds: u32,
    },
    Command {
        command: Vec<String>,
        #[serde(default = "default_interval")]
        interval_seconds: u32,
        #[serde(default = "default_timeout")]
        timeout_seconds: u32,
    },
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

#[derive(Clone, Debug, Deserialize, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceLimitsInput {
    pub cpu_millis: Option<u32>,
    pub memory_bytes: Option<u64>,
}

/// A field-level, safe validation error.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ValidationError {
    pub code: String,
    pub path: String,
    pub message: String,
}

/// All independently discoverable manifest errors, in stable path order.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ValidationErrors(pub Vec<ValidationError>);

impl fmt::Display for ValidationErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "application manifest has {} error(s)", self.0.len())
    }
}
impl std::error::Error for ValidationErrors {}

/// Validated domain application. Collection order still reflects user input.
#[derive(Clone, Debug, PartialEq)]
pub struct ValidatedApplication {
    name: String,
    spec: ApplicationSpec,
}

/// Canonical desired application plus persistence-assigned identity.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NormalizedApplication {
    pub id: ApplicationId,
    pub api_version: String,
    pub kind: String,
    pub metadata: Metadata,
    pub spec: ApplicationSpec,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Metadata {
    pub name: String,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicationSpec {
    pub services: Vec<Service>,
    pub volumes: Vec<Volume>,
    pub routes: Vec<Route>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Service {
    pub name: String,
    pub source: Source,
    pub replicas: u16,
    pub environment: BTreeMap<String, String>,
    pub command: Vec<String>,
    pub arguments: Vec<String>,
    pub ports: Vec<u16>,
    pub mounts: Vec<Mount>,
    pub secrets: Vec<SecretReference>,
    pub healthcheck: Option<HealthCheck>,
    pub resources: Option<ResourceLimits>,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum Source {
    Image {
        image: String,
    },
    Git {
        repository: String,
        reference: String,
        context: String,
        dockerfile: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Volume {
    pub name: String,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Mount {
    pub volume: String,
    pub target: String,
    pub read_only: bool,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SecretReference {
    pub source: String,
    pub target: String,
    pub mode: String,
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Route {
    pub host: String,
    pub service: String,
    pub port: u16,
}

#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum HealthCheck {
    Http {
        port: u16,
        path: String,
        interval_seconds: u32,
        timeout_seconds: u32,
    },
    Command {
        command: Vec<String>,
        interval_seconds: u32,
        timeout_seconds: u32,
    },
}
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResourceLimits {
    pub cpu_millis: Option<u32>,
    pub memory_bytes: Option<u64>,
}

/// Parses and validates strict TOML without performing I/O.
///
/// # Errors
/// Returns validation errors, or one safe decode error for malformed input.
pub fn parse_toml(input: &str) -> Result<ValidatedApplication, ValidationErrors> {
    let manifest = toml::from_str(input).map_err(|_| decode_error())?;
    validate(manifest)
}

/// Parses and validates strict JSON without performing I/O.
///
/// # Errors
/// Returns validation errors, or one safe decode error for malformed input.
pub fn parse_json(input: &str) -> Result<ValidatedApplication, ValidationErrors> {
    let manifest = serde_json::from_str(input).map_err(|_| decode_error())?;
    validate(manifest)
}

fn decode_error() -> ValidationErrors {
    ValidationErrors(vec![ValidationError {
        code: "manifest_decode_failed".into(),
        path: "$".into(),
        message: "manifest does not match the strict piqueld.dev/v1alpha1 Application schema"
            .into(),
    }])
}

#[allow(clippy::too_many_lines)]
fn validate(input: ApplicationManifest) -> Result<ValidatedApplication, ValidationErrors> {
    let mut errors = Vec::new();
    if input.api_version != APPLICATION_API_VERSION {
        error(
            &mut errors,
            "api_version_unsupported",
            "api_version",
            "unsupported application API version",
        );
    }
    if input.kind != APPLICATION_KIND {
        error(
            &mut errors,
            "kind_unsupported",
            "kind",
            "resource kind must be Application",
        );
    }
    validate_name(&input.metadata.name, "metadata.name", &mut errors);
    let service_names = unique_names(
        input.spec.services.iter().map(|s| &s.name),
        "spec.services",
        &mut errors,
    );
    let volume_names = unique_names(
        input.spec.volumes.iter().map(|v| &v.name),
        "spec.volumes",
        &mut errors,
    );
    let mut route_hosts = BTreeSet::new();
    let mut route_keys = BTreeSet::new();

    for (index, service) in input.spec.services.iter().enumerate() {
        let base = format!("spec.services[{index}]");
        validate_name(&service.name, &format!("{base}.name"), &mut errors);
        if !(1..=100).contains(&service.replicas) {
            error(
                &mut errors,
                "replicas_out_of_range",
                &format!("{base}.replicas"),
                "replicas must be between 1 and 100",
            );
        }
        match &service.source {
            SourceInput::Image { image } => {
                if image.is_empty() || image.chars().any(char::is_whitespace) {
                    error(
                        &mut errors,
                        "image_invalid",
                        &format!("{base}.source.image"),
                        "image must be a non-empty reference without whitespace",
                    );
                }
            }
            SourceInput::Git {
                repository,
                reference,
                context,
                dockerfile,
            } => {
                if !(repository.starts_with("https://") || repository.starts_with("http://")) {
                    error(
                        &mut errors,
                        "git_repository_unsupported",
                        &format!("{base}.source.repository"),
                        "only HTTP(S) Git repositories are supported",
                    );
                }
                if reference.trim().is_empty() {
                    error(
                        &mut errors,
                        "git_reference_invalid",
                        &format!("{base}.source.reference"),
                        "Git reference must not be empty",
                    );
                }
                validate_relative_path(context, &format!("{base}.source.context"), &mut errors);
                validate_relative_path(
                    dockerfile,
                    &format!("{base}.source.dockerfile"),
                    &mut errors,
                );
            }
        }
        for (key, value) in &service.environment {
            if !valid_env_name(key) {
                error(
                    &mut errors,
                    "environment_name_invalid",
                    &format!("{base}.environment.{key}"),
                    "environment names must use letters, digits, and underscores and cannot start with a digit",
                );
            }
            if value.contains('\0') {
                error(
                    &mut errors,
                    "environment_value_invalid",
                    &format!("{base}.environment.{key}"),
                    "environment values cannot contain NUL",
                );
            }
        }
        let mut mount_targets = BTreeSet::new();
        for (mount_index, mount) in service.mounts.iter().enumerate() {
            let path = format!("{base}.mounts[{mount_index}]");
            if !volume_names.contains(&mount.volume) {
                error(
                    &mut errors,
                    "mount_volume_missing",
                    &format!("{path}.volume"),
                    "mount references an undeclared volume",
                );
            }
            validate_absolute_path(&mount.target, &format!("{path}.target"), &mut errors);
            if !mount_targets.insert(&mount.target) {
                error(
                    &mut errors,
                    "mount_target_duplicate",
                    &format!("{path}.target"),
                    "mount target is duplicated in this service",
                );
            }
        }
        let mut secret_targets = BTreeSet::new();
        for (secret_index, secret) in service.secrets.iter().enumerate() {
            let path = format!("{base}.secrets[{secret_index}]");
            validate_name(&secret.source, &format!("{path}.source"), &mut errors);
            let target = secret.target.as_deref().unwrap_or(&secret.source);
            validate_secret_target(target, &format!("{path}.target"), &mut errors);
            if !secret_targets.insert(target) {
                error(
                    &mut errors,
                    "secret_target_duplicate",
                    &format!("{path}.target"),
                    "secret target is duplicated in this service",
                );
            }
            if !valid_mode(&secret.mode) {
                error(
                    &mut errors,
                    "secret_mode_invalid",
                    &format!("{path}.mode"),
                    "secret mode must be four octal digits and may not grant write access",
                );
            }
        }
        for (port_index, port) in service.ports.iter().enumerate() {
            if *port == 0 {
                error(
                    &mut errors,
                    "port_invalid",
                    &format!("{base}.ports[{port_index}]"),
                    "port must be between 1 and 65535",
                );
            }
        }
        if let Some(health) = &service.healthcheck {
            validate_health(health, &format!("{base}.healthcheck"), &mut errors);
            if let HealthCheckInput::Http { port, .. } = health {
                if !service.ports.is_empty() && !service.ports.contains(port) {
                    error(
                        &mut errors,
                        "healthcheck_port_missing",
                        &format!("{base}.healthcheck.port"),
                        "health-check port is not declared by its service",
                    );
                }
            }
        }
        if let Some(resources) = &service.resources {
            if resources.cpu_millis == Some(0) {
                error(
                    &mut errors,
                    "cpu_limit_invalid",
                    &format!("{base}.resources.cpu_millis"),
                    "CPU limit must be greater than zero",
                );
            }
            if resources.memory_bytes == Some(0) {
                error(
                    &mut errors,
                    "memory_limit_invalid",
                    &format!("{base}.resources.memory_bytes"),
                    "memory limit must be greater than zero",
                );
            }
        }
    }
    for (index, volume) in input.spec.volumes.iter().enumerate() {
        validate_name(
            &volume.name,
            &format!("spec.volumes[{index}].name"),
            &mut errors,
        );
    }
    for (index, route) in input.spec.routes.iter().enumerate() {
        let base = format!("spec.routes[{index}]");
        validate_hostname(&route.host, &format!("{base}.host"), &mut errors);
        if !service_names.contains(&route.service) {
            error(
                &mut errors,
                "route_service_missing",
                &format!("{base}.service"),
                "route references an undeclared service",
            );
        }
        if let Some(service) = input
            .spec
            .services
            .iter()
            .find(|service| service.name == route.service)
        {
            if !service.ports.is_empty() && !service.ports.contains(&route.port) {
                error(
                    &mut errors,
                    "route_port_missing",
                    &format!("{base}.port"),
                    "route port is not declared by its service",
                );
            }
        }
        if route.port == 0 {
            error(
                &mut errors,
                "port_invalid",
                &format!("{base}.port"),
                "port must be between 1 and 65535",
            );
        }
        let host = route.host.to_ascii_lowercase();
        if !route_keys.insert((host.clone(), route.service.clone(), route.port)) {
            error(&mut errors, "route_duplicate", &base, "route is duplicated");
        }
        if !route_hosts.insert(host) {
            error(
                &mut errors,
                "public_route_conflict",
                &format!("{base}.host"),
                "only one public route may own a hostname",
            );
        }
    }
    errors.sort_by(|a, b| a.path.cmp(&b.path).then(a.code.cmp(&b.code)));
    if !errors.is_empty() {
        return Err(ValidationErrors(errors));
    }

    Ok(ValidatedApplication {
        name: input.metadata.name,
        spec: convert_spec(input.spec),
    })
}

fn unique_names<'a>(
    names: impl Iterator<Item = &'a String>,
    path: &str,
    errors: &mut Vec<ValidationError>,
) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for (index, name) in names.enumerate() {
        if !found.insert(name.clone()) {
            error(
                errors,
                if path.ends_with("services") {
                    "service_name_duplicate"
                } else {
                    "volume_name_duplicate"
                },
                &format!("{path}[{index}].name"),
                "name is duplicated",
            );
        }
    }
    found
}

fn convert_spec(input: ApplicationSpecInput) -> ApplicationSpec {
    ApplicationSpec {
        services: input
            .services
            .into_iter()
            .map(|s| Service {
                name: s.name,
                replicas: s.replicas,
                environment: s.environment,
                command: s.command,
                arguments: s.arguments,
                ports: s.ports,
                mounts: s
                    .mounts
                    .into_iter()
                    .map(|m| Mount {
                        volume: m.volume,
                        target: m.target,
                        read_only: m.read_only,
                    })
                    .collect(),
                secrets: s
                    .secrets
                    .into_iter()
                    .map(|v| SecretReference {
                        target: v.target.unwrap_or_else(|| v.source.clone()),
                        source: v.source,
                        mode: v.mode,
                    })
                    .collect(),
                source: match s.source {
                    SourceInput::Image { image } => Source::Image { image },
                    SourceInput::Git {
                        repository,
                        reference,
                        context,
                        dockerfile,
                    } => Source::Git {
                        repository,
                        reference,
                        context,
                        dockerfile,
                    },
                },
                healthcheck: s.healthcheck.map(|h| match h {
                    HealthCheckInput::Http {
                        port,
                        path,
                        interval_seconds,
                        timeout_seconds,
                    } => HealthCheck::Http {
                        port,
                        path,
                        interval_seconds,
                        timeout_seconds,
                    },
                    HealthCheckInput::Command {
                        command,
                        interval_seconds,
                        timeout_seconds,
                    } => HealthCheck::Command {
                        command,
                        interval_seconds,
                        timeout_seconds,
                    },
                }),
                resources: s.resources.map(|r| ResourceLimits {
                    cpu_millis: r.cpu_millis,
                    memory_bytes: r.memory_bytes,
                }),
            })
            .collect(),
        volumes: input
            .volumes
            .into_iter()
            .map(|v| Volume { name: v.name })
            .collect(),
        routes: input
            .routes
            .into_iter()
            .map(|r| Route {
                host: r.host.to_ascii_lowercase(),
                service: r.service,
                port: r.port,
            })
            .collect(),
    }
}

impl ValidatedApplication {
    /// Returns editable application metadata name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the validated domain specification before canonical sorting.
    #[must_use]
    pub fn spec(&self) -> &ApplicationSpec {
        &self.spec
    }

    /// Canonicalizes all semantically unordered collections and attaches a stable ID.
    #[must_use]
    pub fn normalize(self, id: ApplicationId) -> NormalizedApplication {
        let mut spec = self.spec;
        normalize_spec(&mut spec);
        NormalizedApplication {
            id,
            api_version: APPLICATION_API_VERSION.into(),
            kind: APPLICATION_KIND.into(),
            metadata: Metadata { name: self.name },
            spec,
        }
    }

    /// Logical secret names for repository-backed existence validation in Plan 04.
    #[must_use]
    pub fn logical_secret_references(&self) -> BTreeSet<&str> {
        self.spec
            .services
            .iter()
            .flat_map(|s| s.secrets.iter().map(|v| v.source.as_str()))
            .collect()
    }

    /// Applies repository knowledge without coupling the domain to persistence.
    /// The callback receives logical names only; it must never return plaintext.
    ///
    /// # Errors
    /// Returns field errors for every reference the callback reports as absent.
    pub fn validate_secret_references(
        &self,
        exists: impl Fn(&str) -> bool,
    ) -> Result<(), ValidationErrors> {
        let mut errors = Vec::new();
        for (service_index, service) in self.spec.services.iter().enumerate() {
            for (secret_index, secret) in service.secrets.iter().enumerate() {
                if !exists(&secret.source) {
                    error(
                        &mut errors,
                        "logical_secret_missing",
                        &format!("spec.services[{service_index}].secrets[{secret_index}].source"),
                        "logical secret does not exist",
                    );
                }
            }
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(ValidationErrors(errors))
        }
    }
}

fn normalize_spec(spec: &mut ApplicationSpec) {
    spec.services.sort_by(|a, b| a.name.cmp(&b.name));
    for service in &mut spec.services {
        service.ports.sort_unstable();
        service.ports.dedup();
        service.mounts.sort();
        service.secrets.sort();
    }
    spec.volumes.sort();
    spec.routes.sort();
}

impl NormalizedApplication {
    /// Reapplies canonical ordering. This operation is idempotent.
    #[must_use]
    pub fn normalize(mut self) -> Self {
        normalize_spec(&mut self.spec);
        self
    }

    /// Versioned SHA-256 over canonical JSON after defaults and normalization.
    ///
    /// # Panics
    /// Panics only if Serde cannot JSON-encode the primitive domain fields.
    #[must_use]
    pub fn spec_hash(&self) -> String {
        #[derive(Serialize)]
        struct Envelope<'a> {
            hash_version: &'static str,
            metadata: &'a Metadata,
            spec: &'a ApplicationSpec,
        }
        let bytes = serde_json::to_vec(&Envelope {
            hash_version: "piqueld-spec-hash/v1",
            metadata: &self.metadata,
            spec: &self.spec,
        })
        .expect("domain serialization is infallible");
        format!("sha256:{:x}", Sha256::digest(bytes))
    }

    /// Canonical JSON representation used for durable desired state.
    ///
    /// # Errors
    /// Propagates a JSON serialization error.
    pub fn canonical_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Portable desired TOML. Internal identity is intentionally omitted and only
    /// logical secret references can be represented by these types.
    ///
    /// # Errors
    /// Propagates a TOML serialization error.
    pub fn export_toml(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(&self.to_manifest())
    }

    fn to_manifest(&self) -> ApplicationManifest {
        ApplicationManifest {
            api_version: self.api_version.clone(),
            kind: self.kind.clone(),
            metadata: MetadataInput {
                name: self.metadata.name.clone(),
            },
            spec: ApplicationSpecInput {
                services: self
                    .spec
                    .services
                    .iter()
                    .map(|s| ServiceInput {
                        name: s.name.clone(),
                        replicas: s.replicas,
                        environment: s.environment.clone(),
                        command: s.command.clone(),
                        arguments: s.arguments.clone(),
                        ports: s.ports.clone(),
                        mounts: s
                            .mounts
                            .iter()
                            .map(|m| MountInput {
                                volume: m.volume.clone(),
                                target: m.target.clone(),
                                read_only: m.read_only,
                            })
                            .collect(),
                        secrets: s
                            .secrets
                            .iter()
                            .map(|v| SecretReferenceInput {
                                source: v.source.clone(),
                                target: Some(v.target.clone()),
                                mode: v.mode.clone(),
                            })
                            .collect(),
                        source: match &s.source {
                            Source::Image { image } => SourceInput::Image {
                                image: image.clone(),
                            },
                            Source::Git {
                                repository,
                                reference,
                                context,
                                dockerfile,
                            } => SourceInput::Git {
                                repository: repository.clone(),
                                reference: reference.clone(),
                                context: context.clone(),
                                dockerfile: dockerfile.clone(),
                            },
                        },
                        healthcheck: s.healthcheck.as_ref().map(|h| match h {
                            HealthCheck::Http {
                                port,
                                path,
                                interval_seconds,
                                timeout_seconds,
                            } => HealthCheckInput::Http {
                                port: *port,
                                path: path.clone(),
                                interval_seconds: *interval_seconds,
                                timeout_seconds: *timeout_seconds,
                            },
                            HealthCheck::Command {
                                command,
                                interval_seconds,
                                timeout_seconds,
                            } => HealthCheckInput::Command {
                                command: command.clone(),
                                interval_seconds: *interval_seconds,
                                timeout_seconds: *timeout_seconds,
                            },
                        }),
                        resources: s.resources.as_ref().map(|r| ResourceLimitsInput {
                            cpu_millis: r.cpu_millis,
                            memory_bytes: r.memory_bytes,
                        }),
                    })
                    .collect(),
                volumes: self
                    .spec
                    .volumes
                    .iter()
                    .map(|v| VolumeInput {
                        name: v.name.clone(),
                    })
                    .collect(),
                routes: self
                    .spec
                    .routes
                    .iter()
                    .map(|r| RouteInput {
                        host: r.host.clone(),
                        service: r.service.clone(),
                        port: r.port,
                    })
                    .collect(),
            },
        }
    }
}

/// JSON Schema snapshot source for strict application requests.
#[must_use]
pub fn application_manifest_schema() -> RootSchema {
    schemars::schema_for!(ApplicationManifest)
}
/// JSON Schema snapshot source for canonical application responses.
#[must_use]
pub fn normalized_application_schema() -> RootSchema {
    schemars::schema_for!(NormalizedApplication)
}

fn validate_name(value: &str, path: &str, errors: &mut Vec<ValidationError>) {
    if value.is_empty()
        || value.len() > 63
        || !value.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
        || !value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        || value.ends_with('-')
    {
        error(
            errors,
            "name_invalid",
            path,
            "names must be 1-63 lowercase letters, digits, or hyphens, start with a letter, and end with a letter or digit",
        );
    }
}
fn validate_hostname(value: &str, path: &str, errors: &mut Vec<ValidationError>) {
    let valid = !value.is_empty()
        && value.len() <= 253
        && !value.ends_with('.')
        && value.split('.').count() >= 2
        && value.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        });
    if !valid {
        error(
            errors,
            "route_host_invalid",
            path,
            "route host must be a valid fully qualified DNS hostname without a trailing dot",
        );
    }
}
fn validate_relative_path(value: &str, path: &str, errors: &mut Vec<ValidationError>) {
    if value.is_empty()
        || value.starts_with('/')
        || value.contains('\0')
        || value.split('/').any(|p| p == "..")
    {
        error(
            errors,
            "source_path_unsafe",
            path,
            "source paths must be relative and cannot traverse parent directories",
        );
    }
}
fn validate_absolute_path(value: &str, path: &str, errors: &mut Vec<ValidationError>) {
    if !value.starts_with('/')
        || value == "/"
        || value.contains('\0')
        || value.split('/').any(|p| p == ".." || p == ".")
    {
        error(
            errors,
            "mount_target_unsafe",
            path,
            "mount target must be an absolute, normalized container path below root",
        );
    }
}
fn validate_secret_target(value: &str, path: &str, errors: &mut Vec<ValidationError>) {
    if value.starts_with('/') {
        validate_absolute_path(value, path, errors);
    } else if value.is_empty() || value.contains('/') || value == "." || value == ".." {
        error(
            errors,
            "secret_target_unsafe",
            path,
            "secret target must be a safe file name or absolute normalized container path",
        );
    }
}
fn valid_env_name(value: &str) -> bool {
    !value.is_empty()
        && !value.as_bytes()[0].is_ascii_digit()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_')
}
fn valid_mode(value: &str) -> bool {
    value.len() == 4
        && value.bytes().all(|b| matches!(b, b'0'..=b'7'))
        && value.as_bytes()[2] <= b'5'
        && value.as_bytes()[3] <= b'5'
}
fn validate_health(value: &HealthCheckInput, path: &str, errors: &mut Vec<ValidationError>) {
    let (interval, timeout) = match value {
        HealthCheckInput::Http {
            port,
            path: request_path,
            interval_seconds,
            timeout_seconds,
        } => {
            if *port == 0 {
                error(
                    errors,
                    "port_invalid",
                    &format!("{path}.port"),
                    "port must be between 1 and 65535",
                );
            }
            if !request_path.starts_with('/')
                || request_path.contains(char::is_whitespace)
                || request_path.contains('\0')
            {
                error(
                    errors,
                    "healthcheck_path_invalid",
                    &format!("{path}.path"),
                    "HTTP health-check path must start with / and contain no whitespace",
                );
            }
            (*interval_seconds, *timeout_seconds)
        }
        HealthCheckInput::Command {
            command,
            interval_seconds,
            timeout_seconds,
        } => {
            if command.is_empty() || command.iter().any(|v| v.contains('\0')) {
                error(
                    errors,
                    "healthcheck_command_invalid",
                    &format!("{path}.command"),
                    "health-check command must contain at least one NUL-free argument",
                );
            }
            (*interval_seconds, *timeout_seconds)
        }
    };
    if interval == 0 {
        error(
            errors,
            "healthcheck_interval_invalid",
            &format!("{path}.interval_seconds"),
            "health-check interval must be greater than zero",
        );
    }
    if timeout == 0 || timeout > interval {
        error(
            errors,
            "healthcheck_timeout_invalid",
            &format!("{path}.timeout_seconds"),
            "health-check timeout must be greater than zero and no longer than its interval",
        );
    }
}
fn error(errors: &mut Vec<ValidationError>, code: &str, path: &str, message: &str) {
    errors.push(ValidationError {
        code: code.into(),
        path: path.into(),
        message: message.into(),
    });
}
