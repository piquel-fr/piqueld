//! Public application manifests and their validated, canonical domain model.

use crate::ApplicationId;
use crate::codes;
use crate::resource::valid_logical_name;
use serde::{Deserialize, Serialize};
use serde_path_to_error::Path;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};
use utoipa::ToSchema;

/// The only application API version supported by the Plan 06A product.
pub const APPLICATION_API_VERSION: &str = "piqueld.dev/v1alpha1";
/// The only resource kind supported by the Plan 06A product.
pub const APPLICATION_KIND: &str = "Application";
/// Envelope version for the specification hash. Version 2 hashes only the
/// canonical spec, so cosmetic metadata changes no longer redeploy services.
pub const SPEC_HASH_VERSION: &str = "piqueld-spec-hash/v2";

const MAX_SERVICES: usize = 64;
const MAX_VOLUMES: usize = 64;
const MAX_ENVIRONMENT_ENTRIES: usize = 256;
const MAX_ENVIRONMENT_VALUE_BYTES: usize = 65_536;
const MAX_PROCESS_ELEMENTS: usize = 128;
const MAX_PROCESS_ELEMENT_BYTES: usize = 4_096;
const MAX_MOUNTS_PER_SERVICE: usize = 32;
const MAX_HEALTHCHECK_INTERVAL_SECONDS: u32 = 3_600;
const MAX_CPU_MILLIS: u32 = 1_048_576;

/// Strict public application manifest request and export shape.
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
    /// Declared named volumes.
    pub volumes: Vec<VolumeInput>,
}

/// User-declared application service.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ServiceInput {
    /// Logical service name.
    pub name: String,
    /// Prebuilt container image source.
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
    /// Persistent volume mounts.
    #[serde(default)]
    pub mounts: Vec<MountInput>,
    /// Optional container health check.
    pub healthcheck: Option<HealthCheckInput>,
    /// Optional CPU and memory limits.
    pub resources: Option<ResourceLimitsInput>,
}

fn default_replicas() -> u16 {
    1
}

/// The exhaustive set of deployable service sources.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum SourceInput {
    /// Pull a prebuilt image from a registry.
    Image {
        /// Image reference.
        image: String,
    },
}

/// User-declared named volume.
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
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "application manifest has {} error(s)",
            self.0.len()
        )?;
        for error in &self.0 {
            write!(
                formatter,
                "; {} at {}: {}",
                error.code, error.path, error.message
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for ValidationErrors {}

/// Validated domain application before canonical collection ordering.
#[derive(Clone, Debug, PartialEq)]
pub struct ValidatedApplication {
    name: String,
    spec: ApplicationSpec,
}

/// Canonical desired application plus persistence-assigned identity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
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
pub struct Metadata {
    /// User-facing application name.
    pub name: String,
}

/// Canonical resource specification.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct ApplicationSpec {
    /// Canonical services.
    pub services: Vec<Service>,
    /// Canonical named volumes.
    pub volumes: Vec<Volume>,
}

/// Canonical service definition.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct Service {
    /// Logical service name.
    pub name: String,
    /// Prebuilt image source.
    pub source: Source,
    /// Desired replica count.
    pub replicas: u16,
    /// Environment variables keyed by name.
    pub environment: BTreeMap<String, String>,
    /// Container entrypoint command.
    pub command: Vec<String>,
    /// Arguments passed to the command.
    pub arguments: Vec<String>,
    /// Persistent volume mounts.
    pub mounts: Vec<Mount>,
    /// Optional health check.
    pub healthcheck: Option<HealthCheck>,
    /// Optional CPU and memory limits.
    pub resources: Option<ResourceLimits>,
}

/// Canonical prebuilt image source.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Source {
    /// An image reference resolved by the Docker adapter before persistence.
    Image {
        /// Image reference requested by the user.
        image: String,
    },
}

/// Canonical named volume.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
pub struct Volume {
    /// Logical volume name.
    pub name: String,
}

/// Canonical persistent volume mount.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
pub struct Mount {
    /// Referenced logical volume name.
    pub volume: String,
    /// Container target path.
    pub target: String,
    /// Whether the mount is read-only.
    pub read_only: bool,
}

