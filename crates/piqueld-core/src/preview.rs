//! Manifest differences describe user intent without requiring Docker observation.
use crate::manifest::dependencies::StartupOrder;
use crate::{
    ActionKind, ActionReason, DiagnosticSeverity, DockerServiceName, EnvironmentId,
    NormalizedApplication, PlanAction, PlanDiagnostic,
    api::{ManifestChange, ServiceRolloutView},
    codes,
    manifest::{ApplicationSpec, FileSource, JobVolumeAccess, files::content_digest},
};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

impl ManifestChange {
    /// Compares specifications without exposing environment or process values.
    /// Rendered specifications compare through [`crate::manifest::domain::ValidatedSpec::to_input`].
    ///
    /// Both sides are flattened into sorted field paths (see `fields`) and every path
    /// whose value differs is reported. `None` means the application does not exist
    /// yet, so every field is an addition.
    #[must_use]
    pub fn between(current: Option<&ApplicationSpec>, proposed: &ApplicationSpec) -> Vec<Self> {
        Self::differences(
            &current.map_or_else(BTreeMap::new, Self::fields),
            &Self::fields(proposed),
        )
    }

    /// Compares rendered applications like [`Self::between`], plus the
    /// repository files each captured, one `repository_files.<path>` entry
    /// per file summarized by size and digest: a release's files can change
    /// while the paths its manifest mounts stay the same.
    #[must_use]
    pub fn between_applications(
        current: Option<&NormalizedApplication>,
        proposed: &NormalizedApplication,
    ) -> Vec<Self> {
        let fields = |application: &NormalizedApplication| {
            let mut fields = Self::fields(&application.spec().to_input());
            fields.extend(application.files().iter().map(|(path, content)| {
                (
                    format!("repository_files.{path}"),
                    Value::String(format!(
                        "{} bytes, sha256:{}",
                        content.len(),
                        &content_digest(content)[..12]
                    )),
                )
            }));
            fields
        };
        Self::differences(
            &current.map_or_else(BTreeMap::new, fields),
            &fields(proposed),
        )
    }

    /// Every field whose value differs between `old` and `new`, including
    /// fields only one has.
    fn differences(old: &BTreeMap<String, Value>, new: &BTreeMap<String, Value>) -> Vec<Self> {
        old.keys()
            .chain(new.keys())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .filter_map(|field| {
                let before = old.get(field);
                let after = new.get(field);
                (before != after).then(|| Self {
                    field: field.clone(),
                    before: before.map(|value| Self::display(field, value)),
                    after: after.map(|value| Self::display(field, value)),
                })
            })
            .collect()
    }

