//! Public manifests and their validated, canonical domain representation.

use crate::ApplicationId;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};
use url::Url;
use utoipa::ToSchema;

/// Only application API version accepted by this prototype.
pub const APPLICATION_API_VERSION: &str = "piqueld.dev/v1alpha1";
/// Only resource kind accepted by this prototype.
pub const APPLICATION_KIND: &str = "Application";

/// Strict public application request and export shape.
/// User-declared application service.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ApplicationManifest {
    /// API version string.
    pub api_version: String,
    /// Resource kind string.
    pub kind: String,
    /// User-provided metadata.
    pub metadata: MetadataInput,
    /// Desired application resources.
    pub spec: ApplicationSpecInput,
}
/// User-provided application metadata.
/// User-declared persistent volume.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MetadataInput {
    /// User-facing application name.
    pub name: String,
}

/// User-provided application resource lists.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ApplicationSpecInput {
    /// Declared services.
    pub services: Vec<ServiceInput>,
    /// Declared persistent volumes.
    pub volumes: Vec<VolumeInput>,
    /// Declared HTTP routes.
    pub routes: Vec<RouteInput>,
}

/// User-declared logical secret mount.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ServiceInput {
    /// Logical service name.
    pub name: String,
    /// Image or Git source.
    pub source: SourceInput,
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
    /// Published ports.
    #[serde(default)]
    pub ports: Vec<u16>,
    /// Persistent volume mounts.
    #[serde(default)]
    pub mounts: Vec<MountInput>,
    /// Logical secret mounts.
    #[serde(default)]
    pub secrets: Vec<SecretReferenceInput>,
    /// Optional container health check.
    pub healthcheck: Option<HealthCheckInput>,
    /// Optional resource limits.
    pub resources: Option<ResourceLimitsInput>,
}

fn default_replicas() -> u16 {
    1
}

/// Exactly one supported service source. The tag makes image/Git exhaustive.
/// User-declared HTTP route.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum SourceInput {
    /// Pull an image from a registry.
    Image {
        /// Image reference.
        image: String,
    },
    /// Build an image from a Git repository.
    Git {
        /// Repository URL.
        repository: String,
        /// Git reference to build.
        #[serde(default = "default_git_reference")]
        reference: String,
        /// Build context path.
        #[serde(default = "default_context")]
        context: String,
        /// Dockerfile path.
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

/// User-declared persistent volume.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct VolumeInput {
    /// Logical volume name.
    pub name: String,
}
/// A persistent volume mount in a service.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct MountInput {
    /// Referenced logical volume name.
    pub volume: String,
    /// Container target path.
    pub target: String,
    /// Whether the mount is read-only.
    #[serde(default)]
    pub read_only: bool,
}

/// User-declared logical secret mount.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SecretReferenceInput {
    /// Logical secret name.
    pub source: String,
    /// Optional container target path.
    pub target: Option<String>,
    /// File mode expressed as an octal string.
    #[serde(default = "default_secret_mode")]
    pub mode: String,
}
fn default_secret_mode() -> String {
    "0400".into()
}

/// User-declared HTTP route.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RouteInput {
    /// Hostname matched by the route.
    pub host: String,
    /// Logical service receiving the route.
    pub service: String,
    /// Container port receiving the route.
    pub port: u16,
}

/// User-declared container health check.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum HealthCheckInput {
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
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ResourceLimitsInput {
    /// CPU limit in millicores.
    pub cpu_millis: Option<u32>,
    /// Memory limit in bytes.
    #[schema(minimum = 1, maximum = 9_223_372_036_854_775_807_u64)]
    pub memory_bytes: Option<u64>,
}

/// A field-level, safe validation error.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ValidationError {
    /// Stable machine-readable validation code.
    pub code: String,
    /// Safe manifest field path.
    pub path: String,
    /// Safe human-readable validation message.
    pub message: String,
}

/// All independently discoverable manifest errors, in stable path order.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
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
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct NormalizedApplication {
    /// Stable application identity.
    pub id: ApplicationId,
    /// API version string.
    pub api_version: String,
    /// Resource kind string.
    pub kind: String,
    /// Canonical metadata.
    pub metadata: Metadata,
    /// Canonical resource specification.
    pub spec: ApplicationSpec,
}

