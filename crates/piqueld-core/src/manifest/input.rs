//! Public manifest input and export shapes, before semantic validation.

use super::variables::{RenderTarget, Template, Typed};
use super::{APPLICATION_API_VERSION, APPLICATION_KIND, Rollback, Visibility};
use crate::names::string_enum;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use utoipa::ToSchema;

/// An application: what it runs, and how its environments and previews
/// deploy it.
// Strict public request and export shape: unknown fields are errors.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ApplicationManifest {
    /// Version of the manifest format.
    pub api_version: String,
    /// Kind of resource the manifest describes.
    pub kind: String,
    /// The application's identity.
    pub metadata: Metadata,
    /// What the application runs, and how it deploys.
    pub spec: ApplicationSpec,
}

impl ApplicationManifest {
    /// Standalone JSON Schema (draft 7) for manifest files, published for
    /// editor completion and inline errors. It covers structure only: names,
    /// limits, and cross-references are checked by [`super::parse_toml`].
    #[must_use]
    pub fn json_schema() -> Value {
        let mut schema = crate::json_schema::standalone::<Self>("piqueld application manifest");
        schema["properties"]["api_version"]["const"] = APPLICATION_API_VERSION.into();
        schema["properties"]["kind"]["const"] = APPLICATION_KIND.into();
        // utoipa expresses `deny_unknown_fields` on structs only, not on the
        // variants of these internally tagged enums. serde enforces it on
        // their struct variants only: a unit variant, whose sole property is
        // its tag, ignores other fields.
        for name in [
            Source::name(),
            Build::name(),
            HealthCheck::name(),
            SecretGenerator::name(),
            crate::sync::RepositorySync::name(),
        ] {
            for variant in schema["definitions"][name.as_ref()]["oneOf"]
                .as_array_mut()
                .into_iter()
                .flatten()
                .filter(|variant| {
                    variant["properties"]
                        .as_object()
                        .is_some_and(|p| p.len() > 1)
                })
            {
                variant["additionalProperties"] = false.into();
            }
        }
        // utoipa bounds unsigned integers from below only; typed fields'
        // literals are their first alternative.
        for (name, maximum) in [
            ("Typed_u16", u64::from(u16::MAX)),
            ("Typed_u32", u64::from(u32::MAX)),
            ("Typed_u64", u64::MAX),
        ] {
            schema["definitions"][name]["oneOf"][0]["maximum"] = maximum.into();
        }
        schema
    }
}

/// The application's identity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Metadata {
    /// Name of the application. A manifest with another name applies to
    /// another application, so rename one with `piquelctl app rename`.
    pub name: String,
}

/// What the application runs, and how its environments and previews deploy
/// it.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct ApplicationSpec {
    /// The Git repository each deployment reads the manifest from. Without
    /// it, deployments use the saved manifest. A manifest read from the
    /// repository can't change it: its own `spec.manifest` is ignored.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest: Option<RepositoryManifest>,
    /// The application's services, at most 64.
    pub services: Vec<Service>,
    /// Named volumes the services mount, at most 64.
    pub volumes: Vec<Volume>,
    /// HTTP routes to the services, at most 64. They take effect when an
    /// environment deploys them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub routes: Vec<Route>,
    /// Secrets piqueld generates a value for in each environment, at most 64.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<SecretDeclaration>,
    /// Containers that run to completion during each deployment, such as
    /// database migrations, at most 16. They run in the order listed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub jobs: Vec<Job>,
    /// Services of this application that other applications may reach, at
    /// most 64.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub exports: Vec<super::links::Export>,
    /// Services of other applications this application reaches, at most 64.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub imports: Vec<super::links::Import>,
    /// Default value of the variable `<variable>`, referenced as
    /// `${{ vars.<variable> }}`. An environment's or the previews' own value
    /// replaces it. Variable names start with a letter or `_`, use letters,
    /// digits, `_` or `-`, and are at most 63 bytes. Each table of variables
    /// holds at most 128.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub variables: BTreeMap<String, Variable>,
    /// Declares the environment `<environment>` and how it deploys, for at
    /// most 64 environments.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub environments: BTreeMap<String, EnvironmentConfig>,
    /// Settings shared by every preview.
    #[serde(skip_serializing_if = "PreviewConfig::is_default")]
    pub previews: PreviewConfig,
}

