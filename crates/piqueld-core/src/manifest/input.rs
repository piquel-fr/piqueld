//! Public manifest input and export shapes, before semantic validation.

use super::variables::{RenderTarget, Template, Typed};
use super::{APPLICATION_API_VERSION, APPLICATION_KIND, RouteAccess, Visibility};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use utoipa::{PartialSchema, ToSchema};

/// Strict public application manifest request and export shape.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ApplicationManifest {
    /// API version string.
    pub api_version: String,
    /// Resource kind string.
    pub kind: String,
    /// User-provided metadata.
    pub metadata: Metadata,
    /// Desired application resources.
    pub spec: ApplicationSpec,
}

impl ApplicationManifest {
    /// Standalone JSON Schema (draft 7) for manifest files, published for
    /// editor completion and inline errors. It covers structure only: names,
    /// limits, and cross-references are checked by [`super::parse_toml`].
    #[must_use]
    pub fn json_schema() -> Value {
        // Rewrites utoipa's `OpenAPI` component refs to draft-7 `definitions` refs.
        fn relocate_refs(value: &mut Value) {
            match value {
                Value::Object(object) => {
                    if let Some(Value::String(reference)) = object.get_mut("$ref")
                        && let Some(name) = reference.strip_prefix("#/components/schemas/")
                    {
                        *reference = format!("#/definitions/{name}");
                    }
                    for value in object.values_mut() {
                        relocate_refs(value);
                    }
                }
                Value::Array(values) => {
                    for value in values {
                        relocate_refs(value);
                    }
                }
                _ => {}
            }
        }

        let mut definitions = Vec::new();
        Self::schemas(&mut definitions);
        let mut schema = json!(Self::schema());
        schema["$schema"] = "http://json-schema.org/draft-07/schema#".into();
        schema["title"] = "piqueld application manifest".into();
        schema["definitions"] = json!(definitions.into_iter().collect::<BTreeMap<_, _>>());
        schema["properties"]["api_version"]["const"] = APPLICATION_API_VERSION.into();
        schema["properties"]["kind"]["const"] = APPLICATION_KIND.into();
        relocate_refs(&mut schema);
        schema
    }
}

/// User-provided application metadata.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Metadata {
    /// User-facing application name.
    pub name: String,
}

/// User-provided application resource lists.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ApplicationSpec {
    /// Optional repository that supplies this application's manifest on Deploy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest: Option<RepositoryManifest>,
    /// Declared services.
    pub services: Vec<Service>,
    /// Declared named volumes.
    pub volumes: Vec<Volume>,
    /// Exact-hostname HTTP routes, activated on deployment.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<Route>,
    /// Secrets whose values piqueld generates once per environment.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<SecretDeclaration>,
    /// One-shot jobs, run in declared order at their deployment point.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub jobs: Vec<Job>,
    /// Default values of variables referenced as `${{ vars.<name> }}`, used by
    /// environments that set no value of their own.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub variables: BTreeMap<String, Variable>,
    /// Configuration for each environment, selected by environment name.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub environments: BTreeMap<String, EnvironmentConfig>,
    /// Configuration shared by every preview.
    #[serde(skip_serializing_if = "PreviewConfig::is_default")]
    pub previews: PreviewConfig,
}

impl ApplicationSpec {
    /// Sets `environment`'s visibility ceiling, dropping its block when left
    /// at its defaults, since such a block configures nothing.
    pub fn set_environment_visibility(&mut self, environment: &str, visibility: Visibility) {
        let config = self.environments.entry(environment.to_owned()).or_default();
        config.visibility = visibility;
        if *config == EnvironmentConfig::default() {
            self.environments.remove(environment);
        }
    }