/// Canonical application metadata.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Metadata {
    /// User-facing application name.
    pub name: String,
}

/// Canonical resource specification.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ApplicationSpec {
    /// Canonical services.
    pub services: Vec<Service>,
    /// Canonical volumes.
    pub volumes: Vec<Volume>,
    /// Canonical routes.
    pub routes: Vec<Route>,
}

/// Canonical service definition.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Service {
    /// Logical service name.
    pub name: String,
    /// Resolved source description.
    pub source: Source,
    /// Desired replica count.
    pub replicas: u16,
    /// Environment variables keyed by name.
    pub environment: BTreeMap<String, String>,
    /// Container entrypoint command.
    pub command: Vec<String>,
    /// Arguments passed to the command.
    pub arguments: Vec<String>,
    /// Published ports.
    pub ports: Vec<u16>,
    /// Persistent volume mounts.
    pub mounts: Vec<Mount>,
    /// Logical secret mounts.
    pub secrets: Vec<SecretReference>,
    /// Optional health check.
    pub healthcheck: Option<HealthCheck>,
    /// Optional resource limits.
    pub resources: Option<ResourceLimits>,
}

/// Canonical service source.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum Source {
    /// An image reference.
    Image {
        /// Immutable or requested image reference.
        image: String,
    },
    /// A resolved Git source.
    Git {
        /// Repository URL.
        repository: String,
        /// Resolved Git reference.
        reference: String,
        /// Build context path.
        context: String,
        /// Dockerfile path.
        dockerfile: String,
    },
}

/// Canonical persistent volume.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Volume {
    /// Logical volume name.
    pub name: String,
}
/// Canonical persistent volume mount.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Mount {
    /// Referenced volume name.
    pub volume: String,
    /// Container target path.
    pub target: String,
    /// Whether the mount is read-only.
    pub read_only: bool,
}
/// Canonical secret mount.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SecretReference {
    /// Logical secret name.
    pub source: String,
    /// Container target path.
    pub target: String,
    /// File mode expressed as an octal string.
    pub mode: String,
}
/// Canonical HTTP route.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Route {
    /// Hostname matched by the route.
    pub host: String,
    /// Logical service receiving the route.
    pub service: String,
    /// Container port receiving the route.
    pub port: u16,
}

/// Canonical service health check.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum HealthCheck {
    /// HTTP health endpoint check.
    Http {
        /// Container port to probe.
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
/// Canonical CPU and memory limits.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ResourceLimits {
    /// CPU limit in millicores.
    pub cpu_millis: Option<u32>,
    /// Memory limit in bytes.
    #[schema(minimum = 1, maximum = 9_223_372_036_854_775_807_u64)]
    pub memory_bytes: Option<u64>,
}

/// Parses and validates strict TOML without performing I/O.
///
/// # Errors
/// Returns validation errors, or one safe decode error for malformed input.
pub fn parse_toml(input: &str) -> Result<ValidatedApplication, ValidationErrors> {
    let manifest = serde_path_to_error::deserialize(toml::Deserializer::new(input))
        .map_err(|error| decode_error(error.path()))?;
    validate(manifest)
}

/// Parses and validates strict JSON without performing I/O.
///
/// # Errors
/// Returns validation errors, or one safe decode error for malformed input.
pub fn parse_json(input: &str) -> Result<ValidatedApplication, ValidationErrors> {
    let mut deserializer = serde_json::Deserializer::from_str(input);
    let manifest = serde_path_to_error::deserialize(&mut deserializer)
        .map_err(|error| decode_error(error.path()))?;
    validate(manifest)
}

fn decode_error(path: &serde_path_to_error::Path) -> ValidationErrors {
    let path = safe_decode_path(&path.to_string());
    ValidationErrors(vec![ValidationError {
        code: "manifest_decode_failed".into(),
        path,
        message: "manifest does not match the strict piqueld.dev/v1alpha1 Application schema"
            .into(),
    }])
}

