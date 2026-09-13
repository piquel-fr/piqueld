//! Public application manifests and their validated, canonical domain model.

pub mod input;
pub mod validation;

pub use input::{
    ApplicationManifest, ApplicationSpec, Build, GitRepository, HealthCheck, Metadata, Mount,
    RepositoryManifest, ResourceLimits, Service, Source, Volume,
};
pub(crate) use validation::valid_image_reference;
pub use validation::{
    ValidationError, ValidationErrors, parse_json, parse_toml, safe_decode_path, valid_git_commit,
    valid_repository_path,
};

use crate::ApplicationId;
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
    name: String,
    spec: ApplicationSpec,
}

/// Canonical desired application plus persistence-assigned identity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct NormalizedApplication {
    /// Stable application identity.
    pub id: ApplicationId,
    /// API version string.
    pub api_version: String,
    /// Resource kind string.
    pub kind: String,
    /// Canonical metadata.
    pub metadata: Metadata,
    /// Canonical resource specification.
    pub spec: ApplicationSpec,
}

impl ValidatedApplication {
    /// Returns the editable application name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Returns the validated specification before canonical ordering.
    #[must_use]
    pub fn spec(&self) -> &ApplicationSpec {
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
            metadata: Metadata { name: self.name },
            spec,
        }
    }
}

fn normalize_spec(spec: &mut ApplicationSpec) {
    spec.services
        .sort_by(|left, right| left.name.cmp(&right.name));
    for service in &mut spec.services {
        service.mounts.sort();
    }
    spec.volumes.sort();
}

impl NormalizedApplication {
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
            spec: &'a ApplicationSpec,
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

    fn to_manifest(&self) -> ApplicationManifest {
        ApplicationManifest {
            api_version: self.api_version.clone(),
            kind: self.kind.clone(),
            metadata: self.metadata.clone(),
            spec: self.spec.clone(),
        }
    }
}