    /// Caps every route at `target`'s visibility ceiling, as rendering for it
    /// does. Unconfigured environments restrict nothing; previews are private
    /// unless `[spec.previews] visibility` says otherwise.
    pub(super) fn cap_visibility(&mut self, target: &RenderTarget) {
        let ceiling = match target {
            RenderTarget::Environment(name) => self
                .environments
                .get(name.as_str())
                .map_or(Visibility::Public, |config| config.visibility),
            RenderTarget::Preview(_) => self.previews.visibility,
        };
        for route in &mut self.routes {
            route.visibility = route.visibility.capped(ceiling);
        }
    }

    /// The table of values that override the defaults for `target`, with its
    /// path: the environment's block, if any, or `[spec.previews.variables]`.
    pub(super) fn overrides(
        &self,
        target: &RenderTarget,
    ) -> Option<(String, &BTreeMap<String, Variable>)> {
        match target {
            RenderTarget::Environment(name) => self.environments.get(name.as_str()).map(|config| {
                (
                    format!("spec.environments.{name}.variables"),
                    &config.variables,
                )
            }),
            RenderTarget::Preview(_) => Some((
                "spec.previews.variables".to_owned(),
                &self.previews.variables,
            )),
        }
    }
}

/// Configuration of one environment, `[spec.environments.<name>]`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct EnvironmentConfig {
    /// The strictest visibility this environment's routes get. The default,
    /// `public`, restricts nothing; `private` keeps every route on the tailnet.
    #[serde(skip_serializing_if = "Visibility::is_public")]
    pub visibility: Visibility,
    /// This environment's values: each overrides the `[spec.variables]` default
    /// of the same name, or declares a variable without a default.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub variables: BTreeMap<String, Variable>,
}

impl Default for EnvironmentConfig {
    fn default() -> Self {
        Self {
            visibility: Visibility::Public,
            variables: BTreeMap::new(),
        }
    }
}

/// Configuration shared by previews, `[spec.previews]`.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PreviewConfig {
    /// The strictest visibility preview routes get; `private` by default, so
    /// previews publish routes only with an explicit `public`.
    pub visibility: Visibility,
    /// Values for every preview: each overrides the `[spec.variables]`
    /// default of the same name, or declares a variable without a default.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub variables: BTreeMap<String, Variable>,
}

impl PreviewConfig {
    /// Whether every setting has its default, so exports can omit the block.
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

/// A declared variable value: a string, integer, or boolean. Strings may
/// reference system variables, but not other variables.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, ToSchema)]
#[serde(untagged)]
pub enum Variable {
    /// A boolean.
    Boolean(bool),
    /// A signed 64-bit integer.
    Integer(i64),
    /// Text, which may reference system variables.
    String(Template),
}

impl<'de> Deserialize<'de> for Variable {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match Value::deserialize(deserializer)? {
            Value::Bool(value) => Ok(Self::Boolean(value)),
            Value::String(value) => Ok(Self::String(value.into())),
            Value::Number(number) if number.is_i64() => {
                Ok(Self::Integer(number.as_i64().unwrap_or_default()))
            }
            _ => Err(serde::de::Error::custom(
                "variables must be strings, integers, or booleans",
            )),
        }
    }
}

impl Variable {
    /// Parses a value typed into a form or command line: `true`, `false`, and
    /// integers keep their type, text in double quotes is that text, and
    /// anything else is text.
    ///
    /// ```text
    /// "3" -> Integer(3)    "\"3\"" -> String("3")    "piquel.fr" -> String("piquel.fr")
    /// ```
    #[must_use]
    pub fn from_text(text: &str) -> Self {
        if let Some(quoted) = text
            .strip_prefix('"')
            .and_then(|text| text.strip_suffix('"'))
        {
            return Self::String(quoted.into());
        }
        match (text.parse(), text.parse()) {
            (Ok(value), _) => Self::Boolean(value),
            (_, Ok(value)) => Self::Integer(value),
            _ => Self::String(text.into()),
        }
    }

    /// The text [`Self::from_text`] parses back into this value, quoting text
    /// that would otherwise read as a boolean, an integer, quoted text, or,
    /// when blank, no value.
    #[must_use]
    pub fn to_text(&self) -> String {
        match self {
            Self::String(text)
                if text.as_str().trim().is_empty() || Self::from_text(text.as_str()) != *self =>
            {
                format!("\"{text}\"")
            }
            value => value.to_string(),
        }
    }
}

