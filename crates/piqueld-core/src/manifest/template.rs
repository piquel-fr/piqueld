//! Saved manifests whose values may still reference variables, and rendering
//! them into the configuration one environment deploys.

use super::domain::{ValidatedMetadata, ValidatedSpec};
use super::variables::{RenderContext, VariableValue};
use super::{
    APPLICATION_API_VERSION, APPLICATION_KIND, ApplicationManifest, ApplicationSpec, Hostname,
    ManifestRevision, Metadata, NormalizedApplication, RepositoryManifest, ValidatedApplication,
    ValidationError, ValidationErrors,
};
use crate::{ApplicationId, ApplicationName, EnvironmentName, codes};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use utoipa::ToSchema;

/// A manifest validated as far as its literal values allow. Values that
/// reference variables are validated once rendered for an environment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ValidatedTemplate {
    metadata: ValidatedMetadata,
    spec: ApplicationSpec,
}

/// A saved manifest in canonical order, with its persistence-assigned
/// identity. Its values may reference variables: [`Self::render`] produces the
/// [`NormalizedApplication`] one environment deploys.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, ToSchema)]
pub struct ApplicationTemplate {
    /// Stable application identity.
    id: ApplicationId,
    /// API version string.
    api_version: String,
    /// Resource kind string.
    kind: String,
    /// Canonical metadata.
    metadata: ValidatedMetadata,
    /// Resource specification, with references unresolved.
    spec: ApplicationSpec,
}

/// A template rendered for one environment: what a deployment captures.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Rendering {
    /// The rendered, validated configuration.
    pub application: NormalizedApplication,
    /// The value of every variable in scope, keyed by reference, e.g.
    /// `vars.domain` or `env.name`.
    pub values: BTreeMap<String, VariableValue>,
}

impl ValidatedTemplate {
    pub(super) const fn new(metadata: ValidatedMetadata, spec: ApplicationSpec) -> Self {
        Self { metadata, spec }
    }

    /// Returns the editable application name.
    #[must_use]
    pub fn name(&self) -> &ApplicationName {
        &self.metadata.name
    }

    /// Returns the validated specification, with references unresolved.
    #[must_use]
    pub fn spec(&self) -> &ApplicationSpec {
        &self.spec
    }

    /// Canonicalizes unordered collections and attaches a stable ID.
    #[must_use]
    pub fn normalize(mut self, id: ApplicationId) -> ApplicationTemplate {
        let spec = &mut self.spec;
        spec.services
            .sort_by(|left, right| left.name.cmp(&right.name));
        for service in &mut spec.services {
            service.mounts.sort();
            service.secrets.sort();
            service.depends_on.sort();
        }
        spec.volumes.sort();
        spec.routes.sort();
        spec.secrets.sort();
        ApplicationTemplate {
            id,
            api_version: APPLICATION_API_VERSION.into(),
            kind: APPLICATION_KIND.into(),
            metadata: self.metadata,
            spec: self.spec,
        }
    }

    /// Converts a template that references no variables into the domain model.
    ///
    /// # Errors
    ///
    /// Returns `variable_unresolved` for every value that references variables.
    pub fn into_literal(mut self) -> Result<ValidatedApplication, ValidationErrors> {
        let mut errors = Vec::new();
        self.spec.visit_values(&mut |path, slot| {
            if slot
                .template()
                .is_some_and(|template| template.as_literal().is_none())
            {
                errors.push(ValidationError {
                    code: codes::VARIABLE_UNRESOLVED.into(),
                    path: path.into(),
                    message: "references must be rendered for an environment first".into(),
                });
            }
        });
        if !errors.is_empty() {
            return Err(ValidationErrors::sorted(errors));
        }
        Ok(ValidatedApplication {
            metadata: self.metadata,
            spec: ValidatedSpec::from_input(self.spec)?,
        })
    }
}

impl ApplicationTemplate {
    /// Returns the storage-assigned application identity.
    #[must_use]
    pub fn id(&self) -> &ApplicationId {
        &self.id
    }

    /// Returns validated application metadata.
    #[must_use]
    pub fn metadata(&self) -> &ValidatedMetadata {
        &self.metadata
    }

    /// Returns the specification, with references unresolved.
    #[must_use]
    pub fn spec(&self) -> &ApplicationSpec {
        &self.spec
    }

    /// Rebinds storage identity without changing configuration.
    #[must_use]
    pub fn with_id(mut self, id: ApplicationId) -> Self {
        self.id = id;
        self
    }

    /// Changes display identity without changing configuration.
    #[must_use]
    pub fn with_name(mut self, name: ApplicationName) -> Self {
        self.metadata.name = name;
        self
    }

    /// Replaces `spec.manifest`, the repository the manifest is fetched from.
    /// Fetched manifests carry the repository they were actually read from,
    /// whatever their own `spec.manifest` says.
    #[must_use]
    pub fn with_manifest(mut self, manifest: Option<RepositoryManifest>) -> Self {
        self.spec.manifest = manifest;
        self
    }

    /// Fetches the manifest from another branch or commit for one deployment.
    ///
    /// # Errors
    ///
    /// Rejects applications without a manifest repository and invalid revisions.
    pub fn with_manifest_revision(
        mut self,
        revision: &ManifestRevision,
    ) -> Result<Self, ValidationErrors> {
        let mut errors = Vec::new();
        let path = "spec.manifest.repository";
        match &mut self.spec.manifest {
            Some(manifest) => {
                manifest.repository = manifest.repository.at(revision);
                manifest.repository.validate(path, &mut errors);
            }
            None => ManifestRevision::unbacked(path, &mut errors),
        }
        if errors.is_empty() {
            Ok(self)
        } else {
            Err(ValidationErrors(errors))
        }
    }