fn safe_decode_path(path: &str) -> String {
    const FIELDS: &[&str] = &[
        "api_version",
        "kind",
        "metadata",
        "name",
        "spec",
        "services",
        "volumes",
        "routes",
        "source",
        "replicas",
        "environment",
        "command",
        "arguments",
        "ports",
        "mounts",
        "secrets",
        "healthcheck",
        "resources",
        "type",
        "image",
        "repository",
        "reference",
        "context",
        "dockerfile",
        "volume",
        "target",
        "read_only",
        "mode",
        "host",
        "service",
        "port",
        "path",
        "interval_seconds",
        "timeout_seconds",
        "cpu_millis",
        "memory_bytes",
    ];
    let mut safe = Vec::new();
    for component in path.split('.') {
        let field_end = component.find('[').unwrap_or(component.len());
        let (field, indices) = component.split_at(field_end);
        if !FIELDS.contains(&field) || !valid_path_indices(indices) {
            break;
        }
        safe.push(component);
    }
    if safe.is_empty() {
        "$".into()
    } else {
        safe.join(".")
    }
}

fn valid_path_indices(mut value: &str) -> bool {
    while !value.is_empty() {
        let Some(after_open) = value.strip_prefix('[') else {
            return false;
        };
        let Some(close) = after_open.find(']') else {
            return false;
        };
        if close == 0
            || !after_open[..close]
                .bytes()
                .all(|byte| byte.is_ascii_digit())
        {
            return false;
        }
        value = &after_open[close + 1..];
    }
    true
}

fn validate(input: ApplicationManifest) -> Result<ValidatedApplication, ValidationErrors> {
    let mut errors = Vec::new();
    validate_header(&input, &mut errors);
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
    validate_services(&input.spec.services, &volume_names, &mut errors);
    validate_volumes(&input.spec.volumes, &mut errors);
    validate_routes(
        &input.spec.routes,
        &input.spec.services,
        &service_names,
        &mut errors,
    );
    errors.sort_by(|a, b| a.path.cmp(&b.path).then(a.code.cmp(&b.code)));
    if !errors.is_empty() {
        return Err(ValidationErrors(errors));
    }

    Ok(ValidatedApplication {
        name: input.metadata.name,
        spec: convert_spec(input.spec),
    })
}

fn validate_header(input: &ApplicationManifest, errors: &mut Vec<ValidationError>) {
    if input.api_version != APPLICATION_API_VERSION {
        error(
            errors,
            "api_version_unsupported",
            "api_version",
            "unsupported application API version",
        );
    }
    if input.kind != APPLICATION_KIND {
        error(
            errors,
            "kind_unsupported",
            "kind",
            "resource kind must be Application",
        );
    }
    validate_name(&input.metadata.name, "metadata.name", errors);
    if input.spec.services.is_empty() {
        error(
            errors,
            "service_required",
            "spec.services",
            "application must declare at least one service",
        );
    }
}

fn validate_services(
    services: &[ServiceInput],
    volume_names: &BTreeSet<String>,
    errors: &mut Vec<ValidationError>,
) {
    for (index, service) in services.iter().enumerate() {
        validate_service(service, index, volume_names, errors);
    }
}

fn validate_service(
    service: &ServiceInput,
    index: usize,
    volume_names: &BTreeSet<String>,
    errors: &mut Vec<ValidationError>,
) {
    let base = format!("spec.services[{index}]");
    validate_name(&service.name, &format!("{base}.name"), errors);
    if !(1..=100).contains(&service.replicas) {
        error(
            errors,
            "replicas_out_of_range",
            &format!("{base}.replicas"),
            "replicas must be between 1 and 100",
        );
    }
    validate_source(&service.source, &base, errors);
    validate_environment(&service.environment, &base, errors);
    let mount_targets = validate_mounts(&service.mounts, &base, volume_names, errors);
    validate_secrets(&service.secrets, &base, &mount_targets, errors);
    validate_ports(&service.ports, &base, errors);
    validate_process_arguments(&service.command, &format!("{base}.command"), errors);
    if service
        .command
        .first()
        .is_some_and(|argument| argument.trim().is_empty())
    {
        error(
            errors,
            "process_command_invalid",
            &format!("{base}.command[0]"),
            "an explicit container command must start with a non-empty executable",
        );
    }
    validate_process_arguments(&service.arguments, &format!("{base}.arguments"), errors);
    validate_healthcheck(service, &base, errors);
    validate_resources(service.resources.as_ref(), &base, errors);
}