impl std::fmt::Display for Variable {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Boolean(value) => value.fmt(formatter),
            Self::Integer(value) => value.fmt(formatter),
            Self::String(value) => value.fmt(formatter),
        }
    }
}

/// Independently selects the manifest used by a manual deployment.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RepositoryManifest {
    /// Repository and revision containing the manifest.
    pub repository: GitRepository,
    /// Exact TOML or JSON file path relative to the repository root.
    pub path: String,
}

/// User-declared application service.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Service {
    /// Logical service name.
    pub name: String,
    /// Explicit image or build source.
    pub source: Source,
    /// Desired replica count.
    #[serde(default = "default_replicas")]
    pub replicas: Typed<u16>,
    /// Environment variables keyed by name.
    #[serde(default)]
    pub environment: BTreeMap<String, Template>,
    /// Container entrypoint command.
    #[serde(default)]
    pub command: Vec<Template>,
    /// Arguments passed to the command.
    #[serde(default)]
    pub arguments: Vec<Template>,
    /// Persistent volume mounts.
    #[serde(default)]
    pub mounts: Vec<Mount>,
    /// Secrets mounted as files.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<SecretMount>,
    /// Optional container health check.
    pub healthcheck: Option<HealthCheck>,
    /// Optional CPU and memory limits.
    pub resources: Option<ResourceLimits>,
    /// Services in this application that must be healthy before this one rolls out.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    /// Optional rollout order and monitor window.
    #[serde(default, skip_serializing_if = "super::Rollout::is_default")]
    pub rollout: super::Rollout,
}

/// Serde default for `Service::replicas`.
fn default_replicas() -> Typed<u16> {
    Typed::Literal(1)
}

/// A container that runs to completion at a defined point in each deployment.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Job {
    /// Logical job name.
    pub name: String,
    /// Service whose prepared image, environment, secrets, and mounts the job reuses.
    pub service: String,
    /// Command replacing the service's command and arguments.
    pub command: Vec<Template>,
    /// Deployment point at which the job runs.
    pub run: JobRun,
    /// Seconds the job may run before the deployment fails.
    #[serde(default = "default_job_timeout")]
    pub timeout_seconds: u32,
}

/// Deployment point at which a job runs.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "kebab-case")]
pub enum JobRun {
    /// After sources and startup dependencies are ready, before other services roll out.
    BeforeRollout,
}

impl Job {
    /// Timeout applied when a job declares none.
    pub const DEFAULT_TIMEOUT_SECONDS: u32 = 300;
    /// Longest timeout a job may declare.
    pub const MAX_TIMEOUT_SECONDS: u32 = 86_400;
    /// Most jobs one application may declare.
    pub const MAX_PER_APPLICATION: usize = 16;
}

/// Serde default for `Job::timeout_seconds`.
fn default_job_timeout() -> u32 {
    Job::DEFAULT_TIMEOUT_SECONDS
}

/// The exhaustive set of deployable service sources.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum Source {
    /// Pull a prebuilt image from a registry.
    Image {
        /// Image reference.
        image: Template,
    },
    /// Build a checked-out Git revision.
    Git {
        /// Repository and revision to resolve.
        repository: SourceRepository,
        /// Explicit build instructions.
        build: Build,
    },
}

/// The repository a Git build source checks out.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(untagged)]
pub enum SourceRepository {
    /// `"self"`: the application's manifest repository, at the exact commit its
    /// manifest was read from, so one deployment builds from one revision.
    Manifest(ManifestRepository),
    /// An independently resolved repository and revision.
    Git(GitRepository),
}

/// Shows the repository and its requested revision.
impl std::fmt::Display for SourceRepository {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Manifest(_) => formatter.write_str("self (manifest repository)"),
            Self::Git(repository) => write!(
                formatter,
                "{} ({})",
                repository.url,
                repository.commit.as_ref().unwrap_or(&repository.branch)
            ),
        }
    }
}

