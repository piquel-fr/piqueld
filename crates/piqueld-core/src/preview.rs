//! Manifest differences describe user intent without requiring Docker observation.
use crate::{NormalizedApplication, api::ManifestChange};
use std::collections::{BTreeMap, BTreeSet};

impl ManifestChange {
    /// Compares normalized specifications without exposing environment or process values.
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

    fn fields(application: &NormalizedApplication) -> BTreeMap<String, serde_json::Value> {
        let mut fields = BTreeMap::new();
        for service in &application.spec.services {
            let value = serde_json::to_value(service).expect("manifest service is serializable");
            if let serde_json::Value::Object(values) = value {
                for (key, value) in values {
                    if key != "name" {
                        fields.insert(format!("services.{}.{}", service.name, key), value);
                    }
                }
            }
        }
        for volume in &application.spec.volumes {
            fields.insert(
                format!("volumes.{}", volume.name),
                serde_json::Value::String("present (retained if removed)".into()),
            );
        }
        fields
    }

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

impl crate::Plan {
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
        let mut changed = original.clone();
        changed.spec.services[0].replicas = 3;
        changed.spec.services[0]
            .environment
            .insert("TOKEN".into(), "new-secret".into());
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