impl ApplicationSpec {
    /// Whether saving this spec over an application whose repository
    /// connection is `current` lets sync deploy what no deployment chose:
    /// it turns sync on, or declares environments opted in (`sync = true`)
    /// while it syncs. That takes the permission to deploy.
    #[must_use]
    pub fn deploys_on_push(&self, current: Option<&RepositoryManifest>) -> bool {
        self.manifest
            .as_ref()
            .is_some_and(RepositoryManifest::syncs)
            && (!current.is_some_and(RepositoryManifest::syncs)
                || self
                    .environments
                    .values()
                    .any(|config| config.sync == Some(true)))
    }

    /// Every secret mount of the services and jobs.
    pub fn secret_mounts(&self) -> impl Iterator<Item = &SecretMount> {
        self.services
            .iter()
            .flat_map(|service| &service.secrets)
            .chain(self.jobs.iter().flat_map(|job| &job.secrets))
    }

    /// Disconnects the manifest repository. "self" sources keep building from
    /// the former repository, and declared environments return to the saved
    /// manifest, which has no branches to follow.
    pub fn disconnect(&mut self) {
        let Some(previous) = self.manifest.take() else {
            return;
        };
        for service in &mut self.services {
            if let Source::Git { repository, .. } = &mut service.source {
                repository.resolve_manifest(&previous.repository);
            }
        }
        for config in self.environments.values_mut() {
            config.branch = None;
            config.commit = None;
            config.sync = None;
        }
    }

    /// Sets `environment`'s visibility ceiling. Blocks declare environments,
    /// so one is kept once it exists, and only a restriction adds one.
    pub fn set_environment_visibility(&mut self, environment: &str, visibility: Visibility) {
        match self.environments.get_mut(environment) {
            Some(config) => config.visibility = visibility,
            None if visibility.is_public() => {}
            None => {
                self.environments.insert(
                    environment.to_owned(),
                    EnvironmentConfig {
                        visibility,
                        ..EnvironmentConfig::default()
                    },
                );
            }
        }
    }

    /// Sets `environment`'s rollback, or returns it to the default with
    /// `None`. Like visibility, only a setting adds a block.
    pub fn set_environment_rollback(&mut self, environment: &str, rollback: Option<Rollback>) {
        match self.environments.get_mut(environment) {
            Some(config) => config.rollback = rollback,
            None if rollback.is_none() => {}
            None => {
                self.environments.insert(
                    environment.to_owned(),
                    EnvironmentConfig {
                        rollback,
                        ..EnvironmentConfig::default()
                    },
                );
            }
        }
    }

