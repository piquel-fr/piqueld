//! Environments: the deployable units of an application.
//!
//! An application owns the shared manifest or repository connection. Each
//! environment deploys it, from its own branch when repository-backed, with its
//! own deployment history, status, volumes, generated secrets, routes, and
//! Docker network. A preview is an environment of another kind: a disposable
//! deployment of one branch, configured by `[spec.previews]`.

use crate::manifest::{
    GitRepository, PreviewLimits, RepositoryManifest, ValidationError, ValidationErrors,
    valid_git_branch, valid_git_commit,
};
use crate::{
    ApplicationName, EnvironmentName, GitBranch, NormalizedApplication, PlanDiagnostic,
    PreviewSlot, PreviewSlug,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use utoipa::ToSchema;

/// Name of the environment created with every new application, and of the one
/// each application received when environments were introduced.
pub const DEFAULT_ENVIRONMENT: &str = "production";

/// What an environment is. Behaviour that differs between environments and
/// previews matches on this kind.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EnvironmentKind {
    /// A long-lived environment, configured by `[spec.environments.<name>]`.
    #[default]
    Environment,
    /// A disposable deployment of a branch, configured by `[spec.previews]`.
    /// Boxed, so environment views stay small in the many futures that hold them.
    Preview(Box<Preview>),
}

impl EnvironmentKind {
    /// Whether a successful preparation records a release. Environments do;
    /// previews never do, so they can never be a release's source.
    #[must_use]
    pub const fn records_releases(&self) -> bool {
        match self {
            Self::Environment => true,
            Self::Preview(_) => false,
        }
    }

    /// Bounds the configuration a deployment of this kind renders, returning
    /// a warning for every service it changed. Previews get `limits`' default
    /// CPU and memory limits and replica cap; environments are never bounded.
    #[must_use]
    pub fn bound(
        &self,
        application: &mut NormalizedApplication,
        limits: &PreviewLimits,
    ) -> Vec<PlanDiagnostic> {
        match self {
            Self::Environment => Vec::new(),
            Self::Preview(_) => limits.bound(application),
        }
    }
}

/// A preview's identity. Creation is idempotent on its branch and slot.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, ToSchema)]
pub struct Preview {
    /// Branch of the application's manifest repository it deploys.
    pub branch: GitBranch,
    /// Distinguishes several previews of one branch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<PreviewSlot>,
    /// DNS-safe identifier, `env.slug`, persisted when the preview is created.
    pub slug: PreviewSlug,
}

impl PreviewSlug {
    /// Longest slug, which leaves room in a 63-byte DNS label for a prefix
    /// such as `api-` in `api-${{ env.slug }}.dev.example.com`.
    pub const MAX_LEN: usize = 40;
    /// Hexadecimal digits of the key's hash at the end of every slug.
    const HASH_LEN: usize = 6;

    /// Derives the slug of a preview from `<application>-<branch>[-<slot>]`:
    /// lowercase letters and digits, every other run of characters a single
    /// hyphen, truncated, then a short hash of the exact application, branch,
    /// and slot, so keys that sanitize alike still differ. Derived once and
    /// persisted, so renaming the application keeps it.
    ///
    /// ```
    /// use piqueld_core::{ApplicationName, GitBranch, PreviewSlug};
    /// let notes = ApplicationName::parse("notes").unwrap();
    /// let branch = GitBranch::parse("feat/Login").unwrap();
    /// let slug = PreviewSlug::derive(&notes, &branch, None);
    /// assert!(slug.as_str().starts_with("notes-feat-login-"));
    /// ```
    ///
    /// # Panics
    ///
    /// Never: application names start with a letter, and the hash ends the
    /// slug with a digit or letter.
    #[must_use]
    pub fn derive(
        application: &ApplicationName,
        branch: &GitBranch,
        slot: Option<&PreviewSlot>,
    ) -> Self {
        let mut key = Sha256::new();
        key.update(application.as_str());
        key.update([0]);
        key.update(branch.as_str());
        if let Some(slot) = slot {
            key.update([0]);
            key.update(slot.as_str());
        }
        let hash = format!("{:x}", key.finalize());
        let mut readable = String::new();
        let parts = [application.as_str(), branch.as_str()]
            .into_iter()
            .chain(slot.map(PreviewSlot::as_str));
        for character in parts.collect::<Vec<_>>().join("-").chars() {
            if character.is_ascii_alphanumeric() {
                readable.push(character.to_ascii_lowercase());
            } else if !readable.ends_with('-') {
                readable.push('-');
            }
        }
        readable.truncate(Self::MAX_LEN - Self::HASH_LEN - 1);
        let readable = readable.trim_end_matches('-');
        Self::parse(format!("{readable}-{}", &hash[..Self::HASH_LEN]))
            .expect("derived slugs are valid")
    }
}

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
            sync: connection.sync,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn slug(branch: &str, slot: Option<&str>) -> PreviewSlug {
        PreviewSlug::derive(
            &ApplicationName::parse("notes").unwrap(),
            &GitBranch::parse(branch).unwrap(),
            slot.map(|slot| PreviewSlot::parse(slot).unwrap()).as_ref(),
        )
    }

    #[test]
    fn preview_slugs_are_sanitized_truncated_and_hashed_over_the_exact_key() {
        // Stable across releases: slugs are persisted, and hostnames use them.
        assert_eq!(
            slug("feat/Login_Page", None).as_str(),
            "notes-feat-login-page-6fdeb6"
        );
        assert_eq!(
            slug("feat/login", Some("agent-2")).as_str(),
            "notes-feat-login-agent-2-503aa8"
        );
        let long = slug(
            &format!("feature/{}", ["very-long-name"; 8].join("/")),
            None,
        );
        assert_eq!(long.as_str().len(), PreviewSlug::MAX_LEN);
        assert!(
            long.as_str()
                .starts_with("notes-feature-very-long-name-very-")
        );
        assert!(!long.as_str().contains("--"));
        // Keys that sanitize alike still get distinct slugs.
        let alike = [
            slug("feat/login", None),
            slug("feat-login", None),
            slug("Feat/Login", None),
            slug("feat/login-agent", None),
            slug("feat/login", Some("agent")),
        ];
        let distinct = alike
            .iter()
            .map(PreviewSlug::as_str)
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(distinct.len(), alike.len());
        for slug in &alike {
            assert!(slug.as_str().starts_with("notes-feat-login-"), "{slug}");
            assert!(EnvironmentName::parse(slug.as_str()).is_ok());
        }
    }
}
