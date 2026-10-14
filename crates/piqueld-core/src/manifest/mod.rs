//! Public application manifests and their validated, canonical domain model.

pub mod declaration;
pub(crate) mod dependencies;
pub mod domain;
pub mod files;
pub mod input;
pub mod limits;
pub mod rollout;
pub mod routes;
pub mod template;
/// Shared tests for manifest variables and templates.
#[cfg(test)]
mod template_tests;
pub mod variables;
pub use declaration::{Declaration, DeclaredSource, EnvironmentChange};
pub use domain::{
    HealthExecution, SecretSource, ValidatedBuild, ValidatedHealthCheck, ValidatedResourceLimits,
    ValidatedSecretMount, ValidatedSource,
};
pub use files::{
    FileCaptureError, FileMode, FileModeError, FileSource, RepositoryFiles, ServiceFile,
};
pub use limits::{PreviewLimits, PreviewLimitsError};
pub use rollout::{Rollout, RolloutOrder, RolloutOrderSource, RolloutPolicy, ValidatedRollout};
pub use routes::{
    Hostname, RedirectStatus, RedirectUrl, RouteHostname, RouteTarget, ValidatedRedirect,
    ValidatedRoute, Visibility, VisibilityError,
};
pub use template::{ApplicationTemplate, Rendering, ValidatedTemplate};
pub use variables::{
    GitRevision, PREVIEW_DEPLOYMENT_ID, RenderContext, RenderTarget, SystemVariable, Template,
    TemplateError, Typed, VariableValue,
};

use domain::{ValidatedMetadata, ValidatedSpec};
pub mod validation;

pub use input::{
    ApplicationManifest, ApplicationSpec, Build, EnvironmentConfig, GitRepository, HealthCheck,
    Job, JobRun, JobVolumeAccess, ManifestRepository, ManifestRevision, Metadata, Mount,
    PreviewConfig, Redirect, RepositoryManifest, ResourceLimits, Route, SecretDeclaration,
    SecretEncoding, SecretGenerator, SecretMount, Service, Source, SourceRepository, Variable,
    Volume,
};
pub(crate) use validation::valid_image_reference;
pub use validation::{
    ValidationError, ValidationErrors, parse_json, parse_template_json, parse_template_toml,
    parse_toml, safe_decode_path, valid_git_branch, valid_git_commit, valid_repository_path,
};

use crate::{ApplicationId, ApplicationName};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use utoipa::ToSchema;

/// The supported application API version.
pub const APPLICATION_API_VERSION: &str = "piqueld.dev/v1alpha1";
/// The supported manifest resource kind.
pub const APPLICATION_KIND: &str = "Application";
/// Envelope version for the specification hash. Version 2 hashes only the
/// canonical spec, so cosmetic metadata changes no longer redeploy services.
pub const SPEC_HASH_VERSION: &str = "piqueld-spec-hash/v2";

/// Validated domain application before canonical collection ordering.
#[derive(Clone, Debug, PartialEq)]
pub struct ValidatedApplication {
    metadata: ValidatedMetadata,
    spec: ValidatedSpec,
}

/// Canonical desired application plus persistence-assigned identity.
///
/// Configuration is immutable. Export input with [`Self::to_manifest`] and
/// validate edits before constructing a replacement.
///
/// ```compile_fail
/// fn bypass_validation(app: &mut piqueld_core::NormalizedApplication) {
///     app.spec.services.clear();
/// }
/// ```
#[derive(Clone, Debug, Eq, PartialEq, Serialize, ToSchema)]
pub struct NormalizedApplication {
    /// Stable application identity.
    id: ApplicationId,
    /// API version string.
    api_version: String,
    /// Resource kind string.
    kind: String,
    /// Canonical metadata.
    metadata: ValidatedMetadata,
    /// Canonical resource specification.
    spec: ValidatedSpec,
    /// Contents of the repository files services mount, as captured.
    #[serde(skip_serializing_if = "RepositoryFiles::is_empty")]
    files: RepositoryFiles,
}

impl ValidatedApplication {
    /// Returns the editable application name.
    #[must_use]
    pub fn name(&self) -> &ApplicationName {
        &self.metadata.name
    }

    /// Returns the validated specification before canonical ordering.
    #[must_use]
    pub fn spec(&self) -> &ValidatedSpec {
        &self.spec
    }

    /// Canonicalizes unordered collections and attaches a stable ID.
    #[must_use]
    pub fn normalize(self, id: ApplicationId) -> NormalizedApplication {
        let mut spec = self.spec;
        normalize_spec(&mut spec);
        NormalizedApplication {
            id,
            api_version: APPLICATION_API_VERSION.into(),
            kind: APPLICATION_KIND.into(),
            metadata: self.metadata,
            spec,
            files: RepositoryFiles::default(),
        }
    }
}

/// Sorts every order-insensitive collection (services by name, mounts,
/// secrets, volumes, routes) so equivalent manifests serialize and hash
/// identically. Process arguments and other ordered lists are untouched.
fn normalize_spec(spec: &mut ValidatedSpec) {
    spec.services
        .sort_by(|left, right| left.name.cmp(&right.name));
    for service in &mut spec.services {
        service.mounts.sort();
        service.secrets.sort();
        service.files.sort();
        service.depends_on.sort();
    }
    spec.volumes.sort();
    spec.routes.sort();
    spec.secrets.sort();
}

impl NormalizedApplication {
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

    /// Returns the canonical, validated configuration.
    #[must_use]
    pub fn spec(&self) -> &ValidatedSpec {
        &self.spec
    }

    /// Returns the captured contents of the repository files services mount.
    #[must_use]
    pub const fn files(&self) -> &RepositoryFiles {
        &self.files
    }