    /// Exports editable input; validate it again after changing configuration.
    #[must_use]
    pub fn to_manifest(&self) -> ApplicationManifest {
        ApplicationManifest {
            api_version: self.api_version.clone(),
            kind: self.kind.clone(),
            metadata: Metadata {
                name: self.metadata.name.to_string(),
            },
            spec: self.spec.clone(),
        }
    }

    /// Renders the configuration `context` deploys, then validates it.
    ///
    /// # Errors
    ///
    /// Returns every reference without a value for the environment, every
    /// value of the wrong type, and validation errors of the rendered values.
    pub fn render(&self, context: &RenderContext) -> Result<Rendering, ValidationErrors> {
        let mut manifest = self.to_manifest();
        let mut errors = Vec::new();
        let values = manifest
            .spec
            .render(self.metadata.name.as_str(), context, &mut errors);
        if !errors.is_empty() {
            return Err(ValidationErrors::sorted(errors));
        }
        Ok(Rendering {
            application: manifest.validate()?.normalize(self.id.clone()),
            values,
        })
    }

    /// The hostnames `environment` routes before anything is deployed, from
    /// the routes whose hostname renders without a deployment.
    #[must_use]
    pub fn hostnames(&self, environment: &EnvironmentName) -> Vec<Hostname> {
        self.saved(environment)
            .routes
            .iter()
            .filter_map(|route| route.hostname.as_literal())
            .filter_map(|hostname| {
                Hostname::parse(
                    hostname
                        .strip_suffix('.')
                        .unwrap_or(&hostname)
                        .to_ascii_lowercase(),
                )
                .ok()
            })
            .collect()
    }

    /// Whether `environment` renders the same saved configuration as
    /// `renamed` does in `other`. Renames keep an environment resolved only
    /// when this holds, since `${{ app.name }}` and `${{ env.name }}` read names.
    #[must_use]
    pub fn renders_like(
        &self,
        environment: &EnvironmentName,
        other: &Self,
        renamed: &EnvironmentName,
    ) -> bool {
        self.saved(environment) == other.saved(renamed)
    }

    /// `environment`'s saved configuration, rendered outside any deployment.
    /// Values that only render when deploying stay templates.
    fn saved(&self, environment: &EnvironmentName) -> ApplicationSpec {
        let mut spec = self.spec.clone();
        spec.render(
            self.metadata.name.as_str(),
            &RenderContext::saved(environment.clone()),
            &mut Vec::new(),
        );
        spec
    }

    /// Each declared variable's value in `environment`'s saved configuration,
    /// keyed by name, or `None` when it has none there.
    #[must_use]
    pub fn values(&self, environment: &EnvironmentName) -> BTreeMap<String, Option<VariableValue>> {
        self.spec.values(
            self.metadata.name.as_str(),
            &RenderContext::saved(environment.clone()),
        )
    }

    /// Whether `[spec.environments.<environment>]` configures the environment.
    #[must_use]
    pub fn configures(&self, environment: &EnvironmentName) -> bool {
        self.spec.environments.contains_key(environment.as_str())
    }

    /// Versioned SHA-256 over the canonical specification, excluding the
    /// manifest location. Equal for templates that render identically in
    /// every environment.
    #[must_use]
    pub fn spec_hash(&self) -> String {
        let mut spec = self.spec.clone();
        spec.manifest = None;
        super::hash_spec(&spec)
    }

    /// Canonical JSON representation used for durable intent.
    ///
    /// # Errors
    ///
    /// Returns a serialization error if the template cannot be represented as JSON.
    pub fn canonical_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(self)
    }

    /// Portable TOML representation, as written.
    ///
    /// # Errors
    ///
    /// Returns a serialization error if the template cannot be represented as TOML.
    pub fn export_toml(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(&self.to_manifest())
    }
}

/// A literal application is a template that references nothing.
impl From<&NormalizedApplication> for ApplicationTemplate {
    fn from(application: &NormalizedApplication) -> Self {
        Self {
            id: application.id().clone(),
            api_version: APPLICATION_API_VERSION.into(),
            kind: APPLICATION_KIND.into(),
            metadata: application.metadata().clone(),
            spec: application.spec().to_input(),
        }
    }
}

// Deserialization re-runs template validation and normalization, so stored or
// received JSON can never produce an unvalidated template.
impl<'de> Deserialize<'de> for ApplicationTemplate {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            id: ApplicationId,
            api_version: String,
            kind: String,
            metadata: Metadata,
            spec: ApplicationSpec,
        }
        let wire = Wire::deserialize(deserializer)?;
        ApplicationManifest {
            api_version: wire.api_version,
            kind: wire.kind,
            metadata: wire.metadata,
            spec: wire.spec,
        }
        .validate_template()
        .map(|validated| validated.normalize(wire.id))
        .map_err(serde::de::Error::custom)
    }
}

impl ValidationErrors {
    /// Orders errors by path, then code.
    pub(super) fn sorted(mut errors: Vec<ValidationError>) -> Self {
        errors.sort_by(|left, right| left.path.cmp(&right.path).then(left.code.cmp(&right.code)));
        Self(errors)
    }
}
