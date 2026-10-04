//! Environments: the deployable units of an application.
//!
//! An application owns the shared manifest. Each environment deploys it with
//! its own deployment history, status, volumes, generated secrets, routes, and
//! Docker network.

use crate::{EnvironmentName, NormalizedApplication};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Name of the environment created with every new application, and of the one
/// each application received when environments were introduced.
pub const DEFAULT_ENVIRONMENT: &str = "production";

/// Where an environment's deployments come from.
///
/// Every environment currently follows its application's manifest connection:
/// per-environment branches and promotion add their own sources later.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentSource {
    /// The application's saved manifest, with no repository backing.
    Saved,
    /// The application's manifest repository, at the revision `spec.manifest` names.
    Repository,
}

impl EnvironmentSource {
    /// The source an application's environments deploy from.
    #[must_use]
    pub fn of(application: &NormalizedApplication) -> Self {
        if application.spec().manifest.is_some() {
            Self::Repository
        } else {
            Self::Saved
        }
    }
}

impl EnvironmentName {
    /// The default environment name, `production`.
    ///
    /// # Panics
    ///
    /// Never: the constant is a valid logical name.
    #[must_use]
    pub fn default_name() -> Self {
        Self::parse(DEFAULT_ENVIRONMENT).expect("the default environment name is valid")
    }
}
