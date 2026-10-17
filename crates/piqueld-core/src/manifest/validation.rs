//! Strict decoding and aggregate semantic validation of manifest inputs.

use super::dependencies::StartupOrder;
use super::variables::{Template, Typed};
use super::{
    APPLICATION_API_VERSION, APPLICATION_KIND, ApplicationManifest, ApplicationSpec, Build,
    FileSource, GitRepository, HealthCheck, Job, JobVolumeAccess, ManifestRevision,
    RepositoryManifest, ResourceLimits, SecretGenerator, Service, ServiceFile, Source,
    SourceRepository, ValidatedApplication, ValidatedRollout, ValidatedTemplate,
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
const MAX_VARIABLE_ENTRIES: usize = 256;
/// Variable keys and build targets share the common 255-byte identifier bound so one entry
/// cannot dominate a manifest, a container environment list, or a build command.
const MAX_IDENTIFIER_BYTES: usize = 255;
const MAX_VARIABLE_VALUE_BYTES: usize = 65_536;
/// Build arguments are passed on the `docker build` command line, whose
/// arguments and environment share the kernel's ~2 MiB `ARG_MAX`.
const MAX_BUILD_ARG_TOTAL_BYTES: usize = 262_144;
const MAX_PROCESS_ELEMENTS: usize = 128;
const MAX_PROCESS_ELEMENT_BYTES: usize = 4_096;
const MAX_MOUNTS_PER_SERVICE: usize = 32;
const MAX_SECRET_MOUNTS_PER_SERVICE: usize = 64;
const MAX_HEALTHCHECK_INTERVAL_SECONDS: u32 = 3_600;
pub(super) const MAX_CPU_MILLIS: u32 = 1_048_576;
/// Accepted replica counts of a service.
pub(super) const REPLICAS: std::ops::RangeInclusive<u16> = 1..=100;
const MAX_GENERATED_SECRETS: usize = 64;
/// Accepted raw byte counts for random generated secrets, before encoding.
const RANDOM_SECRET_BYTES: std::ops::RangeInclusive<u16> = 16..=512;
/// Accepted RSA modulus sizes for generated private keys.
const RSA_SECRET_BITS: [u16; 3] = [2048, 3072, 4096];

impl Build {
    /// Validates build inputs without reading the checkout.
    ///
    /// Appends errors at `path` for Dockerfile or context paths that are not
    /// relative or leave the repository, build arguments that break the
    /// environment variable name, count, and size rules or together exceed
    /// their command-line budget, and targets that are not Docker stage names.
    pub fn validate(&self, path: &str, errors: &mut Vec<ValidationError>) {
        let Self::Docker {
            dockerfile,
            context,
            args,
            target,
        } = self;
        for (field, value) in [("dockerfile", dockerfile), ("context", context)] {
            if value
                .as_literal()
                .is_some_and(|value| !valid_repository_path(&value))
            {
                errors.push(ValidationError::new(
                    codes::REPOSITORY_PATH_INVALID,
                    format!("{path}.{field}"),
                    "path must be relative to the repository root and remain within it",
                ));
            }
        }
        VariableMap::BUILD_ARGS.validate(args, path, errors);
        // Each argument is passed as `KEY=VALUE`; references count once rendered.
        let total: usize = args
            .iter()
            .filter_map(|(key, value)| Some(key.len() + value.as_literal()?.len() + 1))
            .sum();
        if total > MAX_BUILD_ARG_TOTAL_BYTES {
            errors.push(ValidationError::new(
                codes::BUILD_ARG_TOTAL_EXCESSIVE,
                format!("{path}.args"),
                format!("build arguments must total at most {MAX_BUILD_ARG_TOTAL_BYTES} bytes"),
            ));
        }
        if target
            .as_ref()
            .and_then(Template::as_literal)
            .is_some_and(|target| !valid_build_target(&target))
        {
            errors.push(ValidationError::new(
                codes::BUILD_TARGET_INVALID,
                format!("{path}.target"),
                format!(
                    "build target must start with a letter, use letters, digits, '-', '_', or '.', and be at most {MAX_IDENTIFIER_BYTES} bytes"
                ),
            ));
        }
    }
}

impl GitRepository {
    /// Validates Git arguments without executing Git.
    ///
    /// Appends errors at `path` for inline URL credentials (any `http(s)`
    /// userinfo, or `user:password@` in other schemes), empty or option-like
    /// URLs, branch names failing `git check-ref-format` style rules, and commits
    /// that are not full lowercase hashes. Leading `-` is rejected so values
    /// cannot be parsed as Git options.
    pub fn validate(&self, path: &str, errors: &mut Vec<ValidationError>) {
        if [Some(&self.url), Some(&self.branch), self.commit.as_ref()]
            .into_iter()
            .flatten()
            .any(|value| Template::mentions_reference(value))
        {
            errors.push(ValidationError::new(
                codes::VARIABLE_NOT_ALLOWED,
                path,
                "repository URLs, branches, and commits cannot reference variables",
            ));
        }
        let inline_credentials = self.url.split_once("://").is_some_and(|(scheme, rest)| {
            let authority = rest.split(['/', '?', '#', '\\']).next().unwrap_or_default();
            authority.rsplit_once('@').is_some_and(|(userinfo, _)| {
                scheme.eq_ignore_ascii_case("http")
                    || scheme.eq_ignore_ascii_case("https")
                    || userinfo.contains(':')
            })
        });
        if inline_credentials {
            errors.push(ValidationError::new(
                codes::GIT_CREDENTIALS_INLINE,
                path,
                "configure Git credentials on the host, not in repository URLs",
            ));
        }
        if self.url.is_empty()
            || self.url.len() > 4096
            || self.url.starts_with('-')
            || self.url.chars().any(char::is_control)
        {
            errors.push(ValidationError::new(
                codes::GIT_REPOSITORY_INVALID,
                path,
                "repository URL must be a nonempty Git location",
            ));
        }
        if let Err(invalid) = crate::TrackedBranch::new(self.branch.clone(), self.commit.clone()) {
            errors.extend(invalid.0.into_iter().map(|error| ValidationError {
                path: path.into(),
                ..error
            }));
        }
    }
}

impl ManifestRevision {
    /// Points `manifest`, a specification's repository connection, at this
    /// revision for one deployment.
    ///
    /// # Errors
    /// Rejects applications without a manifest repository and invalid revisions.
    pub(crate) fn apply(
        &self,
        manifest: &mut Option<RepositoryManifest>,
    ) -> Result<(), ValidationErrors> {
        let mut errors = Vec::new();
        let path = "spec.manifest.repository";
        match manifest {
            Some(manifest) => {
                manifest.repository = manifest.repository.at(self);
                manifest.repository.validate(path, &mut errors);
            }
            None => Self::unbacked(path, &mut errors),
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(ValidationErrors(errors))
        }
    }

    /// Records that `path` needs a repository-backed manifest.
    pub(crate) fn unbacked(path: &str, errors: &mut Vec<ValidationError>) {
        errors.push(ValidationError::new(
            codes::MANIFEST_REPOSITORY_REQUIRED,
            path,
            "the manifest repository applies only when spec.manifest is configured",
        ));
    }
}

/// Whether a value is a branch name `git check-ref-format --branch` would
/// accept, and cannot be read as a Git option.
///
/// ```text
/// "main", "release/2.0"                     -> valid
/// "-x", "/main", "a..b", "a/.hidden", "x.lock" -> invalid
/// ```
#[must_use]
pub fn valid_git_branch(branch: &str) -> bool {
    !(branch.is_empty()
        || branch.len() > 255
        || branch == "@"
        || branch.starts_with(['-', '/'])
        || branch.ends_with(['/', '.'])
        || branch.contains("..")
        || branch.contains("@{")
        || branch.contains("//")
        || branch.split('/').any(|component| {
            component.starts_with('.') || component.strip_suffix(".lock").is_some()
        })
        || branch
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || "~^:?*[\\".contains(c)))
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

impl ValidationError {
    /// An error with the stable `code` at the manifest field `path`.
    #[must_use]
    pub fn new(
        code: impl Into<String>,
        path: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code: code.into(),
            path: path.into(),
            message: message.into(),
        }
    }
}