fn validate_source(source: &SourceInput, base: &str, errors: &mut Vec<ValidationError>) {
    match source {
        SourceInput::Image { image } if !valid_image_reference(image) => {
            error(
                errors,
                "image_invalid",
                &format!("{base}.source.image"),
                "image must be a valid registry reference without credentials or a URL scheme",
            );
        }
        SourceInput::Git {
            repository,
            reference,
            context,
            dockerfile,
        } => {
            if !valid_git_repository(repository) {
                error(
                    errors,
                    "git_repository_unsupported",
                    &format!("{base}.source.repository"),
                    "only HTTP(S) Git repositories are supported",
                );
            }
            if !valid_git_reference(reference) {
                error(
                    errors,
                    "git_reference_invalid",
                    &format!("{base}.source.reference"),
                    "Git reference must use safe Git ref syntax",
                );
            }
            validate_relative_path(context, &format!("{base}.source.context"), errors);
            validate_relative_path(dockerfile, &format!("{base}.source.dockerfile"), errors);
        }
        SourceInput::Image { .. } => {}
    }
}

fn validate_environment(
    environment: &BTreeMap<String, String>,
    base: &str,
    errors: &mut Vec<ValidationError>,
) {
    for (index, (key, value)) in environment.iter().enumerate() {
        if !valid_env_name(key) {
            error(
                errors,
                "environment_name_invalid",
                &format!("{base}.environment[{index}].name"),
                "environment names must use letters, digits, and underscores and cannot start with a digit",
            );
        }
        if value.contains('\0') {
            error(
                errors,
                "environment_value_invalid",
                &format!("{base}.environment[{index}].value"),
                "environment values cannot contain NUL",
            );
        }
    }
}

fn validate_mounts(
    mounts: &[MountInput],
    base: &str,
    volume_names: &BTreeSet<String>,
    errors: &mut Vec<ValidationError>,
) -> BTreeSet<String> {
    let mut targets = BTreeSet::new();
    for (index, mount) in mounts.iter().enumerate() {
        let path = format!("{base}.mounts[{index}]");
        if !volume_names.contains(&mount.volume) {
            error(
                errors,
                "mount_volume_missing",
                &format!("{path}.volume"),
                "mount references an undeclared volume",
            );
        }
        validate_absolute_path(
            &mount.target,
            &format!("{path}.target"),
            "mount_target_unsafe",
            "mount target must be an absolute, normalized container path below root",
            errors,
        );
        if !targets.insert(mount.target.clone()) {
            error(
                errors,
                "mount_target_duplicate",
                &format!("{path}.target"),
                "mount target is duplicated in this service",
            );
        }
    }
    targets
}

fn validate_secrets(
    secrets: &[SecretReferenceInput],
    base: &str,
    mount_targets: &BTreeSet<String>,
    errors: &mut Vec<ValidationError>,
) {
    let mut targets = BTreeSet::new();
    for (index, secret) in secrets.iter().enumerate() {
        let path = format!("{base}.secrets[{index}]");
        validate_name(&secret.source, &format!("{path}.source"), errors);
        let target = secret.target.as_deref().unwrap_or(&secret.source);
        validate_secret_target(target, &format!("{path}.target"), errors);
        let effective_target = effective_secret_target(target);
        if !targets.insert(effective_target.clone()) {
            error(
                errors,
                "secret_target_duplicate",
                &format!("{path}.target"),
                "secret target is duplicated in this service",
            );
        }
        if mount_targets.contains(&effective_target) {
            error(
                errors,
                "target_collision",
                &format!("{path}.target"),
                "secret target conflicts with a volume mount target in this service",
            );
        }
        if !valid_mode(&secret.mode) {
            error(
                errors,
                "secret_mode_invalid",
                &format!("{path}.mode"),
                "secret mode must be 0 plus three octal digits, grant read access, and grant no write access",
            );
        }
    }
}