    /// Flattens a specification into comparable field paths. Services and jobs
    /// contribute one entry per top-level setting except `name`, and services
    /// one per file, keyed by target, which summarizes inline content by size
    /// and digest; routes are keyed by hostname, volumes only record presence,
    /// and variables are keyed by name.
    ///
    /// ```text
    /// manifest                              -> repository manifest settings
    /// services.web.replicas                 -> 3
    /// services.web.files./etc/app.yaml      -> "path: app.yaml, mode: 0444"
    /// services.web.files./etc/motd          -> "content: 5 bytes, sha256:2cf24dba5fb0, mode: 0444"
    /// jobs.migrate.service                  -> "web"
    /// routes.app.example.com                -> {"service":"web","port":3000}
    /// volumes.data                          -> "present (retained if removed)"
    /// variables.domain                      -> "example.com"
    /// environments.staging.variables.domain -> "staging.example.com"
    /// environments.staging.visibility       -> "private"
    /// previews.visibility                   -> "public"
    /// ```
    fn fields(spec: &ApplicationSpec) -> BTreeMap<String, Value> {
        let mut fields = BTreeMap::new();
        for service in &spec.services {
            for file in &service.files {
                let source = match &file.source {
                    FileSource::Path(path) => format!("path: {path}"),
                    FileSource::Content(content) => format!(
                        "content: {} bytes, sha256:{}",
                        content.len(),
                        &content_digest(content)[..12]
                    ),
                };
                fields.insert(
                    format!("services.{}.files.{}", service.name, file.target),
                    Value::String(format!("{source}, mode: {}", file.mode)),
                );
            }
        }
        let Ok(Value::Object(mut spec)) = serde_json::to_value(spec) else {
            return BTreeMap::new();
        };
        let mut take = |key: &str| spec.remove(key).unwrap_or(Value::Null);
        if let Some(manifest) = Some(take("manifest")).filter(|value| !value.is_null()) {
            fields.insert("manifest".into(), manifest);
        }
        let mut entries = |key: &str, by: &str, fields: &mut BTreeMap<String, Value>| {
            let Value::Array(items) = take(key) else {
                return Vec::new();
            };
            items
                .into_iter()
                .filter_map(|item| match item {
                    Value::Object(mut item) => {
                        let id = item.remove(by)?.as_str()?.to_owned();
                        Some((format!("{key}.{id}"), item))
                    }
                    _ => None,
                })
                .inspect(|(path, _)| {
                    if key == "volumes" {
                        fields.insert(
                            path.clone(),
                            Value::String("present (retained if removed)".into()),
                        );
                    }
                })
                .collect()
        };
        for (path, service) in entries("services", "name", &mut fields) {
            for (key, value) in service {
                if key != "files" {
                    fields.insert(format!("{path}.{key}"), value);
                }
            }
        }
        for (path, route) in entries("routes", "hostname", &mut fields) {
            fields.insert(path, Value::Object(route));
        }
        entries("volumes", "name", &mut fields);
        // Jobs run in declared order, so a reordering is a change of its own.
        let mut order = Vec::new();
        for (path, job) in entries("jobs", "name", &mut fields) {
            order.push(Value::String(path["jobs.".len()..].to_owned()));
            for (key, value) in job {
                fields.insert(format!("{path}.{key}"), value);
            }
        }
        if !order.is_empty() {
            fields.insert("jobs.order".into(), Value::Array(order));
        }
        if let Value::Object(variables) = take("variables") {
            for (name, value) in variables {
                fields.insert(format!("variables.{name}"), value);
            }
        }
        if let Value::Object(environments) = take("environments") {
            for (environment, config) in environments {
                if let Some(visibility) = config.get("visibility") {
                    fields.insert(
                        format!("environments.{environment}.visibility"),
                        visibility.clone(),
                    );
                }
                if let Some(Value::Object(variables)) = config.get("variables") {
                    for (name, value) in variables {
                        fields.insert(
                            format!("environments.{environment}.variables.{name}"),
                            value.clone(),
                        );
                    }
                }
            }
        }
        if let Value::Object(previews) = take("previews") {
            for (key, value) in previews {
                fields.insert(format!("previews.{key}"), value);
            }
        }
        fields
    }

    /// Renders a field value for display, replacing non-null environment, process,
    /// and health check values with `<redacted>`. Strings are shown unquoted; other
    /// JSON values use their compact JSON form.
    fn display(field: &str, value: &Value) -> String {
        if [".environment", ".command", ".arguments", ".healthcheck"]
            .iter()
            .any(|suffix| field.ends_with(suffix))
            && !value.is_null()
        {
            "<redacted>".into()
        } else if let Some(value) = value.as_str() {
            value.into()
        } else {
            value.to_string()
        }
    }
}

impl ServiceRolloutView {
    /// Effective rollout of every service in `application`, in canonical order.
    #[must_use]
    pub fn for_application(application: &NormalizedApplication) -> Vec<Self> {
        application
            .spec()
            .services
            .iter()
            .map(|service| {
                let policy = service.rollout_policy();
                Self {
                    service: service.name.to_string(),
                    order: policy.order,
                    order_source: service.rollout.order_source(),
                    monitor_seconds: policy.monitor_seconds,
                }
            })
            .collect()
    }
}

impl crate::Plan {
    /// Adds `diagnostics`, keeping every diagnostic in resource order.
    pub fn warn(&mut self, diagnostics: impl IntoIterator<Item = PlanDiagnostic>) {
        self.diagnostics.extend(diagnostics);
        self.sort_diagnostics();
    }