/// All independently discoverable manifest errors, in stable path order.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(transparent)]
pub struct ValidationErrors(pub Vec<ValidationError>);

impl ValidationErrors {
    /// Orders errors by path, then code.
    pub(crate) fn sorted(mut errors: Vec<ValidationError>) -> Self {
        errors.sort_by(|left, right| left.path.cmp(&right.path).then(left.code.cmp(&right.code)));
        Self(errors)
    }

    /// One decode failure. Serde messages carry the rejected field and
    /// location, and may echo a mistyped value back to its submitter.
    /// Manifests reference secrets by name only, so they are safe to return.
    fn decode(path: &str, message: String) -> Self {
        ValidationError::new(
            codes::MANIFEST_DECODE_FAILED,
            safe_decode_path(path),
            message,
        )
        .into()
    }
}

/// A single error.
impl From<ValidationError> for ValidationErrors {
    fn from(error: ValidationError) -> Self {
        Self(vec![error])
    }
}

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

/// Parses and validates a strict TOML manifest whose values may reference
/// variables, without performing I/O.
///
/// # Errors
/// Returns validation errors, or one decode error naming the rejected input
/// and its location.
pub fn parse_template_toml(input: &str) -> Result<ValidatedTemplate, ValidationErrors> {
    ApplicationManifest::decode_toml(input)?.validate_template()
}

/// Parses and validates a strict JSON manifest whose values may reference
/// variables, without performing I/O.
///
/// # Errors
/// Returns validation errors, or one decode error naming the rejected input
/// and its location.
pub fn parse_template_json(input: &str) -> Result<ValidatedTemplate, ValidationErrors> {
    ApplicationManifest::decode_json(input)?.validate_template()
}

/// Parses and validates strict TOML that references no variables.
///
/// # Errors
/// Returns validation errors, including `variable_unresolved` for each
/// reference, or one decode error naming the rejected input and its location.
pub fn parse_toml(input: &str) -> Result<ValidatedApplication, ValidationErrors> {
    parse_template_toml(input)?.into_literal()
}