/// Canonical service health check.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "lowercase")]
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
pub struct ResourceLimits {
    /// CPU limit in millicores.
    pub cpu_millis: Option<u32>,
    /// Memory limit in bytes.
    // Keep in sync with the identical schema attribute on ResourceLimitsInput.
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
    deserializer.end().map_err(|_| {
        ValidationErrors(vec![ValidationError {
            code: codes::MANIFEST_DECODE_FAILED.into(),
            path: "$".into(),
            message: "manifest contains trailing JSON data".into(),
        }])
    })?;
    validate(manifest)
}

fn decode_error(path: &Path) -> ValidationErrors {
    ValidationErrors(vec![ValidationError {
        code: codes::MANIFEST_DECODE_FAILED.into(),
        path: safe_decode_path(&path.to_string()),
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
        "source",
        "replicas",
        "environment",
        "command",
        "arguments",
        "mounts",
        "healthcheck",
        "resources",
        "type",
        "image",
        "volume",
        "target",
        "read_only",
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
    validate_budgets(&input, &mut errors);
    unique_names(
        input.spec.services.iter().map(|service| &service.name),
        "spec.services",
        codes::SERVICE_NAME_DUPLICATE,
        &mut errors,
    );
    let volume_names = unique_names(
        input.spec.volumes.iter().map(|volume| &volume.name),
        "spec.volumes",
        codes::VOLUME_NAME_DUPLICATE,
        &mut errors,
    );
    validate_services(&input.spec.services, &volume_names, &mut errors);
    validate_volumes(&input.spec.volumes, &mut errors);
    errors.sort_by(|left, right| left.path.cmp(&right.path).then(left.code.cmp(&right.code)));
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
            codes::API_VERSION_UNSUPPORTED,
            "api_version",
            "unsupported application API version",
        );
    }
    if input.kind != APPLICATION_KIND {
        error(
            errors,
            codes::KIND_UNSUPPORTED,
            "kind",
            "resource kind must be Application",
        );
    }
    validate_name(&input.metadata.name, "metadata.name", errors);
    if input.spec.services.is_empty() {
        error(
            errors,
            codes::SERVICE_REQUIRED,
            "spec.services",
            "application must declare at least one service",
        );
    }
}

fn validate_budgets(input: &ApplicationManifest, errors: &mut Vec<ValidationError>) {
    if input.spec.services.len() > MAX_SERVICES {
        error(
            errors,
            codes::SERVICE_COUNT_EXCESSIVE,
            "spec.services",
            &format!("an application must declare at most {MAX_SERVICES} services"),
        );
    }
    if input.spec.volumes.len() > MAX_VOLUMES {
        error(
            errors,
            codes::VOLUME_COUNT_EXCESSIVE,
            "spec.volumes",
            &format!("an application must declare at most {MAX_VOLUMES} volumes"),
        );
    }
}

fn validate_services(
    services: &[ServiceInput],
    volume_names: &BTreeSet<String>,
    errors: &mut Vec<ValidationError>,
) {
    for (index, service) in services.iter().enumerate() {
        let base = format!("spec.services[{index}]");
        validate_name(&service.name, &format!("{base}.name"), errors);
        if !(1..=100).contains(&service.replicas) {
            error(
                errors,
                codes::REPLICAS_OUT_OF_RANGE,
                &format!("{base}.replicas"),
                "replicas must be between 1 and 100",
            );
        }
        if let SourceInput::Image { image } = &service.source
            && !valid_image_reference(image)
        {
            error(
                errors,
                codes::IMAGE_INVALID,
                &format!("{base}.source.image"),
                "image must be a valid registry reference with a lowercase hostname, without credentials or a URL scheme",
            );
        }
        validate_environment(&service.environment, &base, errors);
        validate_mounts(&service.mounts, &base, volume_names, errors);
        validate_process_arguments(
            &service.command,
            &format!("{base}.command"),
            codes::PROCESS_COMMAND_EXCESSIVE,
            errors,
        );
        validate_process_arguments(
            &service.arguments,
            &format!("{base}.arguments"),
            codes::PROCESS_ARGUMENTS_EXCESSIVE,
            errors,
        );
        if service
            .command
            .first()
            .is_some_and(|value| value.trim().is_empty())
        {
            error(
                errors,
                "process_command_invalid",
                &format!("{base}.command[0]"),
                "an explicit container command must start with a non-empty executable",
            );
        }
        if let Some(healthcheck) = &service.healthcheck {
            validate_health(healthcheck, &format!("{base}.healthcheck"), errors);
        }
        validate_resources(service.resources.as_ref(), &base, errors);
    }
}

fn validate_environment(
    environment: &BTreeMap<String, String>,
    base: &str,
    errors: &mut Vec<ValidationError>,
) {
    if environment.len() > MAX_ENVIRONMENT_ENTRIES {
        error(
            errors,
            codes::ENVIRONMENT_COUNT_EXCESSIVE,
            &format!("{base}.environment"),
            &format!(
                "a service must declare at most {MAX_ENVIRONMENT_ENTRIES} environment entries"
            ),
        );
    }
    for (key, value) in environment {
        let key_echo = safe_key_echo(key);
        if !valid_env_name(key) {
            error(
                errors,
                codes::ENVIRONMENT_NAME_INVALID,
                &format!("{base}.environment.name"),
                &format!(
                    "environment key {key_echo} is invalid: names must use letters, digits, and underscores and cannot start with a digit"
                ),
            );
        }
        if value.contains('\0') {
            error(
                errors,
                codes::ENVIRONMENT_VALUE_INVALID,
                &format!("{base}.environment.value"),
                &format!("environment value for key {key_echo} cannot contain NUL"),
            );
        }
        if value.len() > MAX_ENVIRONMENT_VALUE_BYTES {
            error(
                errors,
                codes::ENVIRONMENT_VALUE_EXCESSIVE,
                &format!("{base}.environment.value"),
                &format!(
                    "environment value for key {key_echo} must be at most {MAX_ENVIRONMENT_VALUE_BYTES} bytes"
                ),
            );
        }
    }
}

fn safe_key_echo(key: &str) -> String {
    let truncated: String = key.chars().take(64).collect();
    let sanitized = truncated
        .chars()
        .map(|character| {
            if character.is_control() {
                '\u{fffd}'
            } else {
                character
            }
        })
        .collect::<String>();
    format!("'{sanitized}'")
}

fn validate_mounts(
    mounts: &[MountInput],
    base: &str,
    volume_names: &BTreeSet<String>,
    errors: &mut Vec<ValidationError>,
) {
    let mut targets = BTreeSet::new();
    if mounts.len() > MAX_MOUNTS_PER_SERVICE {
        error(
            errors,
            codes::MOUNT_COUNT_EXCESSIVE,
            &format!("{base}.mounts"),
            &format!("a service must declare at most {MAX_MOUNTS_PER_SERVICE} mounts"),
        );
    }
    for (index, mount) in mounts.iter().enumerate() {
        let path = format!("{base}.mounts[{index}]");
        if !volume_names.contains(&mount.volume) {
            error(
                errors,
                codes::MOUNT_VOLUME_MISSING,
                &format!("{path}.volume"),
                "mount references an undeclared volume",
            );
        }
        validate_absolute_path(&mount.target, &format!("{path}.target"), errors);
        if !targets.insert(mount.target.clone()) {
            error(
                errors,
                codes::MOUNT_TARGET_DUPLICATE,
                &format!("{path}.target"),
                "mount target is duplicated in this service",
            );
        }
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
            codes::RESOURCE_LIMITS_EMPTY,
            &format!("{base}.resources"),
            "resource limits must configure CPU, memory, or both",
        );
    }
    if resources.cpu_millis == Some(0) {
        error(
            errors,
            codes::CPU_LIMIT_INVALID,
            &format!("{base}.resources.cpu_millis"),
            "CPU limit must be greater than zero",
        );
    }
    if resources
        .cpu_millis
        .is_some_and(|value| value > MAX_CPU_MILLIS)
    {
        error(
            errors,
            codes::CPU_LIMIT_EXCESSIVE,
            &format!("{base}.resources.cpu_millis"),
            &format!("CPU limit must be at most {MAX_CPU_MILLIS} millicores"),
        );
    }
    if resources.memory_bytes == Some(0)
        || resources
            .memory_bytes
            .is_some_and(|value| i64::try_from(value).is_err())
    {
        error(
            errors,
            codes::MEMORY_LIMIT_INVALID,
            &format!("{base}.resources.memory_bytes"),
            "memory limit must be greater than zero and fit the runtime value",
        );
    }
}

