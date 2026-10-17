//! Private links between applications: the services an application lets
//! others reach (`[[spec.exports]]`) and the services of other applications it
//! reaches (`[[spec.imports]]`).
//!
//! A link exists only while both sides agree: the exporter grants the
//! importer's application, and the importer asks for the service. Each side is
//! declared in its own manifest, so each application's owners control their
//! half. [`crate::links`] resolves links from deployed targets.

use super::validation::{error, validate_name};
use super::variables::Template;
use super::{ValidationError, ValidationErrors};
use crate::{ApplicationName, EnvironmentName, ServiceName, codes};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use utoipa::ToSchema;

/// Most exports or imports one application declares, and most applications
/// one export grants.
pub const MAX_LINKS: usize = 64;

/// A service other applications may reach, `[[spec.exports]]`.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Export {
    /// Service in this application that importers reach.
    pub service: String,
    /// Applications whose environments may import the service.
    pub applications: Vec<String>,
    /// Whether previews of those applications may import it too; `false` by
    /// default, so only their environments can.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub previews: bool,
}

/// A service of another application this application reaches,
/// `[[spec.imports]]`, as `<service>.<application>`.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct Import {
    /// Application exporting the service.
    pub application: String,
    /// Exported service of that application.
    pub service: String,
    /// Environment of that application to reach. By default, the one named
    /// like the importing environment. It may reference variables, so
    /// previews can reach e.g. `staging`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<Template>,
}

/// A validated export.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ValidatedExport {
    /// Exported service of this application.
    pub service: ServiceName,
    /// Applications whose environments may import it.
    pub applications: BTreeSet<ApplicationName>,
    /// Whether previews of those applications may import it too.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub previews: bool,
}

/// A validated import.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
#[serde(deny_unknown_fields)]
pub struct ValidatedImport {
    /// Application exporting the service.
    pub application: ApplicationName,
    /// Exported service of that application.
    pub service: ServiceName,
    /// Environment of that application to reach; unset reaches the one named
    /// like the importing environment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<EnvironmentName>,
}

impl ValidatedImport {
    /// The name the imported service answers to in the importer's services.
    #[must_use]
    pub fn alias(&self) -> String {
        alias(&self.service, &self.application)
    }
}

/// The name an exported service answers to on its link networks:
/// `<service>.<application>`.
#[must_use]
pub fn alias(service: &ServiceName, application: &ApplicationName) -> String {
    format!("{service}.{application}")
}

impl Export {
    /// Converts input already checked by [`validate`].
    pub(super) fn into_validated(self, index: usize) -> Result<ValidatedExport, ValidationErrors> {
        let path = format!("spec.exports[{index}]");
        Ok(ValidatedExport {
            service: ServiceName::parse(self.service).map_err(|source| {
                ValidationErrors::invalid_name(format!("{path}.service"), source)
            })?,
            applications: self
                .applications
                .into_iter()
                .map(|application| {
                    ApplicationName::parse(application).map_err(|source| {
                        ValidationErrors::invalid_name(format!("{path}.applications"), source)
                    })
                })
                .collect::<Result<_, _>>()?,
            previews: self.previews,
        })
    }
}

impl ValidatedExport {
    /// Converts back to the editable input shape.
    #[must_use]
    pub fn to_input(&self) -> Export {
        Export {
            service: self.service.to_string(),
            applications: self.applications.iter().map(ToString::to_string).collect(),
            previews: self.previews,
        }
    }
}

impl Import {
    /// Converts input already checked by [`validate`] and rendered.
    pub(super) fn into_validated(self, index: usize) -> Result<ValidatedImport, ValidationErrors> {
        let path = format!("spec.imports[{index}]");
        Ok(ValidatedImport {
            application: ApplicationName::parse(self.application).map_err(|source| {
                ValidationErrors::invalid_name(format!("{path}.application"), source)
            })?,
            service: ServiceName::parse(self.service).map_err(|source| {
                ValidationErrors::invalid_name(format!("{path}.service"), source)
            })?,
            environment: self
                .environment
                .map(|environment| {
                    let path = format!("{path}.environment");
                    let literal = environment
                        .as_literal()
                        .ok_or_else(|| ValidationErrors::unresolved(&path))?;
                    EnvironmentName::parse(literal)
                        .map_err(|source| ValidationErrors::invalid_name(&path, source))
                })
                .transpose()?,
        })
    }
}

