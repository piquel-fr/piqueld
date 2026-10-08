//! Environments: the deployable units of an application.
//!
//! An application owns the shared manifest or repository connection. Each
//! environment deploys it, from its own branch when repository-backed, with its
//! own deployment history, status, volumes, generated secrets, routes, and
//! Docker network.

use crate::EnvironmentName;
use crate::manifest::{
    GitRepository, RepositoryManifest, ValidationError, ValidationErrors, valid_git_branch,
    valid_git_commit,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Name of the environment created with every new application, and of the one
/// each application received when environments were introduced.
pub const DEFAULT_ENVIRONMENT: &str = "production";

/// Where an environment's deployments come from.
///
/// Environments of an application without a manifest repository deploy its
/// saved manifest. Environments of a repository-backed application each
/// follow a branch of that repository: its URL and manifest path belong to
/// the application, the branch to the environment.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EnvironmentSource {
    /// The application's saved manifest.
    Saved,
    /// The manifest file on a branch of the application's manifest repository.
    Branch(TrackedBranch),
}

impl EnvironmentSource {
    /// The source of a new environment of an application whose manifest
    /// repository is `connection`: `branch`, by default the branch and pinned
    /// commit `connection` names. Applications without a repository deploy
    /// their saved manifest.
    ///
    /// # Errors
    ///
    /// Returns `manifest_repository_required` for a branch without a
    /// repository, and the errors of [`TrackedBranch::of`].
    pub fn select(
        connection: Option<&RepositoryManifest>,
        branch: Option<TrackedBranch>,
    ) -> Result<Self, ValidationErrors> {
        match (connection, branch) {
            (Some(_), Some(branch)) => Ok(Self::Branch(branch)),
            (Some(connection), None) => {
                TrackedBranch::of(&connection.repository).map(Self::Branch)
            }
            (None, None) => Ok(Self::Saved),
            (None, Some(_)) => Err(ValidationErrors(vec![ValidationError {
                code: "manifest_repository_required".into(),
                path: "branch".into(),
                message: "only environments of a repository-backed application follow a branch; connect a manifest repository first".into(),
            }])),
        }
    }

    /// Whether a successful preparation records a release. Only environments
    /// that build their own source do; previews will not, since they are
    /// never promoted.
    #[must_use]
    pub const fn records_releases(&self) -> bool {
        match self {
            Self::Saved | Self::Branch(_) => true,
        }
    }

    /// The branch this environment follows, if repository-backed.
    #[must_use]
    pub const fn branch(&self) -> Option<&TrackedBranch> {
        match self {
            Self::Saved => None,
            Self::Branch(branch) => Some(branch),
        }
    }
}

/// Describes the source in a sentence.
///
/// ```text
/// the saved manifest    branch main    branch main@0123…
/// ```
impl std::fmt::Display for EnvironmentSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Saved => formatter.write_str("the saved manifest"),
            Self::Branch(branch) => write!(formatter, "branch {branch}"),
        }
    }
}

/// A branch of the application's manifest repository that an environment
/// follows, optionally pinned to one commit. Always a valid Git branch name
/// and, when pinned, a full commit hash.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, ToSchema)]
pub struct TrackedBranch {
    /// Branch whose head is fetched; also `git.branch`.
    branch: String,
    /// Full commit fetched instead of the branch head, when pinned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    commit: Option<String>,
}

impl TrackedBranch {
    /// Validates a branch name and optional full commit hash.
    ///
    /// # Errors
    ///
    /// Returns `git_branch_invalid` at `branch` and `git_commit_invalid` at
    /// `commit`.
    pub fn new(branch: String, commit: Option<String>) -> Result<Self, ValidationErrors> {
        let mut errors = Vec::new();
        if !valid_git_branch(&branch) {
            errors.push(ValidationError {
                code: "git_branch_invalid".into(),
                path: "branch".into(),
                message: "branch must be a valid Git branch name".into(),
            });
        }
        if commit
            .as_deref()
            .is_some_and(|commit| !valid_git_commit(commit))
        {
            errors.push(ValidationError {
                code: "git_commit_invalid".into(),
                path: "commit".into(),
                message: "commit must be a full lowercase hexadecimal Git hash".into(),
            });
        }
        if errors.is_empty() {
            Ok(Self { branch, commit })
        } else {
            Err(ValidationErrors(errors))
        }
    }

    /// The branch and pinned commit `repository` names.
    ///
    /// # Errors
    ///
    /// Returns the errors of [`Self::new`].
    pub fn of(repository: &GitRepository) -> Result<Self, ValidationErrors> {
        Self::new(repository.branch.clone(), repository.commit.clone())
    }

    /// The branch name.
    #[must_use]
    pub fn branch(&self) -> &str {
        &self.branch
    }

    /// The pinned commit, if any.
    #[must_use]
    pub fn commit(&self) -> Option<&str> {
        self.commit.as_deref()
    }

    /// The application's `connection` at this branch: what a deployment fetches.
    #[must_use]
    pub fn in_repository(&self, connection: &RepositoryManifest) -> RepositoryManifest {
        RepositoryManifest {
            repository: GitRepository {
                url: connection.repository.url.clone(),
                branch: self.branch.clone(),
                commit: self.commit.clone(),
            },
            path: connection.path.clone(),
        }
    }
}

/// Shows the branch, and the pinned commit when there is one.
///
/// ```text
/// main    main@0123456789abcdef0123456789abcdef01234567
/// ```
impl std::fmt::Display for TrackedBranch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.commit {
            Some(commit) => write!(formatter, "{}@{commit}", self.branch),
            None => formatter.write_str(&self.branch),
        }
    }
}

// Deserialization validates, so stored or received branches are always valid.
impl<'de> Deserialize<'de> for TrackedBranch {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            branch: String,
            #[serde(default)]
            commit: Option<String>,
        }
        let wire = Wire::deserialize(deserializer)?;
        Self::new(wire.branch, wire.commit).map_err(serde::de::Error::custom)
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