fn validate_volumes(volumes: &[VolumeInput], errors: &mut Vec<ValidationError>) {
    for (index, volume) in volumes.iter().enumerate() {
        validate_name(&volume.name, &format!("spec.volumes[{index}].name"), errors);
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
                    codes::PORT_INVALID,
                    &format!("{path}.port"),
                    "health-check port must be between 1 and 65535",
                );
            }
            let valid_path = request_path == "/"
                || (request_path.starts_with('/')
                    && !request_path.ends_with('/')
                    && request_path
                        .split('/')
                        .skip(1)
                        .all(|part| !part.is_empty() && part != "." && part != ".."));
            if request_path.len() > 2048
                || !valid_path
                || request_path.contains(['\\', '?', '#'])
                || request_path
                    .chars()
                    .any(|character| character.is_whitespace() || character.is_control())
            {
                error(
                    errors,
                    codes::HEALTHCHECK_PATH_INVALID,
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
            if command.first().is_none_or(|value| value.trim().is_empty())
                || command.iter().any(|value| value.contains('\0'))
            {
                error(
                    errors,
                    codes::HEALTHCHECK_COMMAND_INVALID,
                    &format!("{path}.command"),
                    "health-check command must contain at least one NUL-free argument",
                );
            }
            validate_process_arguments(
                command,
                &format!("{path}.command"),
                codes::PROCESS_COMMAND_EXCESSIVE,
                errors,
            );
            (*interval_seconds, *timeout_seconds)
        }
    };
    if interval == 0 {
        error(
            errors,
            codes::HEALTHCHECK_INTERVAL_INVALID,
            &format!("{path}.interval_seconds"),
            "health-check interval must be greater than zero",
        );
    }
    if interval > MAX_HEALTHCHECK_INTERVAL_SECONDS {
        error(
            errors,
            codes::HEALTHCHECK_INTERVAL_EXCESSIVE,
            &format!("{path}.interval_seconds"),
            &format!(
                "health-check interval must be at most {MAX_HEALTHCHECK_INTERVAL_SECONDS} seconds"
            ),
        );
    }
    if interval > 0 && (timeout == 0 || timeout > interval) {
        error(
            errors,
            codes::HEALTHCHECK_TIMEOUT_INVALID,
            &format!("{path}.timeout_seconds"),
            "health-check timeout must be greater than zero and no longer than its interval",
        );
    }
}

