//! Manifest variables: `${{ namespace.name }}` references, the values they
//! name, and rendering a manifest for one environment.
//!
//! Text fields hold a [`Template`] and typed fields a [`Typed`] value, so
//! references stay unresolved in manifest input. Rendering replaces every
//! reference with its value; only rendered input converts into the validated
//! domain model, so unresolved references never reach a
//! [`super::NormalizedApplication`].

use super::{ApplicationSpec, RepositoryManifest, ValidationError, input::Variable};
use crate::{EnvironmentName, codes};
use serde::{Deserialize, Deserializer, Serialize, de::DeserializeOwned};
use std::{collections::BTreeMap, fmt, str::FromStr};
use utoipa::ToSchema;

const OPEN: &str = "${{";
const ESCAPED_OPEN: &str = "$${{";
const CLOSE: &str = "}}";
/// Most text one rendering may produce, twice the largest manifest request.
/// A value can repeat a large variable many times, so expansion is bounded
/// as it happens rather than by validating the result.
const MAX_RENDERED_BYTES: usize = 4 * 1024 * 1024;

/// Text that may contain `${{ namespace.name }}` references, with optional
/// whitespace inside the braces. `$${{` writes a literal `${{`; `${VAR}` and
/// `$VAR` are ordinary text, left for shells and applications to expand.
///
/// ```text
/// "${{ vars.domain }}"       -> the value of `domain`
/// "api.${{vars.domain}}"     -> "api." followed by the value of `domain`
/// "$${{ vars.domain }}"      -> "${{ vars.domain }}", literally
/// ```
#[derive(
    Clone, Debug, Default, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, ToSchema,
)]
#[serde(transparent)]
pub struct Template(String);

/// A parsed piece of a [`Template`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Segment {
    /// Literal text, with escapes applied.
    Text(String),
    /// A reference, resolved when the manifest is rendered.
    Reference(Reference),
}

impl Template {
    /// The template for `text` itself, escaping any `${{` it contains.
    #[must_use]
    pub fn literal(text: &str) -> Self {
        Self(text.replace(OPEN, ESCAPED_OPEN))
    }

    /// The template as written.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Splits the template into literal text and references.
    ///
    /// # Errors
    ///
    /// Returns the first malformed, reserved, or unknown reference.
    pub fn segments(&self) -> Result<Vec<Segment>, TemplateError> {
        let mut segments = Vec::new();
        let mut text = String::new();
        let mut rest = self.0.as_str();
        while let Some(index) = rest.find('$') {
            text.push_str(&rest[..index]);
            rest = &rest[index..];
            if let Some(after) = rest.strip_prefix(ESCAPED_OPEN) {
                text.push_str(OPEN);
                rest = after;
            } else if let Some(after) = rest.strip_prefix(OPEN) {
                let end = after.find(CLOSE).ok_or(TemplateError::Unterminated)?;
                if !text.is_empty() {
                    segments.push(Segment::Text(std::mem::take(&mut text)));
                }
                segments.push(Segment::Reference(Reference::parse(after[..end].trim())?));
                rest = &after[end + CLOSE.len()..];
            } else {
                text.push('$');
                rest = &rest[1..];
            }
        }
        text.push_str(rest);
        if !text.is_empty() {
            segments.push(Segment::Text(text));
        }
        Ok(segments)
    }

    /// The text this template stands for, or `None` when it references
    /// variables or is malformed.
    #[must_use]
    pub fn as_literal(&self) -> Option<String> {
        if !self.0.contains(OPEN) {
            return Some(self.0.clone());
        }
        self.segments()
            .ok()?
            .into_iter()
            .try_fold(String::new(), |mut text, segment| match segment {
                Segment::Text(part) => {
                    text.push_str(&part);
                    Some(text)
                }
                Segment::Reference(_) => None,
            })
    }

    /// Whether the template contains `${{` at all, escaped or not. Fields that
    /// cannot reference variables reject such text outright.
    #[must_use]
    pub fn mentions_reference(text: &str) -> bool {
        text.contains(OPEN)
    }
}