fn validate_ports(ports: &[u16], base: &str, errors: &mut Vec<ValidationError>) {
    for (index, port) in ports.iter().enumerate() {
        if *port == 0 {
            error(
                errors,
                "port_invalid",
                &format!("{base}.ports[{index}]"),
                "port must be between 1 and 65535",
            );
        }
    }
}

fn validate_healthcheck(service: &ServiceInput, base: &str, errors: &mut Vec<ValidationError>) {
    let Some(health) = &service.healthcheck else {
        return;
    };
    validate_health(health, &format!("{base}.healthcheck"), errors);
    if let HealthCheckInput::Http { port, .. } = health
        && !service.ports.is_empty()
        && !service.ports.contains(port)
    {
        error(
            errors,
            "healthcheck_port_missing",
            &format!("{base}.healthcheck.port"),
            "health-check port is not declared by its service",
        );
    }
}

fn validate_resources(
    resources: Option<&ResourceLimitsInput>,
    base: &str,
    errors: &mut Vec<ValidationError>,
) {
    let Some(resources) = resources else { return };
    if resources.cpu_millis.is_none() && resources.memory_bytes.is_none() {
        error(
            errors,
            "resource_limits_empty",
            &format!("{base}.resources"),
            "resource limits must configure CPU, memory, or both",
        );
    }
    if resources.cpu_millis == Some(0) {
        error(
            errors,
            "cpu_limit_invalid",
            &format!("{base}.resources.cpu_millis"),
            "CPU limit must be greater than zero",
        );
    }
    if resources.memory_bytes == Some(0) {
        error(
            errors,
            "memory_limit_invalid",
            &format!("{base}.resources.memory_bytes"),
            "memory limit must be greater than zero",
        );
    }
    if resources
        .memory_bytes
        .is_some_and(|bytes| i64::try_from(bytes).is_err())
    {
        error(
            errors,
            "memory_limit_invalid",
            &format!("{base}.resources.memory_bytes"),
            "memory limit exceeds the maximum supported runtime value",
        );
    }
}

fn validate_volumes(volumes: &[VolumeInput], errors: &mut Vec<ValidationError>) {
    for (index, volume) in volumes.iter().enumerate() {
        validate_name(&volume.name, &format!("spec.volumes[{index}].name"), errors);
    }
}