    /// The rollback `environment` gets, `promoted` or not (see
    /// [`Rollback::effective`]).
    #[must_use]
    pub fn rollback(&self, environment: &str, promoted: bool) -> Rollback {
        Rollback::effective(
            self.environments
                .get(environment)
                .and_then(|config| config.rollback),
            promoted,
        )
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

/// Declares an environment and where it deploys from. Without
/// `promote_from`, the environment builds its own manifest: the saved one, or
/// a branch of the `spec.manifest` repository. A block that sets none of
/// `branch`, `commit`, `promote_from` and `sync` leaves an existing
/// environment's source as it is, unless the previous manifest set one of
/// them: removing them returns the environment to the default source.
// `super::declaration` converges environments to these blocks.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct EnvironmentConfig {
    /// Branch of the manifest repository the environment deploys. By
    /// default, the branch `spec.manifest` names. Needs `spec.manifest`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    /// Full commit the environment deploys instead of its branch's head.
    /// Without `branch`, by default the commit `spec.manifest` pins. Needs
    /// `spec.manifest`, and excludes `sync`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Another environment whose releases this one receives with
    /// `piquelctl env promote`, instead of building. It excludes `branch`,
    /// `commit` and `sync`, and can't lead back to this environment.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub promote_from: Option<String>,
    /// Whether pushes to the environment's branch deploy it while
    /// `spec.manifest.sync` is on. Off when unset. `true` needs
    /// `spec.manifest`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync: Option<bool>,
    /// The strictest visibility the environment's routes get. `public`
    /// restricts nothing; `private` keeps every route on the tailnet.
    #[serde(skip_serializing_if = "Visibility::is_public")]
    pub visibility: Visibility,
    /// What happens when a rollout fails: `automatic` restores the
    /// environment's last successful deployment, `manual` leaves it degraded
    /// until someone acts. By default `automatic` for promoted environments
    /// and `manual` for the others.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rollback: Option<Rollback>,
    /// The environment's value of the variable `<variable>`. It replaces the
    /// `[spec.variables]` default, or declares a variable without one.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub variables: BTreeMap<String, Variable>,
}

impl EnvironmentConfig {
    /// Whether it sets where the environment deploys from: `branch`,
    /// `commit`, `promote_from` or `sync`. Blocks without, such as ones only
    /// setting variables, declare that the environment exists.
    #[must_use]
    pub const fn sets_source(&self) -> bool {
        self.branch.is_some()
            || self.commit.is_some()
            || self.promote_from.is_some()
            || self.sync.is_some()
    }
}

impl Default for EnvironmentConfig {
    fn default() -> Self {
        Self {
            branch: None,
            commit: None,
            promote_from: None,
            sync: None,
            visibility: Visibility::Public,
            rollback: None,
            variables: BTreeMap::new(),
        }
    }
}

/// Settings shared by every preview.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PreviewConfig {
    /// The strictest visibility preview routes get. `private` keeps every
    /// route on the tailnet; `public` restricts nothing.
    pub visibility: Visibility,
    /// The previews' value of the variable `<variable>`. It replaces the
    /// `[spec.variables]` default, or declares a variable without one.
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

/// A variable's value: a string, an integer or a boolean. A string may
/// reference system variables, such as `${{ env.name }}`, but not other
/// variables.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, ToSchema)]
#[serde(untagged)]
pub enum Variable {
    /// A boolean.
    Boolean(bool),
    /// A signed 64-bit integer.
    Integer(i64),
    /// Text of at most 65,536 bytes, which may reference system variables.
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

/// The Git repository and file each deployment reads the manifest from.
// Independent of the services' sources.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct RepositoryManifest {
    /// The repository holding the manifest, and the branch environments
    /// deploy by default.
    pub repository: GitRepository,
    /// Path of the TOML or JSON manifest file, relative to the repository
    /// root.
    pub path: String,
    /// Whether pushes to the repository deploy environments and previews.
    #[schema(default = crate::sync::RepositorySync::default)]
    #[serde(default, skip_serializing_if = "crate::sync::RepositorySync::is_off")]
    pub sync: crate::sync::RepositorySync,
}

impl RepositoryManifest {
    /// Whether pushes deploy anything: the repository is polled or sends
    /// webhooks.
    #[must_use]
    pub const fn syncs(&self) -> bool {
        !self.sync.is_off()
    }
}

/// A service: containers running one image, kept at their replica count.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Service {
    /// Name of the service, unique in the application. The application's
    /// other services reach it by this name.
    pub name: String,
    /// Where the service's image comes from: a prebuilt image, or a Git
    /// build.
    pub source: Source,
    /// Number of containers, from 1 to 100.
    #[serde(default = "default_replicas")]
    #[schema(default = default_replicas)]
    pub replicas: Typed<u16>,
    /// The environment variable `<name>` of the service's containers. Names
    /// use letters, digits and `_`, don't start with a digit, and are at most
    /// 255 bytes. Values are at most 65,536 bytes. At most 256 variables;
    /// `PIQUELD_INGRESS_PROXIES` is reserved.
    #[serde(default)]
    pub environment: BTreeMap<String, Template>,
    /// Command that replaces the image's entrypoint, at most 128 elements of
    /// 4,096 bytes each. When empty, the image's entrypoint runs.
    #[serde(default)]
    pub command: Vec<Template>,
    /// Arguments that replace the image's default arguments, at most 128
    /// elements of 4,096 bytes each.
    #[serde(default)]
    pub arguments: Vec<Template>,
    /// Volumes mounted into the containers, at most 32.
    #[serde(default)]
    pub mounts: Vec<Mount>,
    /// Secrets mounted as files, at most 64.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<SecretMount>,
    /// Read-only files mounted into the containers, at most 32. Their
    /// contents are never rendered with variables. An application's files
    /// total at most 2 MiB.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<super::ServiceFile>,
    /// How Docker checks that a container is healthy. Rollouts and the
    /// services depending on this one wait until it passes.
    pub healthcheck: Option<HealthCheck>,
    /// CPU and memory limits of each container.
    pub resources: Option<ResourceLimits>,
    /// Services of this application that must be running and healthy before
    /// this one rolls out, each listed once.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<String>,
    /// How Docker replaces the service's containers on each deployment.
    #[serde(default, skip_serializing_if = "super::Rollout::is_default")]
    pub rollout: super::Rollout,
    /// Whether the service receives the daemon's traces, metrics and logs,
    /// on a socket in `/run/piqueld/telemetry`, once its environment is
    /// granted telemetry. At most one service of an application receives
    /// telemetry, with one replica.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[schema(default = false)]
    pub receives_telemetry: bool,
}