impl From<&str> for Template {
    fn from(source: &str) -> Self {
        Self(source.into())
    }
}

impl From<String> for Template {
    fn from(source: String) -> Self {
        Self(source)
    }
}

/// Compares the template as written.
impl PartialEq<str> for Template {
    fn eq(&self, other: &str) -> bool {
        self.0 == other
    }
}

/// Compares the template as written.
impl PartialEq<&str> for Template {
    fn eq(&self, other: &&str) -> bool {
        self.0 == *other
    }
}

impl fmt::Display for Template {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A system variable, set by piqueld for every rendering.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum SystemVariable {
    /// `app.name`: the application name.
    AppName,
    /// `env.name`: the environment name.
    EnvName,
    /// `env.slug`: a DNS-safe identifier of the environment.
    EnvSlug,
    /// `git.branch`: the branch the manifest was read from.
    GitBranch,
    /// `git.sha`: the commit the manifest was read from.
    GitSha,
    /// `deployment.id`: the deployment being captured.
    DeploymentId,
}

impl SystemVariable {
    /// Every system variable.
    pub const ALL: [Self; 6] = [
        Self::AppName,
        Self::EnvName,
        Self::EnvSlug,
        Self::GitBranch,
        Self::GitSha,
        Self::DeploymentId,
    ];

    /// The reference spelling, e.g. `env.name`.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::AppName => "app.name",
            Self::EnvName => "env.name",
            Self::EnvSlug => "env.slug",
            Self::GitBranch => "git.branch",
            Self::GitSha => "git.sha",
            Self::DeploymentId => "deployment.id",
        }
    }

    /// Whether only repository-backed manifests have this variable.
    const fn needs_repository(self) -> bool {
        matches!(self, Self::GitBranch | Self::GitSha)
    }
}

/// What a `${{ }}` reference names.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum Reference {
    /// `vars.<name>`: a variable declared in the manifest.
    Variable(String),
    /// A system variable.
    System(SystemVariable),
}

impl Reference {
    /// Parses the trimmed text between `${{` and `}}`.
    fn parse(inner: &str) -> Result<Self, TemplateError> {
        let Some((namespace, name)) = inner
            .split_once('.')
            .filter(|(namespace, name)| valid_identifier(namespace) && valid_identifier(name))
        else {
            return Err(TemplateError::Malformed(echo(inner)));
        };
        match namespace {
            "vars" => Ok(Self::Variable(name.into())),
            "secrets" => Err(TemplateError::Reserved(echo(inner))),
            "app" | "env" | "git" | "deployment" => SystemVariable::ALL
                .into_iter()
                .find(|variable| variable.as_str() == inner)
                .map(Self::System)
                .ok_or_else(|| TemplateError::UnknownSystemVariable(echo(inner))),
            _ => Err(TemplateError::UnknownNamespace(echo(namespace))),
        }
    }
}

impl Reference {
    /// The error for `git.*` unless the manifest is repository-backed.
    fn unavailable(&self, repository: bool) -> Option<(&'static str, String)> {
        matches!(self, Self::System(variable) if variable.needs_repository() && !repository).then(
            || {
                (
                    codes::VARIABLE_UNAVAILABLE,
                    format!("{self} is only set when spec.manifest is configured"),
                )
            },
        )
    }
}

impl fmt::Display for Reference {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Variable(name) => write!(formatter, "vars.{name}"),
            Self::System(variable) => formatter.write_str(variable.as_str()),
        }
    }
}

