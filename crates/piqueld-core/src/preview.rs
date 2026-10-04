//! Manifest differences describe user intent without requiring Docker observation.
use crate::{
    DiagnosticSeverity, NormalizedApplication, PlanDiagnostic,
    api::{ManifestChange, ServiceRolloutView},
    codes,
};
use std::collections::{BTreeMap, BTreeSet};

impl ManifestChange {
    /// Compares normalized specifications without exposing environment or process values.
    ///
    /// Both sides are flattened into sorted field paths (see `fields`) and every path
    /// whose value differs is reported. `None` means the application does not exist
    /// yet, so every field is an addition.
    #[must_use]
    pub fn between(
        current: Option<&NormalizedApplication>,
        proposed: &NormalizedApplication,
    ) -> Vec<Self> {
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

    /// Flattens an application into comparable field paths. Services contribute one
    /// entry per top-level setting except `name`; routes are keyed by hostname and
    /// volumes only record presence.
    ///
    /// ```text
    /// manifest                 -> repository manifest settings
    /// services.web.replicas    -> 3
    /// routes.app.example.com   -> {"service":"web","port":3000}
    /// volumes.data             -> "present (retained if removed)"
    /// ```
    fn fields(application: &NormalizedApplication) -> BTreeMap<String, serde_json::Value> {
        let mut fields = BTreeMap::new();
        if let Some(manifest) = &application.spec().manifest {
            fields.insert(
                "manifest".into(),
                serde_json::to_value(manifest).expect("manifest configuration is serializable"),
            );
        }
        for service in &application.spec().services {
            let value = serde_json::to_value(service).expect("manifest service is serializable");
            if let serde_json::Value::Object(values) = value {
                for (key, value) in values {
                    if key != "name" {
                        fields.insert(format!("services.{}.{}", service.name, key), value);
                    }
                }
            }
        }
        for route in &application.spec().routes {
            fields.insert(
                format!("routes.{}", route.hostname),
                serde_json::to_value(&route.target).expect("route target is serializable"),
            );
        }
        for volume in &application.spec().volumes {
            fields.insert(
                format!("volumes.{}", volume.name),
                serde_json::Value::String("present (retained if removed)".into()),
            );
        }
        fields
    }

    /// Renders a field value for display, replacing non-null environment, process,
    /// and health check values with `<redacted>`. Strings are shown unquoted; other
    /// JSON values use their compact JSON form.
    fn display(field: &str, value: &serde_json::Value) -> String {
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
                    Some(crate::HealthCheck::Command { command, .. }) => {
                        for value in command {
                            "<redacted>".clone_into(value);
                        }
                    }
                    Some(crate::HealthCheck::Http { path, .. }) => "<redacted>".clone_into(path),
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
        changed.spec.services[0].replicas = 3;
        changed.spec.services[0]
            .environment
            .insert("TOKEN".into(), "new-secret".into());
        let changed = changed.validate().unwrap().normalize(original.id().clone());
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