impl SourceRepository {
    /// Replaces `"self"` with `manifest`, the repository it refers to.
    pub fn resolve_manifest(&mut self, manifest: &GitRepository) {
        if let Self::Manifest(_) = self {
            *self = Self::Git(manifest.clone());
        }
    }
}

/// The literal `"self"`, selecting the manifest's own repository.
// A unit variant of the untagged `SourceRepository` would match `null`, not
// `"self"`. Serde's per-variant `untagged` would avoid this type, but utoipa
// only supports `untagged` on the whole enum and would misdescribe Git sources.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub enum ManifestRepository {
    /// The manifest's own repository.
    #[serde(rename = "self")]
    Manifest,
}

/// Overrides the manifest repository revision for one deployment.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "lowercase", deny_unknown_fields)]
pub enum ManifestRevision {
    /// Resolve this branch head.
    Branch(String),
    /// Use this full commit hash.
    Commit(String),
}

/// Git checkout configuration. Credentials come from the host's Git configuration.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct GitRepository {
    /// Git clone URL or local repository path.
    pub url: String,
    /// Branch to fetch when no commit is pinned.
    pub branch: String,
    /// Optional full commit hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

impl GitRepository {
    /// Returns this repository at another branch head or commit.
    #[must_use]
    pub fn at(&self, revision: &ManifestRevision) -> Self {
        let mut repository = self.clone();
        match revision {
            ManifestRevision::Branch(branch) => {
                repository.branch.clone_from(branch);
                repository.commit = None;
            }
            ManifestRevision::Commit(commit) => repository.commit = Some(commit.clone()),
        }
        repository
    }
}

/// Explicit build backend, extensible independently from source selection.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum Build {
    /// Build a local container image using Docker.
    Docker {
        /// Dockerfile path relative to the repository root.
        dockerfile: Template,
        /// Build context relative to the repository root.
        #[serde(default = "default_build_context")]
        context: Template,
        /// Values passed as `--build-arg`. They are recorded in image
        /// metadata, so sensitive values belong in secrets instead.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        args: BTreeMap<String, Template>,
        /// Multi-stage build target; the final stage when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<Template>,
    },
}

/// Serde default for the Docker build context: the repository root.
fn default_build_context() -> Template {
    ".".into()
}

/// User-declared named volume.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Volume {
    /// Logical volume name.
    pub name: String,
}

/// A persistent volume mount in a service.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Mount {
    /// Referenced logical volume name.
    pub volume: String,
    /// Container target path.
    pub target: String,
    /// Whether the mount is read-only.
    #[serde(default)]
    pub read_only: bool,
}

/// User-declared container health check.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum HealthCheck {
    /// HTTP health endpoint check.
    Http {
        /// Container port to probe.
        port: Typed<u16>,
        /// HTTP path to probe.
        #[serde(default = "default_health_path")]
        path: Template,
        /// Probe interval in seconds.
        #[serde(default = "default_interval")]
        interval_seconds: Typed<u32>,
        /// Probe timeout in seconds.
        #[serde(default = "default_timeout")]
        timeout_seconds: Typed<u32>,
    },
    /// Executable command health check.
    Command {
        /// Command and arguments to execute.
        command: Vec<Template>,
        /// Probe interval in seconds.
        #[serde(default = "default_interval")]
        interval_seconds: Typed<u32>,
        /// Probe timeout in seconds.
        #[serde(default = "default_timeout")]
        timeout_seconds: Typed<u32>,
    },
}

/// Serde default for the HTTP health-check path.
fn default_health_path() -> Template {
    "/health".into()
}

/// Serde default health-check interval, in seconds.
fn default_interval() -> Typed<u32> {
    Typed::Literal(super::ValidatedHealthCheck::DEFAULT_INTERVAL_SECONDS)
}

/// Serde default health-check timeout, in seconds.
fn default_timeout() -> Typed<u32> {
    Typed::Literal(super::ValidatedHealthCheck::DEFAULT_TIMEOUT_SECONDS)
}