/// Whether `value` is a variable or namespace name: an ASCII letter or `_`,
/// then letters, digits, `_`, or `-`, at most 63 bytes.
#[must_use]
pub fn valid_identifier(value: &str) -> bool {
    value.len() <= 63
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

/// Quotes user text in messages, truncated to 64 characters.
fn echo(text: &str) -> String {
    text.chars().take(64).collect()
}

/// A user-supplied map key as a path component, truncated to the 255-byte
/// identifier bound so diagnostics stay small for oversized keys.
fn path_key(key: &str) -> &str {
    let mut end = key.len().min(255);
    while !key.is_char_boundary(end) {
        end -= 1;
    }
    &key[..end]
}

/// A malformed or unsupported reference.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum TemplateError {
    /// `${{` without a closing `}}`.
    #[error("`${{{{` must be closed by `}}}}`; write `$${{{{` for a literal `${{{{`")]
    Unterminated,
    /// The braces do not hold a `namespace.name` reference.
    #[error("`{0}` is not a `namespace.name` reference")]
    Malformed(String),
    /// `secrets.*`, reserved for secret references.
    #[error("`{0}` is reserved for secret references, which are not supported yet")]
    Reserved(String),
    /// A namespace other than `vars`, `app`, `env`, `git`, or `deployment`.
    #[error("unknown namespace `{0}`; references use vars, app, env, git, or deployment")]
    UnknownNamespace(String),
    /// A system namespace without this variable.
    #[error("unknown system variable `{0}`")]
    UnknownSystemVariable(String),
}

impl TemplateError {
    /// The stable validation code for this error.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Unterminated | Self::Malformed(_) => codes::TEMPLATE_INVALID,
            Self::Reserved(_) => codes::VARIABLE_RESERVED,
            Self::UnknownNamespace(_) => codes::VARIABLE_NAMESPACE_UNKNOWN,
            Self::UnknownSystemVariable(_) => codes::VARIABLE_UNKNOWN,
        }
    }
}

/// A typed value: a literal, or a template rendered for each environment.
///
/// A template that is exactly one reference yields the variable's own value;
/// any other template yields text. The result must then decode as `T`, so
/// strings are never parsed into numbers.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
#[serde(untagged)]
pub enum Typed<T> {
    /// A value written directly.
    Literal(T),
    /// A value rendered from variables.
    #[schema(value_type = TypedTemplate)]
    Template(Template),
}

/// Schema of a template in a typed field: text containing `${{`.
#[derive(ToSchema)]
#[schema(pattern = r"\$\{\{")]
#[expect(dead_code, reason = "describes the wire shape of `Typed::Template`")]
struct TypedTemplate(String);

impl<T> Typed<T> {
    /// The literal value, or `None` while it references variables.
    #[must_use]
    pub const fn literal(&self) -> Option<&T> {
        match self {
            Self::Literal(value) => Some(value),
            Self::Template(_) => None,
        }
    }
}

impl<T> From<T> for Typed<T> {
    fn from(value: T) -> Self {
        Self::Literal(value)
    }
}

impl<T: fmt::Display> fmt::Display for Typed<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Literal(value) => value.fmt(formatter),
            Self::Template(template) => template.fmt(formatter),
        }
    }
}

/// Parses text containing `${{` as a template and anything else as `T`, e.g.
/// a CLI argument or a form field.
impl<T: FromStr> FromStr for Typed<T> {
    type Err = T::Err;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.contains(OPEN) {
            Ok(Self::Template(value.into()))
        } else {
            value.parse().map(Self::Literal)
        }
    }
}

impl<'de, T: DeserializeOwned> Deserialize<'de> for Typed<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match serde_json::Value::deserialize(deserializer)? {
            serde_json::Value::String(text) if text.contains(OPEN) => {
                Ok(Self::Template(Template(text)))
            }
            value => serde_json::from_value(value)
                .map(Self::Literal)
                .map_err(serde::de::Error::custom),
        }
    }
}

/// A variable's value for one environment.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(untagged)]
pub enum VariableValue {
    /// A boolean.
    Boolean(bool),
    /// A signed 64-bit integer.
    Integer(i64),
    /// Text.
    String(String),
}

impl VariableValue {
    /// The native value a whole-value reference yields in a typed field.
    fn json(&self) -> serde_json::Value {
        match self {
            Self::Boolean(value) => (*value).into(),
            Self::Integer(value) => (*value).into(),
            Self::String(value) => value.as_str().into(),
        }
    }
}

/// Text form, used when a value is interpolated into a string.
impl fmt::Display for VariableValue {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Boolean(value) => value.fmt(formatter),
            Self::Integer(value) => value.fmt(formatter),
            Self::String(value) => formatter.write_str(value),
        }
    }
}