/// Serde default for `Service::replicas`.
fn default_replicas() -> Typed<u16> {
    Typed::Literal(1)
}

/// A container that runs to completion during each deployment, such as a
/// database migration.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Job {
    /// Name of the job, unique in the application.
    pub name: String,
    /// Service whose image, environment, secrets, volumes, resource limits
    /// and network the job reuses. The job also waits for the service's
    /// `depends_on`.
    pub service: String,
    /// Command the job runs, instead of its service's command and arguments.
    /// It can't be empty.
    pub command: Vec<Template>,
    /// The environment variable `<name>` of the job's container only, in
    /// addition to its service's. A name the service also sets is an error.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub environment: BTreeMap<String, Template>,
    /// Secrets only the job's container mounts, in addition to its service's.
    /// A target the service already uses is an error.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub secrets: Vec<SecretMount>,
    /// When the job runs. `before-rollout` runs it after every source is
    /// prepared, before the services roll out.
    pub run: JobRun,
    /// Seconds the job may run before the deployment fails, from 1 to 86,400.
    #[serde(default = "default_job_timeout")]
    #[schema(default = default_job_timeout, maximum = 4_294_967_295_u32)]
    pub timeout_seconds: u32,
    /// How the job mounts its service's volumes. `read-only` mounts every
    /// volume read-only, so the job runs beside the services. `exclusive`
    /// keeps the writable volumes writable, and stops every service mounting
    /// them while the job runs.
    #[serde(default, skip_serializing_if = "JobVolumeAccess::is_default")]
    #[schema(default = JobVolumeAccess::default)]
    pub volume_access: JobVolumeAccess,
    /// Whether the job's service stops before the job runs, once the job's
    /// startup dependencies are ready, so its previous version never runs
    /// against what the job changes. The service starts again with the
    /// rollout, or with the previous deployment if a job fails.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[schema(default = false)]
    pub stop_service: bool,
}