/// Optional CPU and memory limits for a service.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ResourceLimits {
    /// CPU limit in millicores.
    pub cpu_millis: Option<Typed<u32>>,
    /// Memory limit in bytes.
    pub memory_bytes: Option<Typed<u64>>,
}

/// A secret exposed only as a container file. The name selects the
/// environment's generated secret when `spec.secrets` declares it, and the
/// application's store of manually set secrets otherwise.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Ord, PartialOrd, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SecretMount {
    /// Logical secret name. It may reference variables, so each environment
    /// can mount its own stored secret, e.g. `${{ vars.stripe_key }}`.
    pub name: Template,
    /// Absolute normalized destination under /run/secrets.
    pub target: String,
}

/// A secret whose value piqueld generates for each environment when one of
/// its deployments first needs it. A generated value is never replaced, and
/// never shared with another environment.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Ord, PartialOrd, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SecretDeclaration {
    /// Logical secret name; it cannot reference variables.
    pub name: String,
    /// How the value is generated.
    pub generate: SecretGenerator,
}

/// The exhaustive set of secret value generators.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Ord, PartialOrd, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum SecretGenerator {
    /// Random bytes from the operating system, encoded as text.
    Random {
        /// Number of random bytes before encoding.
        bytes: u16,
        /// Text encoding of the random bytes.
        #[serde(default)]
        encoding: SecretEncoding,
    },
    /// RSA private key in PKCS#8 PEM.
    Rsa {
        /// Modulus size in bits.
        bits: u16,
    },
}

/// Text encoding of generated random bytes.
#[derive(
    Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Ord, PartialOrd, Serialize, ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum SecretEncoding {
    /// Lowercase hexadecimal.
    #[default]
    Hex,
    /// URL-safe base64 without padding.
    Base64url,
}

/// HTTP route input, validated independently of ingress enablement.
/// A route sets either `service` and `port`, or `redirect`.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Route {
    /// Exact public DNS hostname.
    pub hostname: Template,
    /// Who may connect; `private` (tailnet only) by default. Its environment's
    /// ceiling can make it stricter, never looser.
    #[serde(default)]
    pub visibility: Visibility,
    /// Passes the connecting device's tailnet identity to the backend in
    /// `Piqueld-*` request headers. Requires the route's own `visibility` to
    /// be `private`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub identity: bool,
    /// Logical service in this application.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    /// Internal HTTP backend port.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(minimum = 1, maximum = 65_535)]
    pub port: Option<u16>,
    /// Redirect answered by the gateway instead of a service.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect: Option<Redirect>,
}

impl Route {
    /// A route proxying `hostname` to a service's internal HTTP port.
    #[must_use]
    pub fn service(hostname: String, access: RouteAccess, service: String, port: u16) -> Self {
        Self {
            hostname: hostname.into(),
            visibility: access.visibility(),
            identity: access.identity(),
            service: Some(service),
            port: Some(port),
            redirect: None,
        }
    }

    /// A route redirecting `hostname` without reaching a service.
    #[must_use]
    pub fn redirect(hostname: String, access: RouteAccess, redirect: Redirect) -> Self {
        Self {
            hostname: hostname.into(),
            visibility: access.visibility(),
            identity: access.identity(),
            service: None,
            port: None,
            redirect: Some(redirect),
        }
    }
}

/// Redirect route input.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Redirect {
    /// Absolute `http` or `https` destination URL.
    pub to: Template,
    /// 301, 302, 303, 307, or 308.
    #[serde(default = "default_redirect_status")]
    #[schema(minimum = 301, maximum = 308)]
    pub status: u16,
    /// Appends the request path and query to `to`.
    #[serde(default = "default_preserve_path")]
    pub preserve_path: bool,
}

fn default_redirect_status() -> u16 {
    super::RedirectStatus::PermanentRedirect.into()
}

fn default_preserve_path() -> bool {
    true
}