/// What a manifest is rendered for.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RenderContext {
    /// The environment: its `[spec.environments.<name>]` block, `env.name`,
    /// and `env.slug`.
    pub environment: EnvironmentName,
    /// The manifest revision of a repository-backed deployment.
    pub git: Option<GitRevision>,
    /// The deployment being captured, for `deployment.id`.
    pub deployment: Option<String>,
}

/// The repository revision a manifest was read from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitRevision {
    /// `git.branch`.
    pub branch: String,
    /// `git.sha`.
    pub sha: String,
}

/// Placeholder `deployment.id` when previewing, before a deployment exists.
pub const PREVIEW_DEPLOYMENT_ID: &str = "preview";

impl RenderContext {
    /// Renders for a deployment of `environment`.
    #[must_use]
    pub fn deployment(environment: EnvironmentName, deployment: String) -> Self {
        Self {
            environment,
            git: None,
            deployment: Some(deployment),
        }
    }

    /// Renders a preview of `environment` before anything is deployed:
    /// `deployment.id` is [`PREVIEW_DEPLOYMENT_ID`], and a manifest fetched
    /// from `repository` uses its branch and pinned commit, or 40 zeros.
    #[must_use]
    pub fn preview(environment: EnvironmentName, repository: Option<&RepositoryManifest>) -> Self {
        Self {
            environment,
            git: repository.map(|manifest| GitRevision {
                branch: manifest.repository.branch.clone(),
                sha: manifest
                    .repository
                    .commit
                    .clone()
                    .unwrap_or_else(|| "0".repeat(40)),
            }),
            deployment: Some(PREVIEW_DEPLOYMENT_ID.into()),
        }
    }

    /// Renders saved configuration outside any deployment, e.g. to reserve
    /// the hostnames an environment will serve.
    #[must_use]
    pub const fn saved(environment: EnvironmentName) -> Self {
        Self {
            environment,
            git: None,
            deployment: None,
        }
    }
}

/// A value that may reference variables, visited with its field path.
pub(super) enum Slot<'a> {
    /// A text field.
    Text(&'a mut Template),
    /// A typed field.
    Typed(&'a mut dyn TypedSlot),
}

/// A type a [`Typed`] field holds, described in type errors instead of
/// serde's message, which repeats the whole rendered value.
pub(super) trait Expected: DeserializeOwned {
    /// What the field accepts, e.g. `an integer from 0 to 65535`.
    const EXPECTED: &'static str;
}

macro_rules! expected {
    ($($type:ty => $expected:literal),* $(,)?) => {
        $(impl Expected for $type {
            const EXPECTED: &'static str = $expected;
        })*
    };
}

expected! {
    u16 => "an integer from 0 to 65535",
    u32 => "an integer from 0 to 4294967295",
    u64 => "an integer from 0 to 18446744073709551615",
    super::RolloutOrder => "`start-first` or `stop-first`",
}

/// Type-erased access to a [`Typed`] field.
pub(super) trait TypedSlot {
    /// The template, unless the value is already literal.
    fn template(&self) -> Option<&Template>;
    /// Replaces the value with `value` decoded as the field's type, or
    /// returns what the field accepts.
    fn set(&mut self, value: serde_json::Value) -> Result<(), &'static str>;
}

impl<T: Expected> TypedSlot for Typed<T> {
    fn template(&self) -> Option<&Template> {
        match self {
            Self::Literal(_) => None,
            Self::Template(template) => Some(template),
        }
    }

    fn set(&mut self, value: serde_json::Value) -> Result<(), &'static str> {
        *self = Self::Literal(serde_json::from_value(value).map_err(|_| T::EXPECTED)?);
        Ok(())
    }
}

impl Slot<'_> {
    /// The template, unless the value is a typed literal.
    pub(super) fn template(&self) -> Option<&Template> {
        match self {
            Self::Text(template) => Some(template),
            Self::Typed(typed) => typed.template(),
        }
    }
}