fn validate_routes(
    routes: &[RouteInput],
    services: &[ServiceInput],
    service_names: &BTreeSet<String>,
    errors: &mut Vec<ValidationError>,
) {
    let mut route_hosts = BTreeSet::new();
    let mut route_keys = BTreeSet::new();
    for (index, route) in routes.iter().enumerate() {
        let base = format!("spec.routes[{index}]");
        validate_hostname(&route.host, &format!("{base}.host"), errors);
        if !service_names.contains(&route.service) {
            error(
                errors,
                "route_service_missing",
                &format!("{base}.service"),
                "route references an undeclared service",
            );
        }
        if let Some(service) = services
            .iter()
            .find(|service| service.name == route.service)
            && !service.ports.is_empty()
            && !service.ports.contains(&route.port)
        {
            error(
                errors,
                "route_port_missing",
                &format!("{base}.port"),
                "route port is not declared by its service",
            );
        }
        if route.port == 0 {
            error(
                errors,
                "port_invalid",
                &format!("{base}.port"),
                "port must be between 1 and 65535",
            );
        }
        let host = route.host.to_ascii_lowercase();
        let duplicate = !route_keys.insert((host.clone(), route.service.clone(), route.port));
        if duplicate {
            error(errors, "route_duplicate", &base, "route is duplicated");
        }
        if !route_hosts.insert(host) && !duplicate {
            error(
                errors,
                "public_route_conflict",
                &format!("{base}.host"),
                "only one public route may own a hostname",
            );
        }
    }
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
        let mut spec = self.spec.clone();
        normalize_spec(&mut spec);
        let bytes = serde_json::to_vec(&Envelope {
            hash_version: "piqueld-spec-hash/v1",
            metadata: &self.metadata,
            spec: &spec,
        })
        .expect("domain serialization is infallible");
        format!("sha256:{:x}", Sha256::digest(bytes))
    }

    /// Canonical JSON representation used for durable desired state.
    ///
    /// # Errors
    /// Propagates a JSON serialization error.
    pub fn canonical_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&self.clone().normalize())
    }

    /// Portable desired TOML. Internal identity is intentionally omitted and only
    /// logical secret references can be represented by these types.
    ///
    /// # Errors
    /// Propagates a TOML serialization error.
    pub fn export_toml(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(&self.clone().normalize().to_manifest())
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

pub(crate) fn valid_image_reference(value: &str) -> bool {
    if value.is_empty()
        || value.len() > 512
        || value
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
        || value.contains("//")
        || value.contains(['?', '#'])
    {
        return false;
    }

    let mut digest_parts = value.split('@');
    let name_and_tag = digest_parts.next().unwrap_or_default();
    if let Some(digest) = digest_parts.next()
        && (digest_parts.next().is_some() || !valid_image_digest(digest))
    {
        return false;
    }

    let last_slash = name_and_tag.rfind('/');
    let tag_separator = name_and_tag
        .rfind(':')
        .filter(|index| last_slash.is_none_or(|slash_index| *index > slash_index));
    let (name, tag) = tag_separator.map_or((name_and_tag, None), |index| {
        (&name_and_tag[..index], Some(&name_and_tag[index + 1..]))
    });
    if tag.is_some_and(|tag| {
        tag.is_empty()
            || tag.len() > 128
            || !tag
                .bytes()
                .next()
                .is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            || !tag
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
    }) {
        return false;
    }
    if name.is_empty() || name.len() > 255 {
        return false;
    }

    let components = name.split('/').collect::<Vec<_>>();
    if components.iter().any(|component| component.is_empty()) {
        return false;
    }
    let first_is_registry = components.len() > 1
        && (components[0].contains(['.', ':']) || components[0] == "localhost");
    let repository_components = if first_is_registry {
        if !valid_registry_authority(components[0]) {
            return false;
        }
        &components[1..]
    } else {
        components.as_slice()
    };
    repository_components
        .iter()
        .all(|component| valid_repository_component(component))
}

fn valid_registry_authority(value: &str) -> bool {
    let (host, port) = value
        .rsplit_once(':')
        .map_or((value, None), |(host, port)| (host, Some(port)));
    if port.is_some_and(|port| port.parse::<u16>().map_or(true, |port| port == 0)) {
        return false;
    }
    host == "localhost"
        || (!host.is_empty()
            && host.split('.').all(|label| {
                !label.is_empty()
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label.bytes().all(|byte| {
                        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
                    })
            }))
}

fn valid_repository_component(value: &str) -> bool {
    let bytes = value.as_bytes();
    if !bytes
        .first()
        .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        || !bytes
            .last()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        return false;
    }
    let mut index = 0;
    while index < bytes.len() {
        while index < bytes.len()
            && (bytes[index].is_ascii_lowercase() || bytes[index].is_ascii_digit())
        {
            index += 1;
        }
        if index == bytes.len() {
            return true;
        }
        let separator_start = index;
        while index < bytes.len() && matches!(bytes[index], b'.' | b'_' | b'-') {
            index += 1;
        }
        let separator = &value[separator_start..index];
        if index == bytes.len()
            || !(separator == "."
                || separator == "_"
                || separator == "__"
                || separator.bytes().all(|byte| byte == b'-'))
        {
            return false;
        }
    }
    true
}

fn valid_image_digest(value: &str) -> bool {
    let Some((algorithm, encoded)) = value.split_once(':') else {
        return false;
    };
    let algorithm_parts = algorithm.split(['_', '+', '.', '-']).collect::<Vec<_>>();
    algorithm_parts.iter().all(|part| {
        part.bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase())
            && part
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    }) && encoded.len() >= 32
        && encoded
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'=' | b'_' | b'-'))
}