    /// Warns about each service of `application` whose explicit start-first
    /// order lets the old and new task write the same volume at once.
    pub fn warn_rollouts(&mut self, application: &NormalizedApplication) {
        self.warn(
            application
                .spec()
                .services
                .iter()
                .filter(|service| service.rollout_overlaps_writable_volume())
                .map(|service| PlanDiagnostic {
                    code: codes::ROLLOUT_START_FIRST_WRITABLE_VOLUME.into(),
                    severity: DiagnosticSeverity::Warning,
                    resource: service.name.to_string(),
                    message: "start-first rollouts briefly run two tasks on the same writable volume; single-writer stores such as PostgreSQL or SQLite can corrupt their data".into(),
                    blocking: false,
                }),
        );
    }

    /// Lists each job where a deployment runs it: after source resolution,
    /// networks, volumes, and the job's startup dependencies, before any other
    /// service change. Warns about the services each exclusive job stops.
    /// Deployments run jobs before planning their rollout, so only previews
    /// list them.
    pub fn add_jobs(&mut self, application: &NormalizedApplication, environment: &EnvironmentId) {
        let spec = application.spec();
        let mut remaining = std::mem::take(&mut self.actions);
        for job in &spec.jobs {
            let dependencies = spec.services.dependencies_of(job.service.as_str());
            let dependencies = spec
                .services
                .iter()
                .filter(|service| dependencies.contains(service.name.as_str()))
                .map(|service| DockerServiceName::for_service(environment, &service.name))
                .collect::<BTreeSet<_>>();
            let prepares = |action: &PlanAction| match &action.kind {
                ActionKind::EnsureService { service } => dependencies.contains(&service.name),
                ActionKind::WaitForService { service } => dependencies
                    .iter()
                    .any(|dependency| dependency.as_str() == service),
                ActionKind::RemoveService { .. }
                | ActionKind::RemoveNetwork { .. }
                | ActionKind::WaitForServiceRemoval { .. } => false,
                _ => true,
            };
            let (prerequisites, rest): (Vec<_>, Vec<_>) = remaining.into_iter().partition(prepares);
            self.actions.extend(prerequisites);
            self.actions.push(PlanAction::new(
                ActionKind::RunJob {
                    job: job.name.to_string(),
                },
                ActionReason::DeploymentJob,
            ));
            remaining = rest;
        }
        self.actions.extend(remaining);
        self.warn(spec.jobs.iter().filter_map(|job| {
            let volumes = spec
                .services
                .iter()
                .find(|service| service.name == job.service)?
                .mounts
                .iter()
                .filter(|mount| job.volume_access == JobVolumeAccess::Exclusive && !mount.read_only)
                .map(|mount| &mount.volume)
                .collect::<BTreeSet<_>>();
            let services = spec
                .services
                .iter()
                .filter(|service| service.mounts.iter().any(|mount| volumes.contains(&mount.volume)))
                .map(|service| service.name.as_str())
                .collect::<Vec<_>>();
            (!services.is_empty()).then(|| PlanDiagnostic {
                code: codes::JOB_STOPS_SERVICES.into(),
                severity: DiagnosticSeverity::Warning,
                resource: job.name.to_string(),
                message: format!(
                    "job {} uses its volumes exclusively: {} and any other running service mounting them stop before it runs and stay stopped until the rollout",
                    job.name,
                    services.join(", ")
                ),
                blocking: false,
            })
        }));
    }

