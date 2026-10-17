//! What a plan shows before execution: manifest field changes, which
//! describe user intent without requiring Docker observation, and each
//! service's effective rollout.

use crate::{
    NormalizedApplication,
    api::{ManifestChange, ServiceRolloutView},
    manifest::{ApplicationSpec, FileSource, files::content_digest},
    planner::REDACTED,
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
                    Value::String(Self::summarize(content)),
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
        changed_keys(old, new)
            .map(|field| Self {
                field: field.clone(),
                before: old.get(field).map(|value| Self::display(field, value)),
                after: new.get(field).map(|value| Self::display(field, value)),
            })
            .collect()
    }

    /// Exports, keyed by service, and imports, keyed by their alias and
    /// valued by the environment they name, if any.
    fn link_fields(spec: &ApplicationSpec) -> BTreeMap<String, Value> {
        let exports = spec.exports.iter().map(|export| {
            let mut value = serde_json::to_value(export).unwrap_or(Value::Null);
            if let Value::Object(object) = &mut value {
                object.remove("service");
            }
            (format!("exports.{}", export.service), value)
        });
        let imports = spec.imports.iter().map(|import| {
            (
                format!("imports.{}.{}", import.service, import.application),
                import
                    .environment
                    .as_ref()
                    .map_or(Value::Null, |environment| environment.as_str().into()),
            )
        });
        exports.chain(imports).collect()
    }

    /// Summarizes file content by size and digest, e.g. `5 bytes, sha256:2cf24dba5fb0`.
    fn summarize(content: &str) -> String {
        format!(
            "{} bytes, sha256:{}",
            content.len(),
            &content_digest(content)[..12]
        )
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
    /// exports.api                           -> {"applications":["notes"]}
    /// imports.api.auth                      -> "staging"
    /// ```
    fn fields(spec: &ApplicationSpec) -> BTreeMap<String, Value> {
        let mut fields = Self::link_fields(spec);
        for service in &spec.services {
            for file in &service.files {
                let source = match &file.source {
                    FileSource::Path(path) => format!("path: {path}"),
                    FileSource::Content(content) => {
                        format!("content: {}", Self::summarize(content))
                    }
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
            REDACTED.into()
        } else if let Some(value) = value.as_str() {
            value.into()
        } else {
            value.to_string()
        }
    }
}

/// Keys whose values differ between `old` and `new`, including keys only one
/// has, in key order.
pub(crate) fn changed_keys<'a, K: Ord, V: PartialEq>(
    old: &'a BTreeMap<K, V>,
    new: &'a BTreeMap<K, V>,
) -> impl Iterator<Item = &'a K> {
    old.keys()
        .chain(new.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|key| old.get(*key) != new.get(*key))
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
                    volume_access: crate::manifest::JobVolumeAccess::ReadOnly,
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
