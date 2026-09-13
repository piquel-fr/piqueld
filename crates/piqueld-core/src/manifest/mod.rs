//! Public application manifests and their validated, canonical domain model.

pub mod domain;
pub mod input;

use domain::{ValidatedMetadata, ValidatedSpec};
pub mod validation;

pub use input::{
    ApplicationManifest, ApplicationSpec, Build, GitRepository, HealthCheck, Metadata, Mount,
    RepositoryManifest, ResourceLimits, SecretMount, Service, Source, Volume,
};
pub(crate) use validation::valid_image_reference;
pub use validation::{
    ValidationError, ValidationErrors, parse_json, parse_toml, safe_decode_path, valid_git_commit,
    valid_repository_path,
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
        }
    }
}

fn normalize_spec(spec: &mut ValidatedSpec) {
    spec.services
        .sort_by(|left, right| left.name.cmp(&right.name));
    for service in &mut spec.services {
        service.mounts.sort();
        service.secrets.sort();
    }
    spec.volumes.sort();
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

    /// Reapplies canonical ordering. This operation is idempotent.
    #[must_use]
    pub fn normalize(mut self) -> Self {
        normalize_spec(&mut self.spec);
        self
    }

    /// Versioned SHA-256 over canonical JSON after defaults and normalization.
    ///
    /// The envelope covers only the canonical spec, so cosmetic metadata edits
    /// do not change the hash or redeploy services.
    #[must_use]
    ///
    /// # Panics
    ///
    /// Panics only if the internal normalized manifest cannot be serialized,
    /// which indicates a bug in the domain types.
    pub fn spec_hash(&self) -> String {
        #[derive(Serialize)]
        struct HashEnvelope<'a> {
            hash_version: &'static str,
            spec: &'a ValidatedSpec,
        }
        let mut normalized = self.clone().normalize();
        // Manifest location does not change the desired runtime resources.
        normalized.spec.manifest = None;
        let bytes = serde_json::to_vec(&HashEnvelope {
            hash_version: SPEC_HASH_VERSION,
            spec: &normalized.spec,
        })
        .expect("domain serialization is infallible");
        format!("sha256:{:x}", Sha256::digest(bytes))
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

impl<'de> Deserialize<'de> for NormalizedApplication {
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
        .validate()
        .map(|validated| validated.normalize(wire.id))
        .map_err(serde::de::Error::custom)
    }
}