string_enum! {
    #[serde]
    /// How a job may use the volumes its service mounts.
    #[derive(Default)]
    pub enum JobVolumeAccess {
        /// Every volume is mounted read-only, so the job can run beside the
        /// services that still write them.
        #[default]
        ReadOnly => "read-only",
        /// The service's writable volumes stay writable. Every service mounting
        /// one of them stops before the job runs and restarts with the rollout,
        /// or with the previous deployment if a job fails.
        Exclusive => "exclusive",
    }
}

impl JobVolumeAccess {
    /// Serde helper: omit the default access when serializing.
    #[must_use]
    pub fn is_default(&self) -> bool {
        *self == Self::default()
    }
}

impl std::str::FromStr for JobVolumeAccess {
    type Err = String;

    /// Parses the manifest spelling, `read-only` or `exclusive`.
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value).ok_or_else(|| format!("unknown job volume access {value:?}"))
    }
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

/// Where a service's image comes from.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum Source {
    /// Pull a prebuilt image from a registry.
    Image {
        /// Image reference, such as `ghcr.io/example/notes:1.4.0`, with an
        /// optional tag or digest. It can't contain a URL scheme or
        /// credentials.
        image: Template,
    },
    /// Build the image from a Git repository.
    Git {
        /// Repository to build: `"self"` for the manifest's own repository,
        /// at the commit the manifest was read from, or a table naming
        /// another one. `"self"` needs `spec.manifest`.
        repository: SourceRepository,
        /// How to build the image.
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
    /// Another repository, and the revision to build.
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

/// A Git repository and the revision to check out. Git uses the host's
/// credentials.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct GitRepository {
    /// Clone URL or local path of the repository. It can't contain
    /// credentials.
    pub url: String,
    /// Branch whose head is checked out when no commit is set.
    pub branch: String,
    /// Full commit hash to check out instead of the branch's head.
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

/// How to build a Git source's image.
// Build backends are selected independently from the source.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum Build {
    /// Build the image with Docker Buildx, from a Dockerfile.
    Docker {
        /// Path of the Dockerfile, relative to the repository root.
        dockerfile: Template,
        /// Build context directory, relative to the repository root.
        #[serde(default = "default_build_context")]
        #[schema(default = default_build_context)]
        context: Template,
        /// The value of the build argument `<name>`, passed with
        /// `--build-arg`. Docker records it in the image's metadata, so pass
        /// sensitive values as secrets instead. Build arguments total at most
        /// 256 KiB.
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        args: BTreeMap<String, Template>,
        /// Stage of a multi-stage Dockerfile to build. By default, the final
        /// stage.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<Template>,
    },
}

/// Serde default for the Docker build context: the repository root.
fn default_build_context() -> Template {
    ".".into()
}

/// A named volume the services mount. Its data is kept across deployments,
/// and when the application is deleted. A preview's volumes are deleted with
/// the preview.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Volume {
    /// Name of the volume, unique in the application.
    pub name: String,
}

/// A volume mounted into a service's containers.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Mount {
    /// Name of a volume in `spec.volumes`.
    pub volume: String,
    /// Absolute path of the mount in the container, unique in the service.
    /// It can't be `/run/piqueld/probe` or `/run/piqueld/telemetry`, or a
    /// path inside them.
    pub target: String,
    /// Whether the containers mount the volume read-only. A writable mount
    /// makes rollouts stop the old container before starting its
    /// replacement.
    #[serde(default)]
    #[schema(default = false)]
    pub read_only: bool,
}