fn unique_names<'a>(
    names: impl Iterator<Item = &'a String>,
    path: &str,
    duplicate_code: &str,
    errors: &mut Vec<ValidationError>,
) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for (index, name) in names.enumerate() {
        if !found.insert(name.clone()) {
            error(
                errors,
                duplicate_code,
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
            .map(|service| Service {
                name: service.name,
                source: match service.source {
                    SourceInput::Image { image } => Source::Image {
                        image: canonicalize_image_reference(&image),
                    },
                },
                replicas: service.replicas,
                environment: service.environment,
                command: service.command,
                arguments: service.arguments,
                mounts: service
                    .mounts
                    .into_iter()
                    .map(|mount| Mount {
                        volume: mount.volume,
                        target: mount.target,
                        read_only: mount.read_only,
                    })
                    .collect(),
                healthcheck: service.healthcheck.map(|health| match health {
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
                resources: service.resources.map(|resources| ResourceLimits {
                    cpu_millis: resources.cpu_millis,
                    memory_bytes: resources.memory_bytes,
                }),
            })
            .collect(),
        volumes: input
            .volumes
            .into_iter()
            .map(|volume| Volume { name: volume.name })
            .collect(),
    }
}

impl ValidatedApplication {
    /// Returns the editable application name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the validated specification before canonical ordering.
    #[must_use]
    pub fn spec(&self) -> &ApplicationSpec {
        &self.spec
    }

    /// Canonicalizes unordered collections and attaches a stable ID.
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
}

fn normalize_spec(spec: &mut ApplicationSpec) {
    spec.services
        .sort_by(|left, right| left.name.cmp(&right.name));
    for service in &mut spec.services {
        service.mounts.sort();
    }
    spec.volumes.sort();
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
    /// The envelope covers only the canonical spec, so cosmetic metadata edits
    /// do not change the hash or redeploy services.
    #[must_use]
    ///
    /// # Panics
    ///
    /// Panics only if the internal normalized manifest cannot be serialized,
    /// which indicates a bug in the domain types.
    pub fn spec_hash(&self) -> String {
        #[derive(Serialize)]
        struct HashEnvelope<'a> {
            hash_version: &'static str,
            spec: &'a ApplicationSpec,
        }
        let normalized = self.clone().normalize();
        let bytes = serde_json::to_vec(&HashEnvelope {
            hash_version: SPEC_HASH_VERSION,
            spec: &normalized.spec,
        })
        .expect("domain serialization is infallible");
        format!("sha256:{:x}", Sha256::digest(bytes))
    }

    /// Canonical JSON representation used for durable desired state.
    ///
    /// # Errors
    ///
    /// Returns a serialization error if the normalized manifest cannot be
    /// represented as JSON.
    pub fn canonical_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&self.clone().normalize())
    }

    /// Portable desired TOML representation.
    ///
    /// # Errors
    ///
    /// Returns a serialization error if the normalized manifest cannot be
    /// represented as TOML.
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
                    .map(|service| ServiceInput {
                        name: service.name.clone(),
                        source: match &service.source {
                            Source::Image { image } => SourceInput::Image {
                                image: image.clone(),
                            },
                        },
                        replicas: service.replicas,
                        environment: service.environment.clone(),
                        command: service.command.clone(),
                        arguments: service.arguments.clone(),
                        mounts: service
                            .mounts
                            .iter()
                            .map(|mount| MountInput {
                                volume: mount.volume.clone(),
                                target: mount.target.clone(),
                                read_only: mount.read_only,
                            })
                            .collect(),
                        healthcheck: service.healthcheck.as_ref().map(|health| match health {
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
                        resources: service.resources.as_ref().map(|resources| {
                            ResourceLimitsInput {
                                cpu_millis: resources.cpu_millis,
                                memory_bytes: resources.memory_bytes,
                            }
                        }),
                    })
                    .collect(),
                volumes: self
                    .spec
                    .volumes
                    .iter()
                    .map(|volume| VolumeInput {
                        name: volume.name.clone(),
                    })
                    .collect(),
            },
        }
    }
}