/// Parses and validates strict JSON that references no variables.
///
/// # Errors
/// Returns validation errors, including `variable_unresolved` for each
/// reference, or one decode error naming the rejected input and its location.
pub fn parse_json(input: &str) -> Result<ValidatedApplication, ValidationErrors> {
    parse_template_json(input)?.into_literal()
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
        "args",
        "volume",
        "target",
        "read_only",
        "port",
        "routes",
        "jobs",
        "run",
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
        "rollout",
        "order",
        "monitor_seconds",
        "variables",
        "environments",
        "exports",
        "imports",
        "application",
        "applications",
        "previews",
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

/// Explains the mutually exclusive route destinations.
pub(super) const ROUTE_TARGET_MESSAGE: &str =
    "route must set either both `service` and `port`, or `redirect`";

impl super::Redirect {
    /// Checks a literal destination URL and the status code of the redirect
    /// of the route at `path`.
    fn validate(&self, path: &str, errors: &mut Vec<ValidationError>) {
        if let Some(Err(source)) = self.to.as_literal().map(super::RedirectUrl::parse) {
            errors.push(ValidationError::new(
                codes::ROUTE_REDIRECT_INVALID,
                format!("{path}.redirect.to"),
                source.to_string(),
            ));
        }
        if let Err(source) = super::RedirectStatus::try_from(self.status) {
            errors.push(ValidationError::new(
                codes::ROUTE_REDIRECT_STATUS_INVALID,
                format!("{path}.redirect.status"),
                source.to_string(),
            ));
        }
    }
}

/// Whether a redirect from `hostname` to `to` reaches the route itself: its
/// own hostname, or a name its wildcard matches that no exact route in
/// `exact` claims.
fn redirect_loops(hostname: &str, to: &str, exact: &BTreeSet<String>) -> bool {
    let (Ok(hostname), Ok(to)) = (
        super::RouteHostname::parse(hostname),
        super::RedirectUrl::parse(to),
    ) else {
        return false;
    };
    hostname.as_str() == to.hostname()
        || super::Hostname::parse(to.hostname())
            .is_ok_and(|to| hostname.matches(&to) && !exact.contains(to.as_str()))
}

impl ApplicationManifest {
    /// Canonicalizes route hostnames in place (lowercase, trailing dot removed),
    /// then checks hostname syntax, uniqueness, the target service, the port,
    /// and that redirects do not loop.
    /// Hostnames that reference variables are checked once rendered. Names
    /// are literal, so they are checked here and stay the same everywhere.
    fn validate_routes(&mut self, errors: &mut Vec<ValidationError>) {
        let mut hostnames = BTreeSet::new();
        let mut names = BTreeSet::new();
        for (index, route) in self.spec.routes.iter_mut().enumerate() {
            if let Some(name) = &route.name {
                let path = format!("spec.routes[{index}].name");
                validate_name(name, &path, errors);
                if !names.insert(name.clone()) {
                    errors.push(ValidationError::new(
                        codes::ROUTE_NAME_DUPLICATE,
                        &path,
                        "route name is already used in this application",
                    ));
                }
            }
            let hostname = route
                .hostname
                .as_literal()
                .map(|hostname| super::RouteHostname::canonicalize(&hostname));
            if let Some(hostname) = &hostname {
                route.hostname = Template::literal(hostname);
            }
            let path = format!("spec.routes[{index}]");
            if hostname
                .as_ref()
                .is_some_and(|hostname| super::RouteHostname::parse(hostname).is_err())
            {
                errors.push(ValidationError::new(
                    codes::ROUTE_HOSTNAME_INVALID,
                    format!("{path}.hostname"),
                    "an exact public ASCII DNS hostname, or `*.` followed by one, is required",
                ));
            }
            if let Some(hostname) = hostname.clone()
                && !hostnames.insert(hostname)
            {
                errors.push(ValidationError::new(
                    codes::ROUTE_HOSTNAME_DUPLICATE,
                    format!("{path}.hostname"),
                    "hostname is already used in this application",
                ));
            }
            match (&route.service, route.port, &route.redirect) {
                (Some(service), Some(port), None) => {
                    if !self.spec.services.iter().any(|s| &s.name == service) {
                        errors.push(ValidationError::new(
                            codes::ROUTE_SERVICE_MISSING,
                            format!("{path}.service"),
                            "route must reference a service in this application",
                        ));
                    }
                    if port == 0 {
                        errors.push(ValidationError::new(
                            codes::ROUTE_PORT_INVALID,
                            format!("{path}.port"),
                            "HTTP backend port must be 1..=65535",
                        ));
                    }
                }
                (None, None, Some(redirect)) => redirect.validate(&path, errors),
                _ => errors.push(ValidationError::new(
                    codes::ROUTE_TARGET_INVALID,
                    &path,
                    ROUTE_TARGET_MESSAGE,
                )),
            }
        }
        for (index, route) in self.spec.routes.iter().enumerate() {
            if let (Some(hostname), Some(to)) = (
                route.hostname.as_literal(),
                route.redirect.as_ref().and_then(|r| r.to.as_literal()),
            ) && redirect_loops(&hostname, &to, &hostnames)
            {
                errors.push(ValidationError::new(
                    codes::ROUTE_REDIRECT_LOOP,
                    format!("spec.routes[{index}].redirect.to"),
                    "redirect must target a hostname the route does not serve",
                ));
            }
        }
    }

    /// Decodes strict TOML without validating it, so callers can adjust the
    /// manifest (e.g. replace `spec.manifest`) before
    /// [`Self::validate_template`].
    ///
    /// # Errors
    /// Returns one decode error naming the rejected input and its location.
    pub fn decode_toml(input: &str) -> Result<Self, ValidationErrors> {
        serde_path_to_error::deserialize(toml::Deserializer::new(input)).map_err(|error| {
            ValidationErrors::decode(
                &error.path().to_string(),
                crate::TomlDiagnostic::new(input, error.inner()).to_string(),
            )
        })
    }

    /// Decodes strict JSON without validating it; see [`Self::decode_toml`].
    ///
    /// # Errors
    /// Returns one decode error naming the rejected input and its location.
    pub fn decode_json(input: &str) -> Result<Self, ValidationErrors> {
        let mut deserializer = serde_json::Deserializer::from_str(input);
        let manifest = serde_path_to_error::deserialize(&mut deserializer).map_err(|error| {
            ValidationErrors::decode(&error.path().to_string(), error.inner().to_string())
        })?;
        deserializer
            .end()
            .map_err(|error| ValidationErrors::decode("$", error.to_string()))?;
        Ok(manifest)
    }

    /// Validates a manifest whose values may reference variables, as far as
    /// its literal values allow. Values that reference variables are checked
    /// again once rendered for an environment.
    ///
    /// Collects every independent error rather than stopping at the first:
    /// 1. Header, metadata, and optional repository manifest source.
    /// 2. Route, service, and volume budgets; exceeding one returns early to
    ///    bound work.
    /// 3. Routes, duplicate names, services, volumes, and variables.
    /// 4. On success, canonicalizes literal hostnames and image registries.
    ///
    /// Errors are sorted by path then code.
    ///
    /// # Errors
    /// Returns all detected manifest validation errors.
    pub fn validate_template(mut self) -> Result<ValidatedTemplate, ValidationErrors> {
        let mut errors = Vec::new();
        self.validate_header(&mut errors);
        if let Some(manifest) = &self.spec.manifest {
            manifest
                .repository
                .validate("spec.manifest.repository", &mut errors);
            manifest.sync.validate("spec.manifest.sync", &mut errors);
            if Template::mentions_reference(&manifest.path) {
                errors.push(ValidationError::new(
                    codes::VARIABLE_NOT_ALLOWED,
                    "spec.manifest.path",
                    "the manifest path cannot reference variables",
                ));
            } else if !valid_repository_path(&manifest.path) {
                errors.push(ValidationError::new(
                    codes::REPOSITORY_PATH_INVALID,
                    "spec.manifest.path",
                    "manifest path must remain within the repository root",
                ));
            }
        }
        // Bound work before walking attacker-controlled collections.
        if !self.spec.within_budgets(&mut errors) {
            return Err(ValidationErrors::sorted(errors));
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
        for (index, service) in self.spec.services.iter().enumerate() {
            service.validate(
                &format!("spec.services[{index}]"),
                &volume_names,
                self.spec.manifest.is_some(),
                &mut errors,
            );
        }
        self.spec.validate_dependencies(&mut errors);
        if self.spec.inline_file_bytes() > ServiceFile::MAX_TOTAL_BYTES {
            errors.push(ValidationError::new(
                codes::FILE_TOTAL_EXCESSIVE,
                "spec.services",
                format!(
                    "inline file contents must total at most {} bytes",
                    ServiceFile::MAX_TOTAL_BYTES
                ),
            ));
        }
        self.spec.validate_volumes(&mut errors);
        self.spec.validate_generated_secrets(&mut errors);
        self.spec.validate_jobs(&mut errors);
        self.spec.validate_links(&mut errors);
        self.spec.check_variables(&mut errors);
        if let Err(invalid) = self.spec.declared_environments() {
            errors.extend(invalid.0);
        }
        if !errors.is_empty() {
            return Err(ValidationErrors::sorted(errors));
        }
        for service in &mut self.spec.services {
            if let Source::Image { image } = &mut service.source
                && let Some(literal) = image.as_literal()
            {
                *image = Template::literal(&canonicalize_image_reference(&literal));
            }
        }
        Ok(ValidatedTemplate::new(
            super::domain::ValidatedMetadata::from_input(self.metadata)?,
            self.spec,
        ))
    }

    /// Validates a manifest that references no variables.
    ///
    /// # Errors
    /// Returns all detected manifest validation errors, including
    /// `variable_unresolved` for each reference.
    pub fn validate(self) -> Result<ValidatedApplication, ValidationErrors> {
        self.validate_template()?.into_literal()
    }

    /// Checks the API version, kind, and application name.
    fn validate_header(&self, errors: &mut Vec<ValidationError>) {
        if self.api_version != APPLICATION_API_VERSION {
            errors.push(ValidationError::new(
                codes::API_VERSION_UNSUPPORTED,
                "api_version",
                "unsupported application API version",
            ));
        }
        if self.kind != APPLICATION_KIND {
            errors.push(ValidationError::new(
                codes::KIND_UNSUPPORTED,
                "kind",
                "resource kind must be Application",
            ));
        }
        validate_name(&self.metadata.name, "metadata.name", errors);
    }
}

impl ApplicationSpec {
    /// Checks route, service, volume, secret, and per-service dependency counts;
    /// returns `false` when a budget is exceeded.
    fn within_budgets(&self, errors: &mut Vec<ValidationError>) -> bool {
        let mut within_budget = true;
        if self.routes.len() > MAX_ROUTES {
            errors.push(ValidationError::new(
                codes::ROUTES_LIMIT,
                "spec.routes",
                format!("at most {MAX_ROUTES} routes are allowed per application"),
            ));
            within_budget = false;
        }
        if self.services.len() > MAX_SERVICES {
            errors.push(ValidationError::new(
                codes::SERVICE_COUNT_EXCESSIVE,
                "spec.services",
                format!("an application must declare at most {MAX_SERVICES} services"),
            ));
            within_budget = false;
        } else {
            // Unique dependencies name other services, so a valid list is shorter.
            for (index, service) in self.services.iter().enumerate() {
                if service.depends_on.len() > MAX_SERVICES {
                    errors.push(ValidationError::new(
                        codes::SERVICE_DEPENDENCY_COUNT_EXCESSIVE,
                        format!("spec.services[{index}].depends_on"),
                        format!("a service must list at most {MAX_SERVICES} dependencies"),
                    ));
                    within_budget = false;
                }
            }
        }
        if self.volumes.len() > MAX_VOLUMES {
            errors.push(ValidationError::new(
                codes::VOLUME_COUNT_EXCESSIVE,
                "spec.volumes",
                format!("an application must declare at most {MAX_VOLUMES} volumes"),
            ));
            within_budget = false;
        }
        if self.secrets.len() > MAX_GENERATED_SECRETS {
            errors.push(ValidationError::new(
                codes::SECRETS_EXCESSIVE,
                "spec.secrets",
                format!("an application must declare at most {MAX_GENERATED_SECRETS} secrets"),
            ));
            within_budget = false;
        }
        if self.jobs.len() > Job::MAX_PER_APPLICATION {
            errors.push(ValidationError::new(
                codes::JOB_COUNT_EXCESSIVE,
                "spec.jobs",
                format!(
                    "an application must declare at most {} jobs",
                    Job::MAX_PER_APPLICATION
                ),
            ));
            within_budget = false;
        }
        self.links_within_budget(errors) && within_budget
    }

    /// Dependencies must name other services once each and must not form a cycle.
    fn validate_dependencies(&self, errors: &mut Vec<ValidationError>) {
        let services = &self.services;
        let names = services
            .iter()
            .map(|service| service.name.as_str())
            .collect::<BTreeSet<_>>();
        for (index, service) in services.iter().enumerate() {
            let mut listed = BTreeSet::new();
            for (dependency_index, dependency) in service.depends_on.iter().enumerate() {
                let path = format!("spec.services[{index}].depends_on[{dependency_index}]");
                if !names.contains(dependency.as_str()) {
                    errors.push(ValidationError::new(
                        codes::SERVICE_DEPENDENCY_MISSING,
                        &path,
                        "dependency must name a service in this application",
                    ));
                } else if !listed.insert(dependency) {
                    errors.push(ValidationError::new(
                        codes::SERVICE_DEPENDENCY_DUPLICATE,
                        &path,
                        "dependency is listed more than once",
                    ));
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
                errors.push(ValidationError::new(
                    codes::SERVICE_DEPENDENCY_CYCLE,
                    format!("spec.services[{index}].depends_on"),
                    "dependencies must not form or lead into a cycle",
                ));
            }
        }
    }

    /// Checks every job: a unique valid name, a declared service, an order after
    /// the jobs of its service's startup dependencies, a command, its volume
    /// access, and its timeout.
    fn validate_jobs(&self, errors: &mut Vec<ValidationError>) {
        let (jobs, services) = (&self.jobs, &self.services);
        unique_names(
            jobs.iter().map(|job| &job.name),
            "spec.jobs",
            codes::JOB_NAME_DUPLICATE,
            errors,
        );
        for (index, job) in jobs.iter().enumerate() {
            let base = format!("spec.jobs[{index}]");
            validate_name(&job.name, &format!("{base}.name"), errors);
            if !services.iter().any(|service| service.name == job.service) {
                errors.push(ValidationError::new(
                    codes::JOB_SERVICE_MISSING,
                    format!("{base}.service"),
                    "job must reference a service in this application",
                ));
            }
            let dependencies = services.dependencies_of(&job.service);
            for prerequisite in jobs[index..]
                .iter()
                .filter(|prerequisite| dependencies.contains(prerequisite.service.as_str()))
            {
                errors.push(ValidationError::new(
                    codes::JOB_DEPENDENCY_ORDER_INVALID,
                    format!("{base}.service"),
                    format!(
                        "job {} for dependency service {} must run before job {}",
                        prerequisite.name, prerequisite.service, job.name
                    ),
                ));
            }
            if job.command.first().is_none_or(|value| {
                value
                    .as_literal()
                    .is_some_and(|value| value.trim().is_empty())
            }) {
                errors.push(ValidationError::new(
                    codes::PROCESS_COMMAND_INVALID,
                    format!("{base}.command"),
                    "a job command must start with a non-empty executable",
                ));
            }
            validate_process_arguments(
                &job.command,
                &format!("{base}.command"),
                codes::PROCESS_COMMAND_EXCESSIVE,
                errors,
            );
            if job.volume_access == JobVolumeAccess::Exclusive {
                job.validate_exclusive_access(services, &dependencies, &base, errors);
            }
            if !(1..=Job::MAX_TIMEOUT_SECONDS).contains(&job.timeout_seconds) {
                errors.push(ValidationError::new(
                    codes::JOB_TIMEOUT_INVALID,
                    format!("{base}.timeout_seconds"),
                    format!(
                        "job timeout must be between 1 and {} seconds",
                        Job::MAX_TIMEOUT_SECONDS
                    ),
                ));
            }
        }
    }

    /// Checks each declared volume name.
    fn validate_volumes(&self, errors: &mut Vec<ValidationError>) {
        for (index, volume) in self.volumes.iter().enumerate() {
            validate_name(&volume.name, &format!("spec.volumes[{index}].name"), errors);
        }
    }

    /// Checks generated secret declarations: unique valid names, random byte counts
    /// in `RANDOM_SECRET_BYTES`, and RSA sizes in `RSA_SECRET_BITS`. Invalid
    /// generator parameters are reported at `spec.secrets[i].generate.bytes` or
    /// `.bits` with `secret_generator_invalid`.
    fn validate_generated_secrets(&self, errors: &mut Vec<ValidationError>) {
        let secrets = &self.secrets;
        unique_names(
            secrets.iter().map(|secret| &secret.name),
            "spec.secrets",
            codes::SECRET_NAME_DUPLICATE,
            errors,
        );
        for (index, secret) in secrets.iter().enumerate() {
            let path = format!("spec.secrets[{index}]");
            validate_name(&secret.name, &format!("{path}.name"), errors);
            match secret.generate {
                SecretGenerator::Random { bytes, .. } if !RANDOM_SECRET_BYTES.contains(&bytes) => {
                    errors.push(ValidationError::new(
                        codes::SECRET_GENERATOR_INVALID,
                        format!("{path}.generate.bytes"),
                        format!(
                            "random secrets must use {}-{} bytes",
                            RANDOM_SECRET_BYTES.start(),
                            RANDOM_SECRET_BYTES.end()
                        ),
                    ));
                }
                SecretGenerator::Rsa { bits } if !RSA_SECRET_BITS.contains(&bits) => {
                    errors.push(ValidationError::new(
                        codes::SECRET_GENERATOR_INVALID,
                        format!("{path}.generate.bits"),
                        "RSA keys must use 2048, 3072, or 4096 bits",
                    ));
                }
                _ => {}
            }
        }
    }
}

impl Source {
    /// Validates Git repository selection and build inputs.
    fn validate_git(&self, base: &str, manifest_backed: bool, errors: &mut Vec<ValidationError>) {
        if let Self::Git { repository, build } = self {
            let path = format!("{base}.source.repository");
            match repository {
                SourceRepository::Git(repository) => repository.validate(&path, errors),
                SourceRepository::Manifest(_) if !manifest_backed => {
                    ManifestRevision::unbacked(&path, errors);
                }
                SourceRepository::Manifest(_) => {}
            }
            build.validate(&format!("{base}.source.build"), errors);
        }
    }
}

impl Service {
    /// Validates the service at `base`: name, replicas, source, secrets, files,
    /// environment, mounts, process arguments, health check, and resource limits.
    ///
    /// `volume_names` holds declared volumes for mount references, and
    /// `manifest_backed` allows `"self"` Git sources and repository files. Secret
    /// targets must be unique under `/run/secrets/` and must not collide with
    /// mount targets; file targets collide with neither.
    fn validate(
        &self,
        base: &str,
        volume_names: &BTreeSet<String>,
        manifest_backed: bool,
        errors: &mut Vec<ValidationError>,
    ) {
        validate_name(&self.name, &format!("{base}.name"), errors);
        if self
            .replicas
            .literal()
            .is_some_and(|replicas| !REPLICAS.contains(replicas))
        {
            errors.push(ValidationError::new(
                codes::REPLICAS_OUT_OF_RANGE,
                format!("{base}.replicas"),
                format!(
                    "replicas must be between {} and {}",
                    REPLICAS.start(),
                    REPLICAS.end()
                ),
            ));
        }
        if let Source::Image { image } = &self.source
            && image
                .as_literal()
                .is_some_and(|image| !valid_image_reference(&image))
        {
            errors.push(ValidationError::new(
                codes::IMAGE_INVALID,
                format!("{base}.source.image"),
                "image must be a valid registry reference without credentials or a URL scheme",
            ));
        }
        self.source.validate_git(base, manifest_backed, errors);
        let mut targets = self
            .mounts
            .iter()
            .map(|m| m.target.as_str())
            .collect::<BTreeSet<_>>();
        for (index, secret) in self.secrets.iter().enumerate() {
            let path = format!("{base}.secrets[{index}]");
            // Names that reference variables are validated once rendered.
            if let Some(name) = secret.name.as_literal() {
                validate_name(&name, &format!("{path}.name"), errors);
            }
            validate_absolute_path(&secret.target, &format!("{path}.target"), errors);
            if !secret.target.starts_with("/run/secrets/") || !targets.insert(&secret.target) {
                errors.push(ValidationError::new(
                    codes::SECRET_TARGET_INVALID,
                    &path,
                    "secret targets must be unique file paths under /run/secrets",
                ));
            }
        }
        if self.secrets.len() > MAX_SECRET_MOUNTS_PER_SERVICE {
            errors.push(ValidationError::new(
                codes::SECRETS_EXCESSIVE,
                base,
                format!("at most {MAX_SECRET_MOUNTS_PER_SERVICE} secret file mounts per service"),
            ));
        }
        self.validate_files(base, manifest_backed, &mut targets, errors);
        VariableMap::ENVIRONMENT.validate(&self.environment, base, errors);
        self.validate_mounts(base, volume_names, errors);
        validate_process_arguments(
            &self.command,
            &format!("{base}.command"),
            codes::PROCESS_COMMAND_EXCESSIVE,
            errors,
        );
        validate_process_arguments(
            &self.arguments,
            &format!("{base}.arguments"),
            codes::PROCESS_ARGUMENTS_EXCESSIVE,
            errors,
        );
        if self
            .command
            .first()
            .and_then(Template::as_literal)
            .is_some_and(|value| value.trim().is_empty())
        {
            errors.push(ValidationError::new(
                codes::PROCESS_COMMAND_INVALID,
                format!("{base}.command[0]"),
                "an explicit container command must start with a non-empty executable",
            ));
        }
        if let Some(healthcheck) = &self.healthcheck {
            healthcheck.validate(&format!("{base}.healthcheck"), errors);
        }
        if let Some(resources) = &self.resources {
            resources.validate(base, errors);
        }
        self.rollout.validate(base, errors);
    }

    /// Checks file count, that each target is safe and collides with no other
    /// target in `targets` (mounts, secrets, and earlier files) or a reserved
    /// directory, that repository paths stay inside a repository the application
    /// has, and that inline contents fit one Swarm config.
    fn validate_files<'a>(
        &'a self,
        base: &str,
        manifest_backed: bool,
        targets: &mut BTreeSet<&'a str>,
        errors: &mut Vec<ValidationError>,
    ) {
        let files = &self.files;
        if files.len() > ServiceFile::MAX_PER_SERVICE {
            errors.push(ValidationError::new(
                codes::FILE_COUNT_EXCESSIVE,
                format!("{base}.files"),
                format!(
                    "a service must mount at most {} files",
                    ServiceFile::MAX_PER_SERVICE
                ),
            ));
        }
        // Mount, secret, and file targets a file must not contain.
        let others = targets
            .iter()
            .copied()
            .chain(files.iter().map(|file| file.target.as_str()))
            .collect::<Vec<_>>();
        for (index, file) in files.iter().enumerate() {
            let path = format!("{base}.files[{index}]");
            validate_absolute_path(&file.target, &format!("{path}.target"), errors);
            if file.reserved_target()
                || others.iter().any(|other| file.contains(other))
                || !targets.insert(&file.target)
            {
                errors.push(ValidationError::new(
                    codes::FILE_TARGET_INVALID,
                    format!("{path}.target"),
                    format!(
                        "file targets must be unique among the service's mount, secret, and file targets, contain none of them, and stay clear of {}",
                        ServiceFile::RESERVED_DIRECTORIES.join(" and ")
                    ),
                ));
            }
            let (code, field, message) = match &file.source {
                FileSource::Path(_) if !manifest_backed => (
                    codes::FILE_REQUIRES_REPOSITORY,
                    "path",
                    "only applications whose manifest comes from a repository can mount repository files; set content instead".to_owned(),
                ),
                FileSource::Path(relative) if !valid_repository_path(relative) => (
                    codes::REPOSITORY_PATH_INVALID,
                    "path",
                    "file path must remain within the repository root".to_owned(),
                ),
                FileSource::Content(content) if content.is_empty() => (
                    codes::FILE_EMPTY,
                    "content",
                    "file content cannot be empty".to_owned(),
                ),
                FileSource::Content(content) if content.len() > ServiceFile::MAX_BYTES => (
                    codes::FILE_TOO_LARGE,
                    "content",
                    format!(
                        "file content must be at most {} bytes",
                        ServiceFile::MAX_BYTES
                    ),
                ),
                FileSource::Path(_) | FileSource::Content(_) => continue,
            };
            errors.push(ValidationError::new(
                code,
                format!("{path}.{field}"),
                message,
            ));
        }
    }

    /// Checks mount count, that each mount references a declared volume, and that
    /// targets are safe and unique.
    fn validate_mounts(
        &self,
        base: &str,
        volume_names: &BTreeSet<String>,
        errors: &mut Vec<ValidationError>,
    ) {
        let mounts = &self.mounts;
        let mut targets = BTreeSet::new();
        if mounts.len() > MAX_MOUNTS_PER_SERVICE {
            errors.push(ValidationError::new(
                codes::MOUNT_COUNT_EXCESSIVE,
                format!("{base}.mounts"),
                format!("a service must declare at most {MAX_MOUNTS_PER_SERVICE} mounts"),
            ));
        }
        for (index, mount) in mounts.iter().enumerate() {
            let path = format!("{base}.mounts[{index}]");
            if !volume_names.contains(&mount.volume) {
                errors.push(ValidationError::new(
                    codes::MOUNT_VOLUME_MISSING,
                    format!("{path}.volume"),
                    "mount references an undeclared volume",
                ));
            }
            validate_absolute_path(&mount.target, &format!("{path}.target"), errors);
            // HTTP health checks mount piqueld-probe there.
            if mount.target == piqueld_probe::CONTAINER_PATH
                || mount
                    .target
                    .starts_with(&format!("{}/", piqueld_probe::CONTAINER_PATH))
            {
                errors.push(ValidationError::new(
                    codes::MOUNT_TARGET_UNSAFE,
                    format!("{path}.target"),
                    format!(
                        "mount target {} is reserved for piqueld's health probe",
                        piqueld_probe::CONTAINER_PATH
                    ),
                ));
            }
            if !targets.insert(mount.target.clone()) {
                errors.push(ValidationError::new(
                    codes::MOUNT_TARGET_DUPLICATE,
                    format!("{path}.target"),
                    "mount target is duplicated in this service",
                ));
            }
        }
    }
}

impl super::Rollout {
    /// Checks a literal rollout monitor window.
    fn validate(&self, base: &str, errors: &mut Vec<ValidationError>) {
        if self
            .monitor_seconds
            .as_ref()
            .and_then(Typed::literal)
            .is_some_and(|seconds| !(1..=ValidatedRollout::MAX_MONITOR_SECONDS).contains(seconds))
        {
            errors.push(ValidationError::new(
                codes::ROLLOUT_MONITOR_INVALID,
                format!("{base}.rollout.monitor_seconds"),
                format!(
                    "rollout monitor must be between 1 and {} seconds",
                    ValidatedRollout::MAX_MONITOR_SECONDS
                ),
            ));
        }
    }
}

/// Rules for a validated name/value map passed to a process, such as a
/// container environment or Docker build arguments.
struct VariableMap {
    /// Human-readable entry kind used in messages.
    noun: &'static str,
    /// Field name below the validated path.
    field: &'static str,
    /// Name piqueld sets itself for ingress, which users cannot declare.
    reserved: Option<&'static str>,
    // Error codes reported for each rule.
    name_invalid: &'static str,
    value_invalid: &'static str,
    count_excessive: &'static str,
    value_excessive: &'static str,
}

impl VariableMap {
    /// Service environment, validated at `<service>.environment`.
    const ENVIRONMENT: Self = Self {
        noun: "environment",
        field: "environment",
        reserved: Some(crate::resource::INGRESS_PROXIES_ENV),
        name_invalid: codes::ENVIRONMENT_NAME_INVALID,
        value_invalid: codes::ENVIRONMENT_VALUE_INVALID,
        count_excessive: codes::ENVIRONMENT_COUNT_EXCESSIVE,
        value_excessive: codes::ENVIRONMENT_VALUE_EXCESSIVE,
    };
    /// Docker build arguments, validated at `<build>.args`.
    const BUILD_ARGS: Self = Self {
        noun: "build argument",
        field: "args",
        reserved: None,
        name_invalid: codes::BUILD_ARG_NAME_INVALID,
        value_invalid: codes::BUILD_ARG_VALUE_INVALID,
        count_excessive: codes::BUILD_ARG_COUNT_EXCESSIVE,
        value_excessive: codes::BUILD_ARG_VALUE_EXCESSIVE,
    };

    /// Appends errors at `base.field` when `values` has too many entries, a
    /// name is not a POSIX-style identifier or exceeds the identifier bound, or
    /// a literal value contains NUL or exceeds its byte budget. Values that
    /// reference variables are checked once rendered.
    fn validate(
        &self,
        values: &BTreeMap<String, Template>,
        base: &str,
        errors: &mut Vec<ValidationError>,
    ) {
        let Self { noun, field, .. } = self;
        if values.len() > MAX_VARIABLE_ENTRIES {
            errors.push(ValidationError::new(
                self.count_excessive,
                format!("{base}.{field}"),
                format!("a service must declare at most {MAX_VARIABLE_ENTRIES} {noun} entries"),
            ));
        }
        for (key, value) in values {
            let key_echo = safe_key_echo(key);
            if Template::mentions_reference(key) {
                errors.push(ValidationError::new(
                    codes::VARIABLE_NOT_ALLOWED,
                    format!("{base}.{field}.name"),
                    format!("{noun} key {key_echo} cannot reference variables"),
                ));
            } else if !valid_env_name(key) || key.len() > MAX_IDENTIFIER_BYTES {
                errors.push(ValidationError::new(
                    self.name_invalid,
                    format!("{base}.{field}.name"),
                    format!(
                        "{noun} key {key_echo} is invalid: names must use letters, digits, and underscores, cannot start with a digit, and must be at most {MAX_IDENTIFIER_BYTES} bytes"
                    ),
                ));
            }
            if self.reserved == Some(key.as_str()) {
                errors.push(ValidationError::new(
                    self.name_invalid,
                    format!("{base}.{field}.name"),
                    format!("{noun} key {key_echo} is reserved for piqueld ingress"),
                ));
            }
            let Some(value) = value.as_literal() else {
                continue;
            };
            if value.contains('\0') {
                errors.push(ValidationError::new(
                    self.value_invalid,
                    format!("{base}.{field}.value"),
                    format!("{noun} value for key {key_echo} cannot contain NUL"),
                ));
            }
            if value.len() > MAX_VARIABLE_VALUE_BYTES {
                errors.push(ValidationError::new(
                    self.value_excessive,
                    format!("{base}.{field}.value"),
                    format!(
                        "{noun} value for key {key_echo} must be at most {MAX_VARIABLE_VALUE_BYTES} bytes"
                    ),
                ));
            }
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

impl ResourceLimits {
    /// Checks optional limits: at least one limit set, CPU in `1..=MAX_CPU_MILLIS`,
    /// and memory nonzero and representable as Docker's `i64`.
    fn validate(&self, base: &str, errors: &mut Vec<ValidationError>) {
        if self.cpu_millis.is_none() && self.memory_bytes.is_none() {
            errors.push(ValidationError::new(
                codes::RESOURCE_LIMITS_EMPTY,
                format!("{base}.resources"),
                "resource limits must configure CPU, memory, or both",
            ));
        }
        let cpu_millis = self.cpu_millis.as_ref().and_then(Typed::literal);
        let memory_bytes = self.memory_bytes.as_ref().and_then(Typed::literal);
        if cpu_millis == Some(&0) {
            errors.push(ValidationError::new(
                codes::CPU_LIMIT_INVALID,
                format!("{base}.resources.cpu_millis"),
                "CPU limit must be greater than zero",
            ));
        }
        if cpu_millis.is_some_and(|value| *value > MAX_CPU_MILLIS) {
            errors.push(ValidationError::new(
                codes::CPU_LIMIT_EXCESSIVE,
                format!("{base}.resources.cpu_millis"),
                format!("CPU limit must be at most {MAX_CPU_MILLIS} millicores"),
            ));
        }
        if memory_bytes == Some(&0)
            || memory_bytes.is_some_and(|value| i64::try_from(*value).is_err())
        {
            errors.push(ValidationError::new(
                codes::MEMORY_LIMIT_INVALID,
                format!("{base}.resources.memory_bytes"),
                "memory limit must be greater than zero and fit the runtime value",
            ));
        }
    }
}

impl Job {
    /// An exclusive job needs a writable volume of its service, and none of its
    /// startup dependencies may mount one: they keep running while it holds them.
    fn validate_exclusive_access(
        &self,
        services: &[Service],
        dependencies: &BTreeSet<&str>,
        base: &str,
        errors: &mut Vec<ValidationError>,
    ) {
        let Some(service) = services.iter().find(|service| service.name == self.service) else {
            return;
        };
        let volumes = service
            .mounts
            .iter()
            .filter(|mount| !mount.read_only)
            .map(|mount| mount.volume.as_str())
            .collect::<BTreeSet<_>>();
        if volumes.is_empty() {
            errors.push(ValidationError::new(
                codes::JOB_VOLUME_ACCESS_INVALID,
                format!("{base}.volume_access"),
                format!(
                    "exclusive volume access requires service {} to mount a writable volume",
                    self.service
                ),
            ));
        }
        for (dependency, volume) in services
            .iter()
            .filter(|service| dependencies.contains(service.name.as_str()))
            .flat_map(|service| service.mounts.iter().map(move |mount| (service, mount)))
            .filter(|(_, mount)| volumes.contains(mount.volume.as_str()))
        {
            errors.push(ValidationError::new(
                codes::JOB_VOLUME_ACCESS_INVALID,
                format!("{base}.volume_access"),
                format!(
                    "startup dependency {} mounts volume {}, which job {} needs exclusively",
                    dependency.name, volume.volume, self.name
                ),
            ));
        }
    }
}

impl HealthCheck {
    /// Validates a health check's probe, then its timing: interval in
    /// `1..=MAX_HEALTHCHECK_INTERVAL_SECONDS` and timeout in `1..=interval`.
    ///
    /// HTTP paths must be normalized absolute paths without query or fragment.
    fn validate(&self, path: &str, errors: &mut Vec<ValidationError>) {
        let (interval, timeout) = match self {
            HealthCheck::Http {
                port,
                path: request_path,
                interval_seconds,
                timeout_seconds,
            } => {
                if port.literal() == Some(&0) {
                    errors.push(ValidationError::new(
                        codes::PORT_INVALID,
                        format!("{path}.port"),
                        "health-check port must be between 1 and 65535",
                    ));
                }
                let request_path = request_path.as_literal().unwrap_or_else(|| "/".into());
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
                    errors.push(ValidationError::new(
                        codes::HEALTHCHECK_PATH_INVALID,
                        format!("{path}.path"),
                        "HTTP health-check path must start with / and contain no whitespace",
                    ));
                }
                (interval_seconds.literal(), timeout_seconds.literal())
            }
            HealthCheck::Command {
                command,
                interval_seconds,
                timeout_seconds,
            } => {
                if command.first().is_none_or(|value| {
                    value
                        .as_literal()
                        .is_some_and(|value| value.trim().is_empty())
                }) || command
                    .iter()
                    .any(|value| value.as_literal().is_some_and(|value| value.contains('\0')))
                {
                    errors.push(ValidationError::new(
                        codes::HEALTHCHECK_COMMAND_INVALID,
                        format!("{path}.command"),
                        "health-check command must contain at least one NUL-free argument",
                    ));
                }
                validate_process_arguments(
                    command,
                    &format!("{path}.command"),
                    codes::PROCESS_COMMAND_EXCESSIVE,
                    errors,
                );
                (interval_seconds.literal(), timeout_seconds.literal())
            }
        };
        let Some(&interval) = interval else {
            return;
        };
        if interval == 0 {
            errors.push(ValidationError::new(
                codes::HEALTHCHECK_INTERVAL_INVALID,
                format!("{path}.interval_seconds"),
                "health-check interval must be greater than zero",
            ));
        }
        if interval > MAX_HEALTHCHECK_INTERVAL_SECONDS {
            errors.push(ValidationError::new(
            codes::HEALTHCHECK_INTERVAL_EXCESSIVE,
            format!("{path}.interval_seconds"),
            format!(
                "health-check interval must be at most {MAX_HEALTHCHECK_INTERVAL_SECONDS} seconds"
            ),
        ));
        }
        if interval > 0 && timeout.is_some_and(|&timeout| timeout == 0 || timeout > interval) {
            errors.push(ValidationError::new(
                codes::HEALTHCHECK_TIMEOUT_INVALID,
                format!("{path}.timeout_seconds"),
                "health-check timeout must be greater than zero and no longer than its interval",
            ));
        }
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
            errors.push(ValidationError::new(
                duplicate_code,
                format!("{path}[{index}].name"),
                "name is duplicated",
            ));
        }
    }
    found
}

/// Reports `NAME_INVALID` unless `value` is a safe logical resource name.
pub(super) fn validate_name(value: &str, path: &str, errors: &mut Vec<ValidationError>) {
    if Template::mentions_reference(value) {
        errors.push(ValidationError::new(
            codes::VARIABLE_NOT_ALLOWED,
            path,
            "names cannot reference variables",
        ));
    } else if !valid_logical_name(value) {
        errors.push(ValidationError::new(
            codes::NAME_INVALID,
            path,
            "names must be 1-63 lowercase letters, digits, or hyphens, start with a letter, and end with a letter or digit",
        ));
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
    if name.split('/').any(str::is_empty) {
        return false;
    }
    let repository = match registry_component(name) {
        Some(registry) if !valid_registry_authority(registry) => return false,
        Some(registry) => &name[registry.len() + 1..],
        None => name,
    };
    repository.split('/').all(valid_repository_component)
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

/// Returns the leading registry host of an image name, if it has one: a
/// first path component containing `.` or `:`, or `localhost`.
///
/// ```text
/// "ghcr.io/org/app" -> "ghcr.io"
/// "org/app", "app"  -> none
/// ```
pub(crate) fn registry_component(name: &str) -> Option<&str> {
    let (first, _) = name.split_once('/')?;
    (first.contains(['.', ':']) || first.eq_ignore_ascii_case("localhost")).then_some(first)
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
    let Some(registry) = registry_component(name) else {
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
    if Template::mentions_reference(value) {
        errors.push(ValidationError::new(
            codes::VARIABLE_NOT_ALLOWED,
            path,
            "container paths cannot reference variables",
        ));
    } else if !value.starts_with('/')
        || value == "/"
        || value.ends_with('/')
        || value.len() > 4096
        || value.contains('\\')
        || value.chars().any(char::is_control)
        || components
            .into_iter()
            .any(|part| part.is_empty() || part.len() > 255 || part == ".." || part == ".")
    {
        errors.push(ValidationError::new(
            codes::MOUNT_TARGET_UNSAFE,
            path,
            "mount target must be an absolute, normalized container path below root",
        ));
    }
}

/// Docker stage names: a leading letter, then letters, digits, `-`, `_`, or `.`.
fn valid_build_target(value: &str) -> bool {
    value.len() <= MAX_IDENTIFIER_BYTES
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphabetic)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}

/// Whether an environment key or build argument name is a POSIX-style identifier
/// (`[A-Za-z_][A-Za-z0-9_]*`).
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
    values: &[Template],
    path: &str,
    excessive_code: &str,
    errors: &mut Vec<ValidationError>,
) {
    if values.len() > MAX_PROCESS_ELEMENTS {
        errors.push(ValidationError::new(
            excessive_code,
            path,
            format!("the list must contain at most {MAX_PROCESS_ELEMENTS} elements"),
        ));
    }
    for (index, value) in values.iter().enumerate() {
        let Some(value) = value.as_literal() else {
            continue;
        };
        if value.contains('\0') {
            errors.push(ValidationError::new(
                codes::PROCESS_ARGUMENT_INVALID,
                format!("{path}[{index}]"),
                "container process arguments cannot contain NUL",
            ));
        }
        if value.len() > MAX_PROCESS_ELEMENT_BYTES {
            errors.push(ValidationError::new(
                excessive_code,
                format!("{path}[{index}]"),
                format!("each element must be at most {MAX_PROCESS_ELEMENT_BYTES} bytes"),
            ));
        }
    }
}