    /// Attaches the repository files captured with the manifest this
    /// application was rendered from.
    #[must_use]
    pub(crate) fn with_files(mut self, files: RepositoryFiles) -> Self {
        self.files = files;
        self
    }

    /// Rebinds storage identity without changing validated configuration.
    #[must_use]
    pub fn with_id(mut self, id: ApplicationId) -> Self {
        self.id = id;
        self
    }

    /// Changes display identity without changing the runtime specification.
    #[must_use]
    pub fn with_name(mut self, name: ApplicationName) -> Self {
        self.metadata.name = name;
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

    /// Pins `"self"` Git sources to `commit`, the revision this manifest was read
    /// from. Deployments prepare the pinned application; saved configuration
    /// keeps `"self"`.
    #[must_use]
    pub fn pin_manifest_sources(mut self, commit: &str) -> Self {
        if let Some(manifest) = &self.spec.manifest {
            let repository = manifest
                .repository
                .at(&ManifestRevision::Commit(commit.into()));
            for service in &mut self.spec.services {
                service.source.resolve_manifest_repository(&repository);
            }
        }
        self
    }

    /// Reapplies canonical ordering. This operation is idempotent.
    #[must_use]
    pub fn normalize(mut self) -> Self {
        normalize_spec(&mut self.spec);
        self
    }

    /// Versioned SHA-256 over canonical JSON after defaults and normalization.
    ///
    /// The envelope covers only the canonical spec and the captured contents
    /// of repository files, so cosmetic metadata edits do not change the hash
    /// or redeploy services.
    #[must_use]
    ///
    /// # Panics
    ///
    /// Panics only if the internal normalized manifest cannot be serialized,
    /// which indicates a bug in the domain types.
    pub fn spec_hash(&self) -> String {
        let mut normalized = self.clone().normalize();
        // Manifest location does not change the desired runtime resources.
        normalized.spec.manifest = None;
        hash_spec(&normalized.spec, &normalized.files)
    }

    /// Canonical JSON representation used for durable desired state.
    ///
    /// # Errors
    ///
    /// Returns a serialization error if the normalized manifest cannot be
    /// represented as JSON.
    pub fn canonical_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&self.clone().normalize())
    }

    /// Portable desired TOML representation.
    ///
    /// # Errors
    ///
    /// Returns a serialization error if the normalized manifest cannot be
    /// represented as TOML.
    pub fn export_toml(&self) -> Result<String, toml::ser::Error> {
        toml::to_string_pretty(&self.clone().normalize().to_manifest())
    }

    /// Exports editable input; call `validate` after changing configuration.
    #[must_use]
    pub fn to_manifest(&self) -> ApplicationManifest {
        ApplicationManifest {
            api_version: self.api_version.clone(),
            kind: self.kind.clone(),
            metadata: Metadata {
                name: self.metadata.name.to_string(),
            },
            spec: self.spec.to_input(),
        }
    }
}

/// Versioned SHA-256 over a specification's canonical JSON and the captured
/// repository files it mounts. Without files, the envelope is unchanged from
/// before files existed, so existing hashes stay stable.
///
/// # Panics
///
/// Panics only if the specification cannot be serialized, which indicates a
/// bug in the manifest types.
fn hash_spec(spec: &impl Serialize, files: &RepositoryFiles) -> String {
    #[derive(Serialize)]
    struct HashEnvelope<'a, S> {
        hash_version: &'static str,
        spec: &'a S,
        #[serde(skip_serializing_if = "RepositoryFiles::is_empty")]
        files: &'a RepositoryFiles,
    }
    let bytes = serde_json::to_vec(&HashEnvelope {
        hash_version: SPEC_HASH_VERSION,
        spec,
        files,
    })
    .expect("manifest serialization is infallible");
    format!("sha256:{:x}", Sha256::digest(bytes))
}

// Deserialization re-runs full manifest validation and normalization, so
// stored or received JSON can never produce an unvalidated application.
impl<'de> Deserialize<'de> for NormalizedApplication {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        /// Escapes `${{` in every string: rendered values are literal text,
        /// never references.
        fn escape(value: &mut serde_json::Value) {
            match value {
                serde_json::Value::String(text) if Template::mentions_reference(text) => {
                    *text = Template::literal(text).as_str().into();
                }
                serde_json::Value::Array(values) => values.iter_mut().for_each(escape),
                serde_json::Value::Object(values) => values.values_mut().for_each(escape),
                _ => {}
            }
        }

        /// The `files` of every service in `spec`, which are never templates,
        /// so they skip [`escape`] and keep their contents as they are.
        fn services(
            spec: &mut serde_json::Value,
        ) -> impl Iterator<Item = &mut serde_json::Map<String, serde_json::Value>> {
            spec.get_mut("services")
                .and_then(serde_json::Value::as_array_mut)
                .into_iter()
                .flatten()
                .filter_map(serde_json::Value::as_object_mut)
        }

        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            id: ApplicationId,
            api_version: String,
            kind: String,
            metadata: Metadata,
            spec: serde_json::Value,
            #[serde(default)]
            files: RepositoryFiles,
        }
        let mut wire = Wire::deserialize(deserializer)?;
        let files = services(&mut wire.spec)
            .map(|service| service.remove("files"))
            .collect::<Vec<_>>();
        escape(&mut wire.spec);
        for (service, files) in services(&mut wire.spec).zip(files) {
            if let Some(files) = files {
                service.insert("files".into(), files);
            }
        }
        ApplicationManifest {
            api_version: wire.api_version,
            kind: wire.kind,
            metadata: wire.metadata,
            spec: ApplicationSpec::deserialize(wire.spec).map_err(serde::de::Error::custom)?,
        }
        .validate()
        .map(|validated| validated.normalize(wire.id).with_files(wire.files))
        .map_err(serde::de::Error::custom)
    }
}