fn validate_name(value: &str, path: &str, errors: &mut Vec<ValidationError>) {
    if !valid_logical_name(value) {
        error(
            errors,
            codes::NAME_INVALID,
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
        .filter(|index| last_slash.is_none_or(|slash| *index > slash));
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
    let lowered = host.to_ascii_lowercase();
    lowered == "localhost"
        || (!lowered.is_empty()
            && lowered.split('.').all(|label| {
                !label.is_empty()
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label.bytes().all(|byte| {
                        byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
                    })
            }))
}

fn first_registry_component(name: &str) -> Option<&str> {
    let components = name.split('/').collect::<Vec<_>>();
    let first_is_registry = components.len() > 1
        && (components[0].contains(['.', ':']) || components[0] == "localhost");
    first_is_registry.then(|| components[0])
}

fn canonicalize_image_reference(value: &str) -> String {
    if !valid_image_reference(value) {
        return value.to_owned();
    }
    let (name, digest) = value
        .split_once('@')
        .map_or((value, None), |(name, digest)| (name, Some(digest)));
    let Some(registry) = first_registry_component(name) else {
        return value.to_owned();
    };
    let lowered = registry.to_ascii_lowercase();
    if lowered == registry {
        return value.to_owned();
    }
    let normalized_name = format!("{lowered}{}", &name[registry.len()..]);
    match digest {
        Some(digest) => format!("{normalized_name}@{digest}"),
        None => normalized_name,
    }
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

fn validate_absolute_path(value: &str, path: &str, errors: &mut Vec<ValidationError>) {
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
        error(
            errors,
            "mount_target_unsafe",
            path,
            "mount target must be an absolute, normalized container path below root",
        );
    }
}

fn valid_env_name(value: &str) -> bool {
    !value.is_empty()
        && !value.as_bytes()[0].is_ascii_digit()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn validate_process_arguments(
    values: &[String],
    path: &str,
    excessive_code: &str,
    errors: &mut Vec<ValidationError>,
) {
    if values.len() > MAX_PROCESS_ELEMENTS {
        error(
            errors,
            excessive_code,
            path,
            &format!("the list must contain at most {MAX_PROCESS_ELEMENTS} elements"),
        );
    }
    for (index, value) in values.iter().enumerate() {
        if value.contains('\0') {
            error(
                errors,
                codes::PROCESS_ARGUMENT_INVALID,
                &format!("{path}[{index}]"),
                "container process arguments cannot contain NUL",
            );
        }
        if value.len() > MAX_PROCESS_ELEMENT_BYTES {
            error(
                errors,
                excessive_code,
                &format!("{path}[{index}]"),
                &format!("each element must be at most {MAX_PROCESS_ELEMENT_BYTES} bytes"),
            );
        }
    }
}

fn error(errors: &mut Vec<ValidationError>, code: &str, path: &str, message: &str) {
    errors.push(ValidationError {
        code: code.into(),
        path: path.into(),
        message: message.into(),
    });
}