    /// Removes sensitive configuration from an informational preview's runtime actions.
    /// Execution always recomputes its own plan from the unredacted desired target.
    pub fn redact_configuration(&mut self) {
        for action in &mut self.actions {
            if let crate::ActionKind::EnsureService { service } = &mut action.kind {
                for value in service
                    .environment
                    .values_mut()
                    .chain(service.command.iter_mut())
                    .chain(service.arguments.iter_mut())
                {
                    "<redacted>".clone_into(value);
                }
                match &mut service.healthcheck {
                    Some(crate::manifest::ValidatedHealthCheck::Command { command, .. }) => {
                        for value in command {
                            "<redacted>".clone_into(value);
                        }
                    }
                    Some(crate::manifest::ValidatedHealthCheck::Http { path, .. }) => {
                        "<redacted>".clone_into(path);
                    }
                    None => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changes_show_configuration_differences_without_exposing_sensitive_values() {
        let input = r#"api_version="piqueld.dev/v1alpha1"
kind="Application"
[metadata]
name="example"
[[spec.services]]
name="web"
replicas=1
[spec.services.source]
type="image"
image="example:1"
[spec.services.environment]
TOKEN="old-secret"
"#;
        let id = crate::ApplicationId::parse("app-example-01").unwrap();
        let original = crate::parse_toml(input).unwrap().normalize(id);
        let mut changed = original.to_manifest();
        changed.spec.services[0].replicas = 3.into();
        changed.spec.services[0]
            .environment
            .insert("TOKEN".into(), "new-secret".into());
        let changed = changed.validate().unwrap().normalize(original.id().clone());
        let (original, changed) = (original.spec().to_input(), changed.spec().to_input());
        let differences = ManifestChange::between(Some(&original), &changed);
        assert_eq!(differences.len(), 2);
        assert!(
            differences
                .iter()
                .any(|change| change.field == "services.web.replicas"
                    && change.before.as_deref() == Some("1")
                    && change.after.as_deref() == Some("3"))
        );
        let json = serde_json::to_string(&differences).unwrap();
        assert!(!json.contains("old-secret"));
        assert!(!json.contains("new-secret"));
        assert!(json.contains("redacted"));
        assert!(ManifestChange::between(Some(&original), &original).is_empty());
    }

    #[test]
    fn changes_show_new_and_changed_jobs() {
        let input = r#"api_version="piqueld.dev/v1alpha1"
kind="Application"
[metadata]
name="example"
[[spec.services]]
name="web"
[spec.services.source]
type="image"
image="example:1"
"#;
        let id = crate::ApplicationId::parse("app-example-01").unwrap();
        let original = crate::parse_toml(input).unwrap().normalize(id.clone());
        let with_jobs = |jobs: &[(&str, &str)]| {
            let mut manifest = original.to_manifest();
            manifest.spec.jobs = jobs
                .iter()
                .map(|(name, command)| crate::manifest::Job {
                    name: (*name).into(),
                    service: "web".into(),
                    command: vec![(*command).into()],
                    run: crate::manifest::JobRun::BeforeRollout,
                    timeout_seconds: 300,
                    volume_access: JobVolumeAccess::ReadOnly,
                })
                .collect();
            manifest
                .validate()
                .unwrap()
                .normalize(id.clone())
                .spec()
                .to_input()
        };
        let original = original.spec().to_input();
        let fields = |changes: Vec<ManifestChange>| {
            changes
                .into_iter()
                .map(|change| (change.field, change.before.is_some()))
                .collect::<Vec<_>>()
        };
        let migrate = [("migrate", "migrate")];
        assert_eq!(
            fields(ManifestChange::between(
                Some(&original),
                &with_jobs(&migrate)
            )),
            [
                ("jobs.migrate.command".to_owned(), false),
                ("jobs.migrate.run".to_owned(), false),
                ("jobs.migrate.service".to_owned(), false),
                ("jobs.migrate.timeout_seconds".to_owned(), false),
                ("jobs.order".to_owned(), false),
            ]
        );
        assert_eq!(
            fields(ManifestChange::between(
                Some(&with_jobs(&migrate)),
                &with_jobs(&[("migrate", "upgrade")])
            )),
            [("jobs.migrate.command".to_owned(), true)]
        );
        assert_eq!(
            fields(ManifestChange::between(
                Some(&with_jobs(&[("migrate", "migrate"), ("seed", "seed")])),
                &with_jobs(&[("seed", "seed"), ("migrate", "migrate")])
            )),
            [("jobs.order".to_owned(), true)]
        );
    }
}