/// How Docker checks that a service's containers are healthy.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum HealthCheck {
    /// Send an HTTP `GET` to a port of the container. A 2xx or 3xx status
    /// passes. The image needs nothing for it.
    Http {
        /// Container port to request, from 1 to 65535.
        port: Typed<u16>,
        /// Absolute HTTP path to request.
        #[serde(default = "default_health_path")]
        #[schema(default = default_health_path)]
        path: Template,
        /// Seconds between checks, from 1 to 3,600.
        #[serde(default = "default_interval")]
        #[schema(default = default_interval)]
        interval_seconds: Typed<u32>,
        /// Seconds a check may take before it fails, from 1 to the interval.
        #[serde(default = "default_timeout")]
        #[schema(default = default_timeout)]
        timeout_seconds: Typed<u32>,
        /// Seconds after a container starts during which failed checks don't
        /// count, up to 3,600.
        #[serde(
            default = "default_start_period",
            skip_serializing_if = "is_default_start_period"
        )]
        #[schema(default = default_start_period)]
        start_period_seconds: Typed<u32>,
        /// Consecutive failed checks that make a container unhealthy, from 1
        /// to 100.
        #[serde(
            default = "default_retries",
            skip_serializing_if = "is_default_retries"
        )]
        #[schema(default = default_retries)]
        retries: Typed<u32>,
    },
    /// Run a command in the container, without a shell. Exit code 0 passes.
    Command {
        /// Command and arguments to run. It can't be empty.
        command: Vec<Template>,
        /// Seconds between checks, from 1 to 3,600.
        #[serde(default = "default_interval")]
        #[schema(default = default_interval)]
        interval_seconds: Typed<u32>,
        /// Seconds a check may take before it fails, from 1 to the interval.
        #[serde(default = "default_timeout")]
        #[schema(default = default_timeout)]
        timeout_seconds: Typed<u32>,
        /// Seconds after a container starts during which failed checks don't
        /// count, up to 3,600.
        #[serde(
            default = "default_start_period",
            skip_serializing_if = "is_default_start_period"
        )]
        #[schema(default = default_start_period)]
        start_period_seconds: Typed<u32>,
        /// Consecutive failed checks that make a container unhealthy, from 1
        /// to 100.
        #[serde(
            default = "default_retries",
            skip_serializing_if = "is_default_retries"
        )]
        #[schema(default = default_retries)]
        retries: Typed<u32>,
    },
}

impl HealthCheck {
    /// The check's timing settings, whichever variant declared them.
    #[must_use]
    pub const fn timing(&self) -> super::HealthTiming<&Typed<u32>> {
        let (Self::Http {
            interval_seconds,
            timeout_seconds,
            start_period_seconds,
            retries,
            ..
        }
        | Self::Command {
            interval_seconds,
            timeout_seconds,
            start_period_seconds,
            retries,
            ..
        }) = self;
        super::HealthTiming {
            interval_seconds,
            timeout_seconds,
            start_period_seconds,
            retries,
        }
    }

    /// The check's timing settings, to edit them.
    pub const fn timing_mut(&mut self) -> super::HealthTiming<&mut Typed<u32>> {
        let (Self::Http {
            interval_seconds,
            timeout_seconds,
            start_period_seconds,
            retries,
            ..
        }
        | Self::Command {
            interval_seconds,
            timeout_seconds,
            start_period_seconds,
            retries,
            ..
        }) = self;
        super::HealthTiming {
            interval_seconds,
            timeout_seconds,
            start_period_seconds,
            retries,
        }
    }
}

/// Serde default for the HTTP health-check path.
fn default_health_path() -> Template {
    "/health".into()
}

/// Serde default health-check start period, in seconds: none. Defaults are
/// not serialized, so checks written before it existed keep their spec hash.
fn default_start_period() -> Typed<u32> {
    Typed::Literal(super::ValidatedHealthCheck::DEFAULT_START_PERIOD_SECONDS)
}

/// Whether `value` is [`default_start_period`].
fn is_default_start_period(value: &Typed<u32>) -> bool {
    *value == default_start_period()
}

/// Serde default health-check retries, likewise not serialized.
fn default_retries() -> Typed<u32> {
    Typed::Literal(super::ValidatedHealthCheck::DEFAULT_RETRIES)
}

/// Whether `value` is [`default_retries`].
fn is_default_retries(value: &Typed<u32>) -> bool {
    *value == default_retries()
}

