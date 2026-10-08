//! Tests for manifest syntax, validation, rendering, and saved templates.

use super::variables::{Reference, Segment, SystemVariable};
use super::*;
use crate::{ApplicationId, EnvironmentName, codes};
use serde_json::{Value, json};

mod rendering;
mod syntax;
mod template;
mod validation;

/// A manifest with one web service, `spec` appended as written.
fn manifest(spec: &str) -> String {
    format!(
        r#"api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "notes"
{spec}
[[spec.services]]
name = "web"
replicas = "${{{{ vars.web_replicas }}}}"
[spec.services.source]
type = "image"
image = "nginx:${{{{ vars.tag }}}}"
[spec.services.environment]
GREETING = "hello ${{{{ env.name }}}}, keep ${{HOME}} and $USER, escape $${{{{ vars.tag }}}}"
"#
    )
}

const VARIABLES: &str = r#"
[spec.variables]
web_replicas = 1
tag = "stable"
domain = "piquel.fr"

[spec.environments.staging.variables]
domain = "staging.piquel.fr"

[spec.environments.production.variables]
web_replicas = 3
"#;

fn template(spec: &str) -> ApplicationTemplate {
    parse_template_toml(&manifest(spec))
        .unwrap()
        .normalize(id())
}

fn render(
    template: &ApplicationTemplate,
    environment_name: &str,
) -> Result<Rendering, ValidationErrors> {
    template.render(&RenderContext::deployment(
        environment(environment_name),
        "operation-1".into(),
    ))
}

fn codes_and_paths(errors: &ValidationErrors) -> Vec<(&str, &str)> {
    errors
        .0
        .iter()
        .map(|error| (error.code.as_str(), error.path.as_str()))
        .collect()
}

fn id() -> ApplicationId {
    ApplicationId::parse("app-notes-01").unwrap()
}
fn environment(name: &str) -> EnvironmentName {
    EnvironmentName::parse(name).unwrap()
}
fn saved() -> RenderContext {
    RenderContext::saved(environment("production"))
}
fn base() -> ApplicationManifest {
    toml::from_str(include_str!(
        "../../../tests/fixtures/manifests/prebuilt.toml"
    ))
    .unwrap()
}
fn normalize(manifest: ApplicationManifest) -> ApplicationTemplate {
    manifest.validate_template().unwrap().normalize(id())
}
fn repository() -> RepositoryManifest {
    RepositoryManifest {
        repository: GitRepository {
            url: "https://example.com/app.git".into(),
            branch: "main".into(),
            commit: None,
        },
        path: "piqueld.toml".into(),
    }
}

/// Both source/check variants and every optional reference-bearing field.
fn complete() -> ApplicationManifest {
    toml::from_str(
        r#"
api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "notes"
[spec.manifest]
path = "piqueld.toml"
[spec.manifest.repository]
url = "https://example.com/app.git"
branch = "main"
[[spec.services]]
name = "a-image"
replicas = 2
command = ["sh", "-c"]
arguments = ["echo", "ready"]
[spec.services.source]
type = "image"
image = "nginx:stable"
[spec.services.environment]
MODE = "production"
[spec.services.healthcheck]
type = "http"
port = 8080
path = "/health"
interval_seconds = 30
timeout_seconds = 5
[spec.services.resources]
cpu_millis = 500
memory_bytes = 67108864
[spec.services.rollout]
order = "stop-first"
monitor_seconds = 20
[[spec.services.secrets]]
name = "token"
target = "/run/secrets/token"
[[spec.services]]
name = "b-build"
command = ["server"]
[spec.services.source]
type = "git"
repository = "self"
[spec.services.source.build]
type = "docker"
dockerfile = "Dockerfile"
context = "."
target = "release"
[spec.services.source.build.args]
MODE = "release"
[spec.services.healthcheck]
type = "command"
command = ["test", "ready"]
interval_seconds = 20
timeout_seconds = 4
[[spec.routes]]
hostname = "notes.example.com"
service = "a-image"
port = 8080
[[spec.routes]]
hostname = "old.example.com"
[spec.routes.redirect]
to = "https://notes.example.com"
[[spec.jobs]]
name = "migrate"
service = "a-image"
command = ["echo", "migrate"]
run = "before-rollout"
"#,
    )
    .unwrap()
}

/// JSON pointer paired with its diagnostic spelling; values come from `complete`.
const FIELDS: &[(&str, &str)] = &[
    (
        "/spec/services/0/source/image",
        "spec.services[0].source.image",
    ),
    ("/spec/services/0/replicas", "spec.services[0].replicas"),
    (
        "/spec/services/0/environment/MODE",
        "spec.services[0].environment.MODE",
    ),
    ("/spec/services/0/command/0", "spec.services[0].command[0]"),
    ("/spec/services/0/command/1", "spec.services[0].command[1]"),
    (
        "/spec/services/0/arguments/0",
        "spec.services[0].arguments[0]",
    ),
    (
        "/spec/services/0/arguments/1",
        "spec.services[0].arguments[1]",
    ),
    (
        "/spec/services/0/healthcheck/port",
        "spec.services[0].healthcheck.port",
    ),
    (
        "/spec/services/0/healthcheck/path",
        "spec.services[0].healthcheck.path",
    ),
    (
        "/spec/services/0/healthcheck/interval_seconds",
        "spec.services[0].healthcheck.interval_seconds",
    ),
    (
        "/spec/services/0/healthcheck/timeout_seconds",
        "spec.services[0].healthcheck.timeout_seconds",
    ),
    (
        "/spec/services/0/resources/cpu_millis",
        "spec.services[0].resources.cpu_millis",
    ),
    (
        "/spec/services/0/resources/memory_bytes",
        "spec.services[0].resources.memory_bytes",
    ),
    (
        "/spec/services/0/rollout/order",
        "spec.services[0].rollout.order",
    ),
    (
        "/spec/services/0/rollout/monitor_seconds",
        "spec.services[0].rollout.monitor_seconds",
    ),
    (
        "/spec/services/0/secrets/0/name",
        "spec.services[0].secrets[0].name",
    ),
    (
        "/spec/services/1/source/build/dockerfile",
        "spec.services[1].source.build.dockerfile",
    ),
    (
        "/spec/services/1/source/build/context",
        "spec.services[1].source.build.context",
    ),
    (
        "/spec/services/1/source/build/args/MODE",
        "spec.services[1].source.build.args.MODE",
    ),
    (
        "/spec/services/1/source/build/target",
        "spec.services[1].source.build.target",
    ),
    ("/spec/services/1/replicas", "spec.services[1].replicas"),
    ("/spec/services/1/command/0", "spec.services[1].command[0]"),
    (
        "/spec/services/1/healthcheck/command/0",
        "spec.services[1].healthcheck.command[0]",
    ),
    (
        "/spec/services/1/healthcheck/command/1",
        "spec.services[1].healthcheck.command[1]",
    ),
    (
        "/spec/services/1/healthcheck/interval_seconds",
        "spec.services[1].healthcheck.interval_seconds",
    ),
    (
        "/spec/services/1/healthcheck/timeout_seconds",
        "spec.services[1].healthcheck.timeout_seconds",
    ),
    ("/spec/routes/0/hostname", "spec.routes[0].hostname"),
    ("/spec/routes/1/hostname", "spec.routes[1].hostname"),
    ("/spec/routes/1/redirect/to", "spec.routes[1].redirect.to"),
    ("/spec/jobs/0/command/0", "spec.jobs[0].command[0]"),
    ("/spec/jobs/0/command/1", "spec.jobs[0].command[1]"),
];
