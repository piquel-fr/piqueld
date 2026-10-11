//! Installation-wide preview limits, the daemon's `[previews]` section, and
//! how they bound what a preview deploys.

use super::validation::{MAX_CPU_MILLIS, REPLICAS};
use super::{NormalizedApplication, ValidatedResourceLimits};
use crate::{DiagnosticSeverity, PlanDiagnostic, codes};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// How many previews may exist, and what each preview service may use.
///
/// Counts are checked when a preview is created, so lowering one below the
/// current count keeps every existing preview running and only refuses new
/// ones. Bounds apply when a preview's manifest is rendered, so a preview
/// picks up changed bounds on its next deployment.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(default, deny_unknown_fields)]
pub struct PreviewLimits {
    /// Previews one application may have.
    pub max_per_application: u32,
    /// Previews the whole installation may have.
    pub max_total: u32,
    /// CPU limit, in millicores, of preview services that set none.
    pub default_cpu_millis: u32,
    /// Memory limit, in bytes, of preview services that set none.
    pub default_memory_bytes: u64,
    /// Most replicas a preview service runs; manifests asking for more are
    /// capped with a warning.
    pub max_replicas: u16,
}

impl Default for PreviewLimits {
    fn default() -> Self {
        Self {
            max_per_application: 10,
            max_total: 30,
            default_cpu_millis: 500,
            default_memory_bytes: 512 * 1024 * 1024,
            max_replicas: 1,
        }
    }
}

/// A `[previews]` bound no manifest could set.
#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum PreviewLimitsError {
    /// The default CPU limit is zero or above the manifest maximum.
    #[error("previews.default_cpu_millis must be between 1 and {}", MAX_CPU_MILLIS)]
    Cpu,
    /// The default memory limit is zero or does not fit Docker's `i64`.
    #[error("previews.default_memory_bytes must be between 1 and {}", i64::MAX)]
    Memory,
    /// The replica cap is outside the manifest's replica range.
    #[error("previews.max_replicas must be between {} and {}", REPLICAS.start(), REPLICAS.end())]
    Replicas,
}

impl PreviewLimits {
    /// Checks that every bound is a value a manifest could set itself.
    ///
    /// # Errors
    /// Returns the first bound out of its manifest range.
    pub fn validate(&self) -> Result<(), PreviewLimitsError> {
        if !(1..=MAX_CPU_MILLIS).contains(&self.default_cpu_millis) {
            return Err(PreviewLimitsError::Cpu);
        }
        if self.default_memory_bytes == 0 || i64::try_from(self.default_memory_bytes).is_err() {
            return Err(PreviewLimitsError::Memory);
        }
        if !REPLICAS.contains(&self.max_replicas) {
            return Err(PreviewLimitsError::Replicas);
        }
        Ok(())
    }

    /// Bounds a preview's rendered services: each CPU or memory limit a
    /// service leaves unset gets its default, and replicas above
    /// `max_replicas` are capped. Explicit limits are kept. Returns a
    /// warning for every service it changed.
    ///
    /// ```text
    /// web: replicas 3, no limits -> replicas 1, 500 millicores, 536870912 bytes
    ///   preview_limits_defaulted, preview_replicas_capped
    /// db:  replicas 1, 2000 millicores -> unchanged CPU, default memory
    ///   preview_limits_defaulted
    /// ```
    pub(crate) fn bound(&self, application: &mut NormalizedApplication) -> Vec<PlanDiagnostic> {
        let mut warnings = Vec::new();
        for service in &mut application.spec.services {
            let warning = |code: &str, message: String| PlanDiagnostic {
                code: code.into(),
                severity: DiagnosticSeverity::Warning,
                resource: service.name.to_string(),
                message,
                blocking: false,
            };
            let resources = service.resources.get_or_insert(ValidatedResourceLimits {
                cpu_millis: None,
                memory_bytes: None,
            });
            let mut defaulted = Vec::new();
            if resources.cpu_millis.is_none() {
                resources.cpu_millis = Some(self.default_cpu_millis);
                defaulted.push(format!("{} millicores of CPU", self.default_cpu_millis));
            }
            if resources.memory_bytes.is_none() {
                resources.memory_bytes = Some(self.default_memory_bytes);
                defaulted.push(format!("{} bytes of memory", self.default_memory_bytes));
            }
            if !defaulted.is_empty() {
                warnings.push(warning(
                    codes::PREVIEW_LIMITS_DEFAULTED,
                    format!(
                        "{} runs with the [previews] default of {}, since its manifest sets no limit",
                        service.name,
                        defaulted.join(" and ")
                    ),
                ));
            }
            if service.replicas > self.max_replicas {
                warnings.push(warning(
                    codes::PREVIEW_REPLICAS_CAPPED,
                    format!(
                        "{} asks for {} replicas; previews run at most {} ([previews] max_replicas)",
                        service.name, service.replicas, self.max_replicas
                    ),
                ));
                service.replicas = self.max_replicas;
            }
        }
        warnings
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ApplicationId, ApplicationName, EnvironmentKind, GitBranch, Preview, PreviewSlug,
        parse_toml,
    };

    /// `web` asks for 3 replicas and sets no limits; `db` sets only its CPU.
    fn application() -> NormalizedApplication {
        parse_toml(
            r#"api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "notes"
[[spec.services]]
name = "web"
replicas = 3
[spec.services.source]
type = "image"
image = "nginx:alpine"
[[spec.services]]
name = "db"
[spec.services.source]
type = "image"
image = "postgres:17"
[spec.services.resources]
cpu_millis = 2000
"#,
        )
        .unwrap()
        .normalize(ApplicationId::parse("app-notes-01").unwrap())
    }

    fn preview() -> EnvironmentKind {
        let (name, branch) = (
            ApplicationName::parse("notes").unwrap(),
            GitBranch::parse("feat/login").unwrap(),
        );
        EnvironmentKind::Preview(Box::new(Preview {
            slug: PreviewSlug::derive(&name, &branch, None),
            branch,
            slot: None,
        }))
    }

    #[test]
    fn previews_get_default_limits_and_capped_replicas_with_warnings_and_environments_never() {
        let limits = PreviewLimits::default();
        let mut environment = application();
        assert_eq!(
            EnvironmentKind::Environment.bound(&mut environment, &limits),
            []
        );
        assert_eq!(environment, application());

        let mut bounded = application();
        let warnings = preview().bound(&mut bounded, &limits);
        let services = bounded
            .spec()
            .services
            .iter()
            .map(|service| (service.name.as_str(), service.replicas, &service.resources))
            .collect::<Vec<_>>();
        let limited = |cpu_millis| {
            Some(ValidatedResourceLimits {
                cpu_millis: Some(cpu_millis),
                memory_bytes: Some(limits.default_memory_bytes),
            })
        };
        assert_eq!(
            services,
            [("db", 1, &limited(2000)), ("web", 1, &limited(500))]
        );
        assert_eq!(
            warnings
                .iter()
                .map(|warning| (warning.resource.as_str(), warning.code.as_str()))
                .collect::<Vec<_>>(),
            [
                ("db", codes::PREVIEW_LIMITS_DEFAULTED),
                ("web", codes::PREVIEW_LIMITS_DEFAULTED),
                ("web", codes::PREVIEW_REPLICAS_CAPPED),
            ]
        );
        assert_eq!(
            warnings[2].message,
            "web asks for 3 replicas; previews run at most 1 ([previews] max_replicas)"
        );
    }
}