/// Serde default health-check interval, in seconds.
fn default_interval() -> Typed<u32> {
    Typed::Literal(super::ValidatedHealthCheck::DEFAULT_INTERVAL_SECONDS)
}

/// Serde default health-check timeout, in seconds.
fn default_timeout() -> Typed<u32> {
    Typed::Literal(super::ValidatedHealthCheck::DEFAULT_TIMEOUT_SECONDS)
}

/// CPU and memory limits of each of a service's containers. Set at least
/// one.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ResourceLimits {
    /// CPU limit in thousandths of a CPU, from 1 to 1,048,576: `500` is half
    /// a CPU.
    pub cpu_millis: Option<Typed<u32>>,
    /// Memory limit in bytes, from 1 to 9,223,372,036,854,775,807.
    pub memory_bytes: Option<Typed<u64>>,
}

/// A secret mounted as a file. A name `spec.secrets` declares mounts the
/// environment's generated value; any other name mounts a value from the
/// application's secret store.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Ord, PartialOrd, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SecretMount {
    /// Name of the secret. It may reference variables, so each environment
    /// can mount its own stored secret, e.g. `${{ vars.stripe_key }}`.
    pub name: Template,
    /// Absolute path of the file under `/run/secrets/`, unique in the
    /// service.
    pub target: String,
}

/// A secret whose value piqueld generates for each environment when one of
/// its deployments first needs it. Later deployments and manifest edits keep
/// that value; `piquelctl env secret regenerate` replaces it. A value is never
/// shared with another environment.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Ord, PartialOrd, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct SecretDeclaration {
    /// Name of the secret, unique in the application. It can't reference
    /// variables.
    pub name: String,
    /// How piqueld generates the value.
    pub generate: SecretGenerator,
}

/// How piqueld generates a secret's value.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Ord, PartialOrd, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
pub enum SecretGenerator {
    /// Random bytes from the operating system, encoded as text.
    Random {
        /// Number of random bytes before encoding, from 16 to 512.
        #[schema(maximum = 65_535)]
        bytes: u16,
        /// Text encoding of the bytes: lowercase `hex`, or unpadded
        /// `base64url`.
        #[serde(default)]
        #[schema(default = SecretEncoding::default)]
        encoding: SecretEncoding,
    },
    /// An RSA private key, in PKCS#8 PEM.
    Rsa {
        /// Size of the key in bits: 2048, 3072 or 4096.
        #[schema(maximum = 65_535)]
        bits: u16,
    },
}

string_enum! {
    #[serde]
    /// Text encoding of generated random bytes.
    #[derive(Default, Ord, PartialOrd)]
    pub enum SecretEncoding {
        /// Lowercase hexadecimal.
        #[default]
        Hex => "hex",
        /// URL-safe base64 without padding.
        Base64url => "base64url",
    }
}

/// An HTTP route: requests for a hostname go to a service, or get a
/// redirect. A route sets either `service` and `port`, or `redirect`.
// Validated independently of whether ingress is enabled.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Route {
    /// Hostname the route serves, such as `notes.example.com`, unique in the
    /// application. A wildcard such as `*.example.com` serves every name one
    /// label below `example.com` that no exact route claims.
    pub hostname: Template,
    // Declared after `hostname`, so routes stay ordered by hostname.
    /// Name that selects the route in every environment and preview, e.g.
    /// `piquelctl env url --route web`. It is unique in the application and
    /// can't reference variables.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Who may connect: `public` for the internet, or `private` for the
    /// tailnet only. The environment's or the previews' visibility can make
    /// it stricter, never looser.
    #[serde(default)]
    #[schema(default = Visibility::default)]
    pub visibility: Visibility,
    /// Service of this application that receives the requests. Needs `port`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service: Option<String>,
    /// Port the service serves HTTP on, from 1 to 65535. Needs `service`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[schema(minimum = 1, maximum = 65_535)]
    pub port: Option<u16>,
    /// Redirect the gateway answers with, instead of a service.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect: Option<Redirect>,
}