/// Visits each element of a text list, with its indexed path.
fn texts<'a>(
    base: &str,
    values: impl IntoIterator<Item = &'a mut Template>,
    visit: &mut impl FnMut(&str, Slot<'_>),
) {
    for (index, value) in values.into_iter().enumerate() {
        visit(&format!("{base}[{index}]"), Slot::Text(value));
    }
}

impl super::HealthCheck {
    /// Visits the check's values; `base` is its field path.
    fn visit_values(&mut self, base: &str, visit: &mut impl FnMut(&str, Slot<'_>)) {
        match self {
            Self::Http {
                port,
                path,
                interval_seconds,
                timeout_seconds,
            } => {
                visit(&format!("{base}.port"), Slot::Typed(port));
                visit(&format!("{base}.path"), Slot::Text(path));
                visit(
                    &format!("{base}.interval_seconds"),
                    Slot::Typed(interval_seconds),
                );
                visit(
                    &format!("{base}.timeout_seconds"),
                    Slot::Typed(timeout_seconds),
                );
            }
            Self::Command {
                command,
                interval_seconds,
                timeout_seconds,
            } => {
                texts(&format!("{base}.command"), command, visit);
                visit(
                    &format!("{base}.interval_seconds"),
                    Slot::Typed(interval_seconds),
                );
                visit(
                    &format!("{base}.timeout_seconds"),
                    Slot::Typed(timeout_seconds),
                );
            }
        }
    }
}

impl ApplicationSpec {
    /// Visits every value that may reference variables, with its field path.
    /// This is the one list of fields that accept `${{ }}`; any other string
    /// field rejects it.
    pub(super) fn visit_values(&mut self, visit: &mut impl FnMut(&str, Slot<'_>)) {
        for (index, service) in self.services.iter_mut().enumerate() {
            let base = format!("spec.services[{index}]");
            match &mut service.source {
                super::Source::Image { image } => {
                    visit(&format!("{base}.source.image"), Slot::Text(image));
                }
                super::Source::Git {
                    build:
                        super::Build::Docker {
                            dockerfile,
                            context,
                            args,
                            target,
                        },
                    ..
                } => {
                    let build = format!("{base}.source.build");
                    visit(&format!("{build}.dockerfile"), Slot::Text(dockerfile));
                    visit(&format!("{build}.context"), Slot::Text(context));
                    for (key, value) in args {
                        visit(
                            &format!("{build}.args.{}", path_key(key)),
                            Slot::Text(value),
                        );
                    }
                    if let Some(target) = target {
                        visit(&format!("{build}.target"), Slot::Text(target));
                    }
                }
            }
            visit(
                &format!("{base}.replicas"),
                Slot::Typed(&mut service.replicas),
            );
            for (key, value) in &mut service.environment {
                visit(
                    &format!("{base}.environment.{}", path_key(key)),
                    Slot::Text(value),
                );
            }
            texts(&format!("{base}.command"), &mut service.command, visit);
            texts(&format!("{base}.arguments"), &mut service.arguments, visit);
            for (secret_index, secret) in service.secrets.iter_mut().enumerate() {
                visit(
                    &format!("{base}.secrets[{secret_index}].name"),
                    Slot::Text(&mut secret.name),
                );
            }
            if let Some(healthcheck) = &mut service.healthcheck {
                healthcheck.visit_values(&format!("{base}.healthcheck"), visit);
            }
            if let Some(resources) = &mut service.resources {
                if let Some(cpu) = &mut resources.cpu_millis {
                    visit(&format!("{base}.resources.cpu_millis"), Slot::Typed(cpu));
                }
                if let Some(memory) = &mut resources.memory_bytes {
                    visit(
                        &format!("{base}.resources.memory_bytes"),
                        Slot::Typed(memory),
                    );
                }
            }
            if let Some(order) = &mut service.rollout.order {
                visit(&format!("{base}.rollout.order"), Slot::Typed(order));
            }
            if let Some(monitor) = &mut service.rollout.monitor_seconds {
                visit(
                    &format!("{base}.rollout.monitor_seconds"),
                    Slot::Typed(monitor),
                );
            }
        }
        for (index, route) in self.routes.iter_mut().enumerate() {
            visit(
                &format!("spec.routes[{index}].hostname"),
                Slot::Text(&mut route.hostname),
            );
            if let Some(redirect) = &mut route.redirect {
                visit(
                    &format!("spec.routes[{index}].redirect.to"),
                    Slot::Text(&mut redirect.to),
                );
            }
        }
        for (index, job) in self.jobs.iter_mut().enumerate() {
            texts(
                &format!("spec.jobs[{index}].command"),
                &mut job.command,
                visit,
            );
        }
    }

    /// Every declared variable value, keyed by path: the defaults, then each
    /// environment's overrides.
    fn declarations(&self) -> impl Iterator<Item = (String, &str, &Variable)> {
        self.variables
            .iter()
            .map(|(name, value)| {
                let path = format!("spec.variables.{}", path_key(name));
                (path, name.as_str(), value)
            })
            .chain(self.environments.iter().flat_map(|(environment, config)| {
                config.variables.iter().map(move |(name, value)| {
                    (
                        format!(
                            "spec.environments.{}.variables.{}",
                            path_key(environment),
                            path_key(name)
                        ),
                        name.as_str(),
                        value,
                    )
                })
            }))
    }

    /// Checks variable declarations and references without rendering:
    /// variable values reference only system variables, names and sizes are
    /// bounded, and every reference is well formed and declared.
    pub(super) fn check_variables(&mut self, errors: &mut Vec<ValidationError>) {
        const MAX_VARIABLES: usize = 128;
        const MAX_ENVIRONMENTS: usize = 64;
        const MAX_VALUE_BYTES: usize = 65_536;

        let repository = self.manifest.is_some();
        if self.variables.len() > MAX_VARIABLES
            || self
                .environments
                .values()
                .any(|config| config.variables.len() > MAX_VARIABLES)
        {
            error(
                errors,
                codes::VARIABLE_COUNT_EXCESSIVE,
                "spec.variables",
                &format!("each variables table may declare at most {MAX_VARIABLES} variables"),
            );
        }
        if self.environments.len() > MAX_ENVIRONMENTS {
            error(
                errors,
                codes::VARIABLE_COUNT_EXCESSIVE,
                "spec.environments",
                &format!("at most {MAX_ENVIRONMENTS} environments may be configured"),
            );
        }
        for name in self.environments.keys() {
            if let Err(source) = EnvironmentName::parse(name.as_str()) {
                error(
                    errors,
                    codes::NAME_INVALID,
                    "spec.environments",
                    &format!("[spec.environments.{}]: {source}", echo(name)),
                );
            }
        }
        for (path, name, value) in self.declarations() {
            if !valid_identifier(name) {
                error(
                    errors,
                    codes::VARIABLE_NAME_INVALID,
                    &path,
                    "variable names start with a letter or `_` and use letters, digits, `_`, or `-`, at most 63 bytes",
                );
            }
            let Variable::String(template) = value else {
                continue;
            };
            if template.as_str().len() > MAX_VALUE_BYTES {
                error(
                    errors,
                    codes::VARIABLE_VALUE_EXCESSIVE,
                    &path,
                    &format!("variable values must be at most {MAX_VALUE_BYTES} bytes"),
                );
            }
            match template.segments() {
                Err(source) => error(errors, source.code(), &path, &source.to_string()),
                // The first problem of each value is reported, bounding errors.
                Ok(segments) => {
                    let problem = segments.into_iter().find_map(|segment| match segment {
                        Segment::Reference(reference @ Reference::Variable(_)) => Some((
                            codes::VARIABLE_NOT_ALLOWED,
                            format!(
                                "variables may reference system variables, but not {reference}"
                            ),
                        )),
                        Segment::Reference(reference) => reference.unavailable(repository),
                        Segment::Text(_) => None,
                    });
                    if let Some((code, message)) = problem {
                        error(errors, code, &path, &message);
                    }
                }
            }
        }
        self.check_references(errors);
    }

    /// Checks every reference in a value: well formed, declared, and
    /// `git.*` only with a repository manifest.
    fn check_references(&mut self, errors: &mut Vec<ValidationError>) {
        let repository = self.manifest.is_some();
        let declared = self
            .declarations()
            .map(|(_, name, _)| name.to_owned())
            .collect::<std::collections::BTreeSet<_>>();
        self.visit_values(&mut |path, slot| {
            let Some(template) = slot.template() else {
                return;
            };
            match template.segments() {
                Err(source) => error(errors, source.code(), path, &source.to_string()),
                // The first problem of each value is reported, bounding errors.
                Ok(segments) => {
                    let problem = segments.into_iter().find_map(|segment| match segment {
                        Segment::Reference(Reference::Variable(name))
                            if !declared.contains(&name) =>
                        {
                            Some((
                                codes::VARIABLE_UNDECLARED,
                                format!(
                                    "vars.{name} is not declared in [spec.variables] or any [spec.environments.<name>.variables]"
                                ),
                            ))
                        }
                        Segment::Reference(reference) => reference.unavailable(repository),
                        Segment::Text(_) => None,
                    });
                    if let Some((code, message)) = problem {
                        error(errors, code, path, &message);
                    }
                }
            }
        });
    }

    /// Replaces every reference with its value for `context`, leaving only
    /// literal values, caps routes at the environment's visibility ceiling,
    /// and clears the variable declarations. Returns the
    /// values in scope, keyed by reference, for the deployment snapshot.
    pub(super) fn render(
        &mut self,
        application: &str,
        context: &RenderContext,
        errors: &mut Vec<ValidationError>,
    ) -> BTreeMap<String, VariableValue> {
        let scope = Scope::new(application, self, context, errors);
        self.visit_values(&mut |path, slot| scope.render(path, slot, errors));
        self.cap_visibility(context.environment.as_str());
        self.variables.clear();
        self.environments.clear();
        scope
            .values
            .into_iter()
            .map(|(reference, value)| (reference.to_string(), value))
            .collect()
    }

    /// Each declared variable's value in `context`, keyed by name, or `None`
    /// when it has none there.
    #[must_use]
    pub fn values(
        &self,
        application: &str,
        context: &RenderContext,
    ) -> BTreeMap<String, Option<VariableValue>> {
        let scope = Scope::new(application, self, context, &mut Vec::new());
        self.variables_for(context.environment.as_str())
            .into_keys()
            .map(|name| {
                let value = scope.values.get(&Reference::Variable(name.into())).cloned();
                (name.to_owned(), value)
            })
            .collect()
    }

    /// Each declared variable's value for `environment`: its override, else
    /// its default, else `None`. String values are shown as written.
    fn variables_for(&self, environment: &str) -> BTreeMap<&str, Option<&Variable>> {
        let overrides = self
            .environments
            .get(environment)
            .map(|config| &config.variables);
        self.declarations()
            .map(|(_, name, _)| {
                let value = overrides
                    .and_then(|overrides| overrides.get(name))
                    .or_else(|| self.variables.get(name));
                (name, value)
            })
            .collect()
    }
}

/// The values one rendering resolves references to.
struct Scope<'a> {
    context: &'a RenderContext,
    values: BTreeMap<Reference, VariableValue>,
    /// Bytes rendered so far, bounded by [`MAX_RENDERED_BYTES`].
    rendered: std::cell::Cell<usize>,
}

impl<'a> Scope<'a> {
    /// Collects the system variables `context` sets, then each variable's
    /// value for the environment, rendering string values with the system
    /// variables.
    fn new(
        application: &str,
        spec: &ApplicationSpec,
        context: &'a RenderContext,
        errors: &mut Vec<ValidationError>,
    ) -> Self {
        let environment = context.environment.as_str();
        let mut values = BTreeMap::new();
        let mut system = |variable, value: &str| {
            values.insert(
                Reference::System(variable),
                VariableValue::String(value.into()),
            );
        };
        system(SystemVariable::AppName, application);
        system(SystemVariable::EnvName, environment);
        system(SystemVariable::EnvSlug, environment);
        if let Some(git) = &context.git {
            system(SystemVariable::GitBranch, &git.branch);
            system(SystemVariable::GitSha, &git.sha);
        }
        if let Some(deployment) = &context.deployment {
            system(SystemVariable::DeploymentId, deployment);
        }
        let mut scope = Self {
            context,
            values,
            rendered: std::cell::Cell::new(0),
        };
        let overrides = spec.environments.get(environment);
        for (name, value) in spec.variables_for(environment) {
            let Some(value) = value else { continue };
            let path = if overrides.is_some_and(|config| config.variables.contains_key(name)) {
                format!("spec.environments.{environment}.variables.{name}")
            } else {
                format!("spec.variables.{name}")
            };
            let value = match value {
                Variable::Boolean(value) => Some(VariableValue::Boolean(*value)),
                Variable::Integer(value) => Some(VariableValue::Integer(*value)),
                Variable::String(template) => scope
                    .text(template, &path, errors)
                    .map(VariableValue::String),
            };
            if let Some(value) = value {
                scope.values.insert(Reference::Variable(name.into()), value);
            }
        }
        scope
    }