impl ValidatedImport {
    /// Converts back to the editable input shape.
    #[must_use]
    pub fn to_input(&self) -> Import {
        Import {
            application: self.application.to_string(),
            service: self.service.to_string(),
            environment: self
                .environment
                .as_ref()
                .map(|environment| Template::literal(environment.as_str())),
        }
    }
}

/// Checks the link budgets; returns `false` when one is exceeded.
pub(super) fn within_budget(
    exports: &[Export],
    imports: &[Import],
    errors: &mut Vec<ValidationError>,
) -> bool {
    let mut within = true;
    for (path, count) in [
        ("spec.exports", exports.len()),
        ("spec.imports", imports.len()),
    ] {
        if count > MAX_LINKS {
            error(
                errors,
                codes::LINK_COUNT_EXCESSIVE,
                path,
                &format!("an application must declare at most {MAX_LINKS} of these"),
            );
            within = false;
        }
    }
    for (index, export) in exports.iter().enumerate() {
        if export.applications.len() > MAX_LINKS {
            error(
                errors,
                codes::LINK_COUNT_EXCESSIVE,
                &format!("spec.exports[{index}].applications"),
                &format!("an export must grant at most {MAX_LINKS} applications"),
            );
            within = false;
        }
    }
    within
}

/// Validates exports and imports of an application whose services are
/// `services`:
/// - names are valid, and environments once rendered;
/// - each export names a declared service, at most once, and grants at least
///   one application, each once;
/// - each import names each service of an application once.
///
/// An application may name itself: its environments then link to each other.
pub(super) fn validate(
    services: &BTreeSet<&str>,
    exports: &[Export],
    imports: &[Import],
    errors: &mut Vec<ValidationError>,
) {
    let mut exported = BTreeSet::new();
    for (index, export) in exports.iter().enumerate() {
        let path = format!("spec.exports[{index}]");
        validate_name(&export.service, &format!("{path}.service"), errors);
        if !services.contains(export.service.as_str()) {
            error(
                errors,
                codes::EXPORT_SERVICE_MISSING,
                &format!("{path}.service"),
                "an export must reference a service in this application",
            );
        }
        if !exported.insert(&export.service) {
            error(
                errors,
                codes::EXPORT_DUPLICATE,
                &format!("{path}.service"),
                "the service is already exported",
            );
        }
        let path = format!("{path}.applications");
        if export.applications.is_empty() {
            error(
                errors,
                codes::EXPORT_APPLICATIONS_EMPTY,
                &path,
                "an export must grant at least one application",
            );
        }
        let mut granted = BTreeSet::new();
        for (position, application) in export.applications.iter().enumerate() {
            let path = format!("{path}[{position}]");
            validate_name(application, &path, errors);
            if !granted.insert(application) {
                error(
                    errors,
                    codes::EXPORT_DUPLICATE,
                    &path,
                    "the application is already granted",
                );
            }
        }
    }
    let mut imported = BTreeSet::new();
    for (index, import) in imports.iter().enumerate() {
        let path = format!("spec.imports[{index}]");
        validate_name(&import.application, &format!("{path}.application"), errors);
        validate_name(&import.service, &format!("{path}.service"), errors);
        if !imported.insert((&import.application, &import.service)) {
            error(
                errors,
                codes::IMPORT_DUPLICATE,
                &format!("{path}.service"),
                "the service is already imported",
            );
        }
        if let Some(environment) = import.environment.as_ref().and_then(Template::as_literal)
            && EnvironmentName::parse(environment).is_err()
        {
            error(
                errors,
                codes::NAME_INVALID,
                &format!("{path}.environment"),
                "environment names must be 1-63 lowercase letters, digits, or hyphens, start with a letter, and end with a letter or digit",
            );
        }
    }
}
