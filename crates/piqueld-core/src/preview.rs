//! Manifest differences describe user intent without requiring Docker observation.
use crate::{
    DiagnosticSeverity, NormalizedApplication, PlanDiagnostic,
    api::{ManifestChange, ServiceRolloutView},
    codes,
    manifest::ApplicationSpec,
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
        let old = current.map_or_else(BTreeMap::new, Self::fields);
        let new = Self::fields(proposed);
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

    /// Flattens a specification into comparable field paths. Services contribute
    /// one entry per top-level setting except `name`; routes are keyed by
    /// hostname, volumes only record presence, and variables are keyed by name.
    ///
    /// ```text
    /// manifest                              -> repository manifest settings
    /// services.web.replicas                 -> 3
    /// routes.app.example.com                -> {"service":"web","port":3000}
    /// volumes.data                          -> "present (retained if removed)"
    /// variables.domain                      -> "example.com"
    /// environments.staging.variables.domain -> "staging.example.com"
    /// ```
    fn fields(spec: &ApplicationSpec) -> BTreeMap<String, Value> {
        let Ok(Value::Object(mut spec)) = serde_json::to_value(spec) else {
            return BTreeMap::new();
        };
        let mut take = |key: &str| spec.remove(key).unwrap_or(Value::Null);
        let mut fields = BTreeMap::new();
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
                fields.insert(format!("{path}.{key}"), value);
            }
        }
        for (path, route) in entries("routes", "hostname", &mut fields) {
            fields.insert(path, Value::Object(route));
        }
        entries("volumes", "name", &mut fields);
        if let Value::Object(variables) = take("variables") {
            for (name, value) in variables {
                fields.insert(format!("variables.{name}"), value);
            }
        }
        if let Value::Object(environments) = take("environments") {
            for (environment, config) in environments {
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
    /// Warns about each `[spec.environments.<name>]` block of `template` that
    /// names none of `environments`. Such a block is kept, so a manifest can be
    /// saved before its environment is created.
    pub fn warn_environments(
        &mut self,
        template: &crate::manifest::ApplicationTemplate,
        environments: &[crate::EnvironmentName],
    ) {
        self.diagnostics.extend(
            template
                .spec()
                .environments
                .keys()
                .filter(|name| !environments.iter().any(|environment| environment.as_str() == *name))
                .map(|name| PlanDiagnostic {
                    code: codes::ENVIRONMENT_BLOCK_UNKNOWN.into(),
                    severity: DiagnosticSeverity::Warning,
                    resource: format!("spec.environments.{name}"),
                    message: format!("the application has no environment named {name}; this configuration applies once one is created"),
                    blocking: false,
                }),
        );
        self.sort_diagnostics();
    }

    /// Warns about each service of `application` whose explicit start-first
    /// order lets the old and new task write the same volume at once.
    pub fn warn_rollouts(&mut self, application: &NormalizedApplication) {
        self.diagnostics.extend(
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
        self.sort_diagnostics();
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
}
