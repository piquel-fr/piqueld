//! Strict decoding and aggregate semantic validation of manifest inputs.

use super::dependencies::StartupOrder;
use super::{
    APPLICATION_API_VERSION, APPLICATION_KIND, ApplicationManifest, Build, GitRepository,
    HealthCheck, Mount, ResourceLimits, SecretDeclaration, SecretGenerator, Service, Source,
    ValidatedApplication, Volume,
};
use crate::{codes, resource::valid_logical_name};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
};
use utoipa::ToSchema;

// Limits bounding manifest size and validation work. Route, service, and
// volume counts are checked before any per-item validation and stop it early;
// the remaining limits are reported alongside per-item errors.
const MAX_ROUTES: usize = 64;
const MAX_SERVICES: usize = 64;
const MAX_VOLUMES: usize = 64;
const MAX_ENVIRONMENT_ENTRIES: usize = 256;
/// Environment keys share the common 255-byte identifier bound so one entry
/// cannot dominate a manifest or a container environment list.
const MAX_ENVIRONMENT_KEY_BYTES: usize = 255;
const MAX_ENVIRONMENT_VALUE_BYTES: usize = 65_536;
const MAX_PROCESS_ELEMENTS: usize = 128;
const MAX_PROCESS_ELEMENT_BYTES: usize = 4_096;
const MAX_MOUNTS_PER_SERVICE: usize = 32;
const MAX_HEALTHCHECK_INTERVAL_SECONDS: u32 = 3_600;
const MAX_CPU_MILLIS: u32 = 1_048_576;
const MAX_GENERATED_SECRETS: usize = 64;
/// Accepted raw byte counts for random generated secrets, before encoding.
const RANDOM_SECRET_BYTES: std::ops::RangeInclusive<u16> = 16..=512;
/// Accepted RSA modulus sizes for generated private keys.
const RSA_SECRET_BITS: [u16; 3] = [2048, 3072, 4096];

impl GitRepository {
    /// Validates Git arguments without executing Git.
    ///
    /// Appends errors at `path` for inline URL credentials (any `http(s)`
    /// userinfo, or `user:password@` in other schemes), empty or option-like
    /// URLs, branch names failing `git check-ref-format` style rules, and commits
    /// that are not full lowercase hashes. Leading `-` is rejected so values
    /// cannot be parsed as Git options.
    pub fn validate(&self, path: &str, errors: &mut Vec<ValidationError>) {
        let inline_credentials = self.url.split_once("://").is_some_and(|(scheme, rest)| {
            let authority = rest.split(['/', '?', '#', '\\']).next().unwrap_or_default();
            authority.rsplit_once('@').is_some_and(|(userinfo, _)| {
                scheme.eq_ignore_ascii_case("http")
                    || scheme.eq_ignore_ascii_case("https")
                    || userinfo.contains(':')
            })
        });
        if inline_credentials {
            error(
                errors,
                "git_credentials_inline",
                path,
                "configure Git credentials on the host, not in repository URLs",
            );
        }
        if self.url.is_empty()
            || self.url.len() > 4096
            || self.url.starts_with('-')
            || self.url.chars().any(char::is_control)
        {
            error(
                errors,
                "git_repository_invalid",
                path,
                "repository URL must be a nonempty Git location",
            );
        }
        if self.branch.is_empty()
            || self.branch.len() > 255
            || self.branch == "@"
            || self.branch.starts_with('-')
            || self.branch.ends_with(['/', '.'])
            || self.branch.contains("..")
            || self.branch.contains("@{")
            || self.branch.contains("//")
            || self.branch.split('/').any(|component| {
                component.starts_with('.') || component.strip_suffix(".lock").is_some()
            })
            || self
                .branch
                .chars()
                .any(|c| c.is_whitespace() || c.is_control() || "~^:?*[\\".contains(c))
        {
            error(
                errors,
                "git_branch_invalid",
                path,
                "branch must be a valid Git branch name",
            );
        }
        if self
            .commit
            .as_ref()
            .is_some_and(|value| !valid_git_commit(value))
        {
            error(
                errors,
                "git_commit_invalid",
                path,
                "commit must be a full lowercase hexadecimal Git hash",
            );
        }
    }
}