    /// The value of `reference`, or an error naming the environment, field,
    /// and variable.
    fn lookup(
        &self,
        reference: &Reference,
        path: &str,
        errors: &mut Vec<ValidationError>,
    ) -> Option<&VariableValue> {
        let value = self.values.get(reference);
        if value.is_none() {
            let environment = &self.context.environment;
            let (code, message) = match reference {
                Reference::Variable(_) => (
                    codes::VARIABLE_VALUE_MISSING,
                    format!("{reference} has no value for environment {environment}"),
                ),
                Reference::System(SystemVariable::DeploymentId) => (
                    codes::VARIABLE_UNAVAILABLE,
                    format!("{reference} is only set when deploying {environment}"),
                ),
                Reference::System(_) => (
                    codes::VARIABLE_UNAVAILABLE,
                    format!(
                        "{reference} is only set for repository-backed deployments of {environment}"
                    ),
                ),
            };
            error(errors, code, path, &message);
        }
        value
    }

    /// Renders `template` as text. Fails once everything rendered exceeds
    /// [`MAX_RENDERED_BYTES`], reporting it at the first field past the budget.
    fn text(
        &self,
        template: &Template,
        path: &str,
        errors: &mut Vec<ValidationError>,
    ) -> Option<String> {
        let segments = template
            .segments()
            .map_err(|source| error(errors, source.code(), path, &source.to_string()))
            .ok()?;
        let mut text = String::new();
        for segment in segments {
            let part = match segment {
                Segment::Text(part) => part,
                // The first missing value is reported, bounding errors.
                Segment::Reference(reference) => self.lookup(&reference, path, errors)?.to_string(),
            };
            let rendered = self.rendered.get();
            if rendered > MAX_RENDERED_BYTES {
                return None;
            }
            self.rendered.set(rendered + part.len());
            if rendered + part.len() > MAX_RENDERED_BYTES {
                error(
                    errors,
                    codes::VARIABLE_VALUE_EXCESSIVE,
                    path,
                    &format!(
                        "the manifest renders to more than {MAX_RENDERED_BYTES} bytes of text"
                    ),
                );
                return None;
            }
            text.push_str(&part);
        }
        Some(text)
    }

    /// Renders one visited value in place.
    fn render(&self, path: &str, slot: Slot<'_>, errors: &mut Vec<ValidationError>) {
        match slot {
            Slot::Text(template) => {
                if let Some(text) = self.text(template, path, errors) {
                    *template = Template::literal(&text);
                }
            }
            Slot::Typed(typed) => {
                let Some(template) = typed.template() else {
                    return;
                };
                let value = match template.segments().as_deref() {
                    Ok([Segment::Reference(reference)]) => self
                        .lookup(reference, path, errors)
                        .map(VariableValue::json),
                    _ => self.text(template, path, errors).map(Into::into),
                };
                if let Some(value) = value {
                    let shown = value.to_string();
                    if let Err(expected) = typed.set(value) {
                        error(
                            errors,
                            codes::VARIABLE_TYPE_INVALID,
                            path,
                            &format!("renders to {}, expected {expected}", echo(&shown)),
                        );
                    }
                }
            }
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