impl Route {
    /// An unnamed route proxying `hostname` to a service's internal HTTP port.
    #[must_use]
    pub fn service(hostname: String, visibility: Visibility, service: String, port: u16) -> Self {
        Self {
            hostname: hostname.into(),
            name: None,
            visibility,
            service: Some(service),
            port: Some(port),
            redirect: None,
        }
    }

    /// An unnamed route redirecting `hostname` without reaching a service.
    #[must_use]
    pub fn redirect(hostname: String, visibility: Visibility, redirect: Redirect) -> Self {
        Self {
            hostname: hostname.into(),
            name: None,
            visibility,
            service: None,
            port: None,
            redirect: Some(redirect),
        }
    }

    /// This route with `name`, or unnamed.
    #[must_use]
    pub fn named(self, name: Option<String>) -> Self {
        Self { name, ..self }
    }
}

/// A redirect the gateway answers with.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Redirect {
    /// Absolute `http` or `https` URL to redirect to, with a lowercase
    /// hostname and no query or fragment. It can't target a hostname the
    /// route serves.
    pub to: Template,
    /// HTTP status of the redirect.
    #[serde(default = "default_redirect_status")]
    #[schema(value_type = super::RedirectStatus, inline, default = default_redirect_status)]
    pub status: u16,
    /// Whether to append the request's path and query to `to`.
    #[serde(default = "default_preserve_path")]
    #[schema(default = default_preserve_path)]
    pub preserve_path: bool,
}

fn default_redirect_status() -> u16 {
    super::RedirectStatus::PermanentRedirect.into()
}

fn default_preserve_path() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::{ApplicationSpec, EnvironmentConfig, GitRepository, RepositoryManifest, Rollback};
    use crate::sync::RepositorySync;

    #[test]
    fn rollback_is_automatic_by_default_only_for_promoted_environments() {
        let mut spec = ApplicationSpec::default();
        assert_eq!(spec.rollback("production", true), Rollback::Automatic);
        assert_eq!(spec.rollback("production", false), Rollback::Manual);
        // Returning to the default adds no block.
        spec.set_environment_rollback("production", None);
        assert!(spec.environments.is_empty());
        spec.set_environment_rollback("production", Some(Rollback::Manual));
        assert_eq!(spec.rollback("production", true), Rollback::Manual);
        spec.set_environment_rollback("staging", Some(Rollback::Automatic));
        assert_eq!(spec.rollback("staging", false), Rollback::Automatic);
        // An existing block is kept, back on the default.
        spec.set_environment_rollback("production", None);
        assert_eq!(
            spec.environments["production"],
            EnvironmentConfig::default()
        );
        assert_eq!(spec.rollback("production", true), Rollback::Automatic);
    }

    #[test]
    fn a_save_deploys_on_push_only_by_turning_sync_on_or_opting_environments_in() {
        let connection = |sync| RepositoryManifest {
            repository: GitRepository {
                url: "https://example.com/app.git".into(),
                branch: "main".into(),
                commit: None,
            },
            path: "piqueld.toml".into(),
            sync,
        };
        let (on, off) = (
            connection(RepositorySync::Webhook),
            connection(RepositorySync::Off),
        );
        let spec = |connection: &RepositoryManifest, opted_in: bool| ApplicationSpec {
            manifest: Some(connection.clone()),
            environments: [(
                "production".to_owned(),
                EnvironmentConfig {
                    sync: opted_in.then_some(true),
                    ..EnvironmentConfig::default()
                },
            )]
            .into(),
            ..ApplicationSpec::default()
        };
        assert!(spec(&on, false).deploys_on_push(None));
        assert!(spec(&on, false).deploys_on_push(Some(&off)));
        assert!(!spec(&on, false).deploys_on_push(Some(&on)));
        assert!(spec(&on, true).deploys_on_push(Some(&on)));
        assert!(!spec(&off, true).deploys_on_push(Some(&on)));
    }
}