fn valid_git_repository(value: &str) -> bool {
    if value.len() > 2048
        || value
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
    {
        return false;
    }
    Url::parse(value).is_ok_and(|url| {
        matches!(url.scheme(), "http" | "https")
            && url.host().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.port() != Some(0)
            && url.query().is_none()
            && url.fragment().is_none()
    })
}

fn valid_git_reference(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 1024
        && value != "@"
        && !value.starts_with('/')
        && !value.ends_with(['/', '.'])
        && !value.contains("..")
        && !value.contains("@{")
        && !value.contains("//")
        && !value.chars().any(|character| {
            character.is_whitespace()
                || character.is_control()
                || matches!(character, '~' | '^' | ':' | '?' | '*' | '[' | '\\')
        })
        && value.split('/').all(|component| {
            !component.starts_with('.') && component.strip_suffix(".lock").is_none()
        })
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
    let components = value.split('/').collect::<Vec<_>>();
    let normalized = value == "."
        || (!value.is_empty()
            && !value.starts_with('/')
            && !value.ends_with('/')
            && value.len() <= 4096
            && components.iter().all(|component| {
                !component.is_empty()
                    && component.len() <= 255
                    && *component != "."
                    && *component != ".."
            }));
    if !normalized || value.contains('\\') || value.chars().any(char::is_control) {
        error(
            errors,
            "source_path_unsafe",
            path,
            "source paths must be relative and cannot traverse parent directories",
        );
    }
}
fn validate_absolute_path(
    value: &str,
    path: &str,
    code: &str,
    message: &str,
    errors: &mut Vec<ValidationError>,
) {
    let components = value.split('/').skip(1);
    if !value.starts_with('/')
        || value == "/"
        || value.ends_with('/')
        || value.len() > 4096
        || value.contains('\\')
        || value.chars().any(char::is_control)
        || components
            .into_iter()
            .any(|part| part.is_empty() || part.len() > 255 || part == ".." || part == ".")
    {
        error(errors, code, path, message);
    }
}
fn validate_secret_target(value: &str, path: &str, errors: &mut Vec<ValidationError>) {
    if value.starts_with('/') {
        validate_absolute_path(
            value,
            path,
            "secret_target_unsafe",
            "secret target must be a safe file name or absolute normalized container path",
            errors,
        );
    } else if value.is_empty()
        || value.contains('/')
        || value.contains('\\')
        || value == "."
        || value == ".."
        || value.len() > 255
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        error(
            errors,
            "secret_target_unsafe",
            path,
            "secret target must be a safe file name or absolute normalized container path",
        );
    }
}

fn effective_secret_target(value: &str) -> String {
    if value.starts_with('/') {
        value.to_owned()
    } else {
        format!("/run/secrets/{value}")
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
        && value.starts_with('0')
        && value.bytes().all(|b| matches!(b, b'0'..=b'7'))
        && u16::from_str_radix(value, 8).is_ok_and(|mode| mode & 0o222 == 0 && mode & 0o444 != 0)
}

fn validate_process_arguments(values: &[String], path: &str, errors: &mut Vec<ValidationError>) {
    for (index, value) in values.iter().enumerate() {
        if value.contains('\0') {
            error(
                errors,
                "process_argument_invalid",
                &format!("{path}[{index}]"),
                "container process arguments cannot contain NUL",
            );
        }
    }
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
            let normalized_path = request_path == "/"
                || (request_path.starts_with('/')
                    && !request_path.ends_with('/')
                    && request_path.split('/').skip(1).all(|component| {
                        !component.is_empty() && component != "." && component != ".."
                    }));
            if request_path.len() > 2048
                || !normalized_path
                || request_path.contains(['\\', '?', '#'])
                || request_path
                    .chars()
                    .any(|character| character.is_whitespace() || character.is_control())
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
            if command
                .first()
                .is_none_or(|argument| argument.trim().is_empty())
                || command.iter().any(|v| v.contains('\0'))
            {
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