/// Whether a value is a full Git object hash.
#[must_use]
pub fn valid_git_commit(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Whether a path stays lexically inside a checkout. Symlinks are checked at runtime.
///
/// ```text
/// "Dockerfile", "docker/app.Dockerfile", "."  -> valid
/// "/etc/passwd", "../x", "a/.git/config"     -> invalid
/// ```
#[must_use]
pub fn valid_repository_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 4096
        && !value.chars().any(char::is_control)
        && !value.contains('\\')
        && !value.contains(':')
        && !value.starts_with('/')
        && value.split('/').all(|part| part != ".." && part != ".git")
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

/// Parses and validates strict TOML without performing I/O.
///
/// # Errors
/// Returns validation errors, or one decode error naming the rejected input
/// and its location.
pub fn parse_toml(input: &str) -> Result<ValidatedApplication, ValidationErrors> {
    let manifest =
        serde_path_to_error::deserialize(toml::Deserializer::new(input)).map_err(|error| {
            ValidationErrors::decode(
                &error.path().to_string(),
                crate::TomlDiagnostic::new(input, error.inner()).to_string(),
            )
        })?;
    ApplicationManifest::validate(manifest)
}

/// Parses and validates strict JSON without performing I/O.
///
/// # Errors
/// Returns validation errors, or one decode error naming the rejected input
/// and its location.
pub fn parse_json(input: &str) -> Result<ValidatedApplication, ValidationErrors> {
    let mut deserializer = serde_json::Deserializer::from_str(input);
    let manifest = serde_path_to_error::deserialize(&mut deserializer).map_err(|error| {
        ValidationErrors::decode(&error.path().to_string(), error.inner().to_string())
    })?;
    deserializer
        .end()
        .map_err(|error| ValidationErrors::decode("$", error.to_string()))?;
    ApplicationManifest::validate(manifest)
}

impl ValidationErrors {
    /// One decode failure. Serde messages carry the rejected field and
    /// location, and may echo a mistyped value back to its submitter.
    /// Manifests reference secrets by name only, so they are safe to return.
    fn decode(path: &str, message: String) -> Self {
        Self(vec![ValidationError {
            code: codes::MANIFEST_DECODE_FAILED.into(),
            path: safe_decode_path(path),
            message,
        }])
    }
}

/// Redacts map keys and unknown components from a serde decode path.
///
/// Keeps the longest prefix made of known manifest field names with numeric
/// indices, so user-supplied map keys (e.g. environment names) never leak.
///
/// ```text
/// "spec.services[0].replicas"         -> "spec.services[0].replicas"
/// "spec.services[0].environment.KEY"  -> "spec.services[0].environment"
/// "bogus.field"                       -> "$"
/// ```
#[must_use]
pub fn safe_decode_path(path: &str) -> String {
    const FIELDS: &[&str] = &[
        "api_version",
        "kind",
        "metadata",
        "name",
        "spec",
        "manifest",
        "services",
        "volumes",
        "source",
        "replicas",
        "environment",
        "command",
        "arguments",
        "mounts",
        "secrets",
        "healthcheck",
        "resources",
        "type",
        "image",
        "repository",
        "url",
        "branch",
        "commit",
        "build",
        "dockerfile",
        "context",
        "volume",
        "target",
        "read_only",
        "port",
        "routes",
        "hostname",
        "service",
        "path",
        "interval_seconds",
        "timeout_seconds",
        "cpu_millis",
        "memory_bytes",
        "generate",
        "bytes",
        "encoding",
        "bits",
        "depends_on",
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

/// Whether `value` is zero or more `[<digits>]` groups, e.g. `""` or `"[0][12]"`.
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

impl ApplicationManifest {
    /// Canonicalizes route hostnames in place (lowercase, trailing dot removed),
    /// then checks hostname syntax, uniqueness, the target service, and the port.
    fn validate_routes(&mut self, errors: &mut Vec<ValidationError>) {
        let mut hostnames = BTreeSet::new();
        for (index, route) in self.spec.routes.iter_mut().enumerate() {
            route.hostname = route
                .hostname
                .strip_suffix('.')
                .unwrap_or(&route.hostname)
                .to_ascii_lowercase();
            let path = format!("spec.routes[{index}]");
            if super::Hostname::parse(&route.hostname).is_err() {
                error(
                    errors,
                    "route_hostname_invalid",
                    &format!("{path}.hostname"),
                    "an exact public ASCII DNS hostname is required",
                );
            }
            if !hostnames.insert(route.hostname.clone()) {
                error(
                    errors,
                    "route_hostname_duplicate",
                    &format!("{path}.hostname"),
                    "hostname is already used in this application",
                );
            }
            if !self
                .spec
                .services
                .iter()
                .any(|service| service.name == route.service)
            {
                error(
                    errors,
                    "route_service_missing",
                    &format!("{path}.service"),
                    "route must reference a service in this application",
                );
            }
            if route.port == 0 {
                error(
                    errors,
                    "route_port_invalid",
                    &format!("{path}.port"),
                    "HTTP backend port must be 1..=65535",
                );
            }
        }
    }

    /// Validates manifest semantics and returns a normalized-input wrapper.
    ///
    /// Collects every independent error rather than stopping at the first:
    /// 1. Header, metadata, and optional repository manifest source.
    /// 2. Route, service, and volume budgets; exceeding one returns early to
    ///    bound work.
    /// 3. Routes, duplicate names, services, and volumes.
    /// 4. On success, canonicalizes image registries and converts to domain types.
    ///
    /// Errors are sorted by path then code.
    ///
    /// # Errors
    /// Returns all detected manifest validation errors.
    pub fn validate(mut self) -> Result<ValidatedApplication, ValidationErrors> {
        let mut errors = Vec::new();
        validate_header(&self, &mut errors);
        if let Some(manifest) = &self.spec.manifest {
            manifest
                .repository
                .validate("spec.manifest.repository", &mut errors);
            if !valid_repository_path(&manifest.path) {
                error(
                    &mut errors,
                    "repository_path_invalid",
                    "spec.manifest.path",
                    "manifest path must remain within the repository root",
                );
            }
        }
        // Bound work before walking attacker-controlled collections.
        if !validate_budgets(&self, &mut errors) {
            errors
                .sort_by(|left, right| left.path.cmp(&right.path).then(left.code.cmp(&right.code)));
            return Err(ValidationErrors(errors));
        }
        self.validate_routes(&mut errors);
        unique_names(
            self.spec.services.iter().map(|service| &service.name),
            "spec.services",
            codes::SERVICE_NAME_DUPLICATE,
            &mut errors,
        );
        let volume_names = unique_names(
            self.spec.volumes.iter().map(|volume| &volume.name),
            "spec.volumes",
            codes::VOLUME_NAME_DUPLICATE,
            &mut errors,
        );
        validate_services(&self.spec.services, &volume_names, &mut errors);
        validate_dependencies(&self.spec.services, &mut errors);
        validate_volumes(&self.spec.volumes, &mut errors);
        validate_generated_secrets(&self.spec.secrets, &mut errors);
        errors.sort_by(|left, right| left.path.cmp(&right.path).then(left.code.cmp(&right.code)));
        if !errors.is_empty() {
            return Err(ValidationErrors(errors));
        }
        for service in &mut self.spec.services {
            if let Source::Image { image } = &mut service.source {
                *image = canonicalize_image_reference(image);
            }
        }
        Ok(ValidatedApplication {
            metadata: super::domain::ValidatedMetadata::from_input(self.metadata)?,
            spec: super::domain::ValidatedSpec::from_input(self.spec)?,
        })
    }
}

/// Checks the API version, kind, and application name.
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
}

/// Checks route, service, and volume counts; returns `false` when a budget is
/// exceeded.
fn validate_budgets(input: &ApplicationManifest, errors: &mut Vec<ValidationError>) -> bool {
    let mut within_budget = true;
    if input.spec.routes.len() > MAX_ROUTES {
        error(
            errors,
            "routes_limit",
            "spec.routes",
            &format!("at most {MAX_ROUTES} routes are allowed per application"),
        );
        within_budget = false;
    }
    if input.spec.services.len() > MAX_SERVICES {
        error(
            errors,
            codes::SERVICE_COUNT_EXCESSIVE,
            "spec.services",
            &format!("an application must declare at most {MAX_SERVICES} services"),
        );
        within_budget = false;
    }
    if input.spec.volumes.len() > MAX_VOLUMES {
        error(
            errors,
            codes::VOLUME_COUNT_EXCESSIVE,
            "spec.volumes",
            &format!("an application must declare at most {MAX_VOLUMES} volumes"),
        );
        within_budget = false;
    }
    if input.spec.secrets.len() > MAX_GENERATED_SECRETS {
        error(
            errors,
            "secrets_excessive",
            "spec.secrets",
            &format!("an application must declare at most {MAX_GENERATED_SECRETS} secrets"),
        );
        within_budget = false;
    }
    within_budget
}

/// Validates every service: name, replicas, source, secrets, environment,
/// mounts, process arguments, health check, and resource limits.
///
/// `volume_names` holds declared volumes for mount references. Secret targets
/// must be unique under `/run/secrets/` and must not collide with mount targets.
fn validate_services(
    services: &[Service],
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
        if let Source::Image { image } = &service.source
            && !valid_image_reference(image)
        {
            error(
                errors,
                codes::IMAGE_INVALID,
                &format!("{base}.source.image"),
                "image must be a valid registry reference without credentials or a URL scheme",
            );
        }
        if let Source::Git {
            repository,
            build:
                Build::Docker {
                    dockerfile,
                    context,
                },
        } = &service.source
        {
            repository.validate(&format!("{base}.source.repository"), errors);
            for (field, value) in [("dockerfile", dockerfile), ("context", context)] {
                if !valid_repository_path(value) {
                    error(
                        errors,
                        "repository_path_invalid",
                        &format!("{base}.source.build.{field}"),
                        "path must be relative to the repository root and remain within it",
                    );
                }
            }
        }
        let mut targets = service
            .mounts
            .iter()
            .map(|m| m.target.as_str())
            .collect::<BTreeSet<_>>();
        for (index, secret) in service.secrets.iter().enumerate() {
            let path = format!("{base}.secrets[{index}]");
            validate_name(&secret.name, &format!("{path}.name"), errors);
            validate_absolute_path(&secret.target, &format!("{path}.target"), errors);
            if !secret.target.starts_with("/run/secrets/") || !targets.insert(&secret.target) {
                error(
                    errors,
                    "secret_target_invalid",
                    &path,
                    "secret targets must be unique file paths under /run/secrets",
                );
            }
        }
        if service.secrets.len() > 64 {
            error(
                errors,
                "secrets_excessive",
                &base,
                "at most 64 secret file mounts per service",
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

/// Dependencies must name other services once each and must not form a cycle.
fn validate_dependencies(services: &[Service], errors: &mut Vec<ValidationError>) {
    let names = services
        .iter()
        .map(|service| service.name.as_str())
        .collect::<BTreeSet<_>>();
    for (index, service) in services.iter().enumerate() {
        let mut listed = BTreeSet::new();
        for (dependency_index, dependency) in service.depends_on.iter().enumerate() {
            let path = format!("spec.services[{index}].depends_on[{dependency_index}]");
            if !names.contains(dependency.as_str()) {
                error(
                    errors,
                    codes::SERVICE_DEPENDENCY_MISSING,
                    &path,
                    "dependency must name a service in this application",
                );
            } else if !listed.insert(dependency) {
                error(
                    errors,
                    codes::SERVICE_DEPENDENCY_DUPLICATE,
                    &path,
                    "dependency is listed more than once",
                );
            }
        }
    }
    let (_, cyclic) = services.startup_order();
    let cyclic = cyclic
        .into_iter()
        .map(|service| service.name.as_str())
        .collect::<BTreeSet<_>>();
    for (index, service) in services.iter().enumerate() {
        if cyclic.contains(service.name.as_str()) {
            error(
                errors,
                codes::SERVICE_DEPENDENCY_CYCLE,
                &format!("spec.services[{index}].depends_on"),
                "dependencies must not form or lead into a cycle",
            );
        }
    }
}

/// Checks environment entry count, key syntax and length, and value content
/// and size.
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
        if !valid_env_name(key) || key.len() > MAX_ENVIRONMENT_KEY_BYTES {
            error(
                errors,
                codes::ENVIRONMENT_NAME_INVALID,
                &format!("{base}.environment.name"),
                &format!(
                    "environment key {key_echo} is invalid: names must use letters, digits, and underscores, cannot start with a digit, and must be at most {MAX_ENVIRONMENT_KEY_BYTES} bytes"
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

/// Quotes a user-supplied key for error messages, truncated to 64 characters with
/// control characters replaced by `U+FFFD`.
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

/// Checks mount count, that each mount references a declared volume, and that
/// targets are safe and unique.
fn validate_mounts(
    mounts: &[Mount],
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

/// Checks optional limits: at least one limit set, CPU in `1..=MAX_CPU_MILLIS`,
/// and memory nonzero and representable as Docker's `i64`.
fn validate_resources(
    resources: Option<&ResourceLimits>,
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

/// Checks each declared volume name.
fn validate_volumes(volumes: &[Volume], errors: &mut Vec<ValidationError>) {
    for (index, volume) in volumes.iter().enumerate() {
        validate_name(&volume.name, &format!("spec.volumes[{index}].name"), errors);
    }
}

/// Checks generated secret declarations: unique valid names, random byte counts
/// in `RANDOM_SECRET_BYTES`, and RSA sizes in `RSA_SECRET_BITS`. Invalid
/// generator parameters are reported at `spec.secrets[i].generate.bytes` or
/// `.bits` with `secret_generator_invalid`.
fn validate_generated_secrets(secrets: &[SecretDeclaration], errors: &mut Vec<ValidationError>) {
    unique_names(
        secrets.iter().map(|secret| &secret.name),
        "spec.secrets",
        "secret_name_duplicate",
        errors,
    );
    for (index, secret) in secrets.iter().enumerate() {
        let path = format!("spec.secrets[{index}]");
        validate_name(&secret.name, &format!("{path}.name"), errors);
        match secret.generate {
            SecretGenerator::Random { bytes, .. } if !RANDOM_SECRET_BYTES.contains(&bytes) => {
                error(
                    errors,
                    "secret_generator_invalid",
                    &format!("{path}.generate.bytes"),
                    &format!(
                        "random secrets must use {}-{} bytes",
                        RANDOM_SECRET_BYTES.start(),
                        RANDOM_SECRET_BYTES.end()
                    ),
                );
            }
            SecretGenerator::Rsa { bits } if !RSA_SECRET_BITS.contains(&bits) => {
                error(
                    errors,
                    "secret_generator_invalid",
                    &format!("{path}.generate.bits"),
                    "RSA keys must use 2048, 3072, or 4096 bits",
                );
            }
            _ => {}
        }
    }
}

/// Validates a health check's probe, then its timing: interval in
/// `1..=MAX_HEALTHCHECK_INTERVAL_SECONDS` and timeout in `1..=interval`.
///
/// HTTP paths must be normalized absolute paths without query or fragment.
fn validate_health(value: &HealthCheck, path: &str, errors: &mut Vec<ValidationError>) {
    let (interval, timeout) = match value {
        HealthCheck::Http {
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
        HealthCheck::Command {
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

/// Reports duplicates at `{path}[index].name` and returns the set of distinct names.
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

/// Reports `NAME_INVALID` unless `value` is a safe logical resource name.
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

/// Whether a value follows Docker's image reference grammar, with extra hardening.
///
/// Accepts `[registry/]repository[:tag][@digest]` and rejects whitespace,
/// control characters, URL schemes (`//`), and `?`/`#`. A first component is a
/// registry when it contains `.` or `:`, or is `localhost`.
///
/// ```text
/// "nginx", "ghcr.io/org/app:v1", "localhost:5000/app@sha256:..."  -> valid
/// "https://ghcr.io/app", "Nginx", "app:", "a//b"                  -> invalid
/// ```
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
        && (components[0].contains(['.', ':']) || components[0].eq_ignore_ascii_case("localhost"));
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

/// Whether a registry authority is `localhost` or DNS-like labels, with an
/// optional nonzero port.
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

/// Returns the leading registry host of an image name, if it has one.
fn first_registry_component(name: &str) -> Option<&str> {
    let components = name.split('/').collect::<Vec<_>>();
    let first_is_registry = components.len() > 1
        && (components[0].contains(['.', ':']) || components[0].eq_ignore_ascii_case("localhost"));
    first_is_registry.then(|| components[0])
}

/// Lowercases the registry host of an image reference; other parts are case-sensitive.
/// Invalid references and references without a registry are returned unchanged.
///
/// ```text
/// "GHCR.io/org/app:v1" -> "ghcr.io/org/app:v1"
/// ```
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

/// Whether a repository path component is lowercase alphanumeric runs joined by
/// one of the Docker separators: `.`, `_`, `__`, or one or more `-`.
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
        if separator.is_empty()
            || index == bytes.len()
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

/// Whether a value matches the OCI digest grammar `algorithm:encoded`.
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

/// Reports a container path unless it is absolute, normalized (no `.`, `..`, or
/// empty components), not `/` itself, and free of backslashes and controls.
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

/// Whether an environment key is a POSIX-style identifier (`[A-Za-z_][A-Za-z0-9_]*`).
fn valid_env_name(value: &str) -> bool {
    !value.is_empty()
        && !value.as_bytes()[0].is_ascii_digit()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// Bounds a command or argument list by element count and size, and rejects NUL
/// bytes. `excessive_code` distinguishes commands from arguments.
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

/// Appends one validation error.
fn error(errors: &mut Vec<ValidationError>, code: &str, path: &str, message: &str) {
    errors.push(ValidationError {
        code: code.into(),
        path: path.into(),
        message: message.into(),
    });
}
