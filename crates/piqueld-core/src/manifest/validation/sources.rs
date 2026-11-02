//! Validation of where an application comes from: builds, Git repositories, branches and commits.

use super::super::variables::Template;
use super::super::{
    Build, GitRepository, ManifestRevision, RepositoryManifest, Source, SourceRepository,
};
use super::{MAX_IDENTIFIER_BYTES, ValidationError, ValidationErrors, VariableMap};
use crate::codes;

/// Build arguments are passed on the `docker build` command line, whose
/// arguments and environment share the kernel's ~2 MiB `ARG_MAX`.
const MAX_BUILD_ARG_TOTAL_BYTES: usize = 262_144;

impl Build {
    /// Validates build inputs without reading the checkout.
    ///
    /// Appends errors at `path` for Dockerfile or context paths that are not
    /// relative or leave the repository, build arguments that break the
    /// environment variable name, count, and size rules or together exceed
    /// their command-line budget, and targets that are not Docker stage names.
    pub fn validate(&self, path: &str, errors: &mut Vec<ValidationError>) {
        let Self::Docker {
            dockerfile,
            context,
            args,
            target,
        } = self;
        for (field, value) in [("dockerfile", dockerfile), ("context", context)] {
            if value
                .as_literal()
                .is_some_and(|value| !valid_repository_path(&value))
            {
                errors.push(ValidationError::new(
                    codes::REPOSITORY_PATH_INVALID,
                    format!("{path}.{field}"),
                    "path must be relative to the repository root and remain within it",
                ));
            }
        }
        VariableMap::BUILD_ARGS.validate(args, path, errors);
        // Each argument is passed as `KEY=VALUE`; references count once rendered.
        let total: usize = args
            .iter()
            .filter_map(|(key, value)| Some(key.len() + value.as_literal()?.len() + 1))
            .sum();
        if total > MAX_BUILD_ARG_TOTAL_BYTES {
            errors.push(ValidationError::new(
                codes::BUILD_ARG_TOTAL_EXCESSIVE,
                format!("{path}.args"),
                format!("build arguments must total at most {MAX_BUILD_ARG_TOTAL_BYTES} bytes"),
            ));
        }
        if target
            .as_ref()
            .and_then(Template::as_literal)
            .is_some_and(|target| !valid_build_target(&target))
        {
            errors.push(ValidationError::new(
                codes::BUILD_TARGET_INVALID,
                format!("{path}.target"),
                format!(
                    "build target must start with a letter, use letters, digits, '-', '_', or '.', and be at most {MAX_IDENTIFIER_BYTES} bytes"
                ),
            ));
        }
    }
}

impl GitRepository {
    /// Validates Git arguments without executing Git.
    ///
    /// Appends errors at `path` for inline URL credentials (any `http(s)`
    /// userinfo, or `user:password@` in other schemes), empty or option-like
    /// URLs, branch names failing `git check-ref-format` style rules, and commits
    /// that are not full lowercase hashes. Leading `-` is rejected so values
    /// cannot be parsed as Git options.
    pub fn validate(&self, path: &str, errors: &mut Vec<ValidationError>) {
        if [Some(&self.url), Some(&self.branch), self.commit.as_ref()]
            .into_iter()
            .flatten()
            .any(|value| Template::mentions_reference(value))
        {
            errors.push(ValidationError::new(
                codes::VARIABLE_NOT_ALLOWED,
                path,
                "repository URLs, branches, and commits cannot reference variables",
            ));
        }
        let inline_credentials = self.url.split_once("://").is_some_and(|(scheme, rest)| {
            let authority = rest.split(['/', '?', '#', '\\']).next().unwrap_or_default();
            authority.rsplit_once('@').is_some_and(|(userinfo, _)| {
                scheme.eq_ignore_ascii_case("http")
                    || scheme.eq_ignore_ascii_case("https")
                    || userinfo.contains(':')
            })
        });
        if inline_credentials {
            errors.push(ValidationError::new(
                codes::GIT_CREDENTIALS_INLINE,
                path,
                "configure Git credentials on the host, not in repository URLs",
            ));
        }
        if self.url.is_empty()
            || self.url.len() > 4096
            || self.url.starts_with('-')
            || self.url.chars().any(char::is_control)
        {
            errors.push(ValidationError::new(
                codes::GIT_REPOSITORY_INVALID,
                path,
                "repository URL must be a nonempty Git location",
            ));
        }
        if let Err(invalid) = crate::TrackedBranch::new(self.branch.clone(), self.commit.clone()) {
            errors.extend(invalid.0.into_iter().map(|error| ValidationError {
                path: path.into(),
                ..error
            }));
        }
    }
}

impl ManifestRevision {
    /// Points `manifest`, a specification's repository connection, at this
    /// revision for one deployment.
    ///
    /// # Errors
    /// Rejects applications without a manifest repository and invalid revisions.
    pub(crate) fn apply(
        &self,
        manifest: &mut Option<RepositoryManifest>,
    ) -> Result<(), ValidationErrors> {
        let mut errors = Vec::new();
        let path = "spec.manifest.repository";
        match manifest {
            Some(manifest) => {
                manifest.repository = manifest.repository.at(self);
                manifest.repository.validate(path, &mut errors);
            }
            None => Self::unbacked(path, &mut errors),
        }
        if errors.is_empty() {
            Ok(())
        } else {
            Err(ValidationErrors(errors))
        }
    }

    /// Records that `path` needs a repository-backed manifest.
    pub(crate) fn unbacked(path: &str, errors: &mut Vec<ValidationError>) {
        errors.push(ValidationError::new(
            codes::MANIFEST_REPOSITORY_REQUIRED,
            path,
            "the manifest repository applies only when spec.manifest is configured",
        ));
    }
}

/// Whether a value is a branch name `git check-ref-format --branch` would
/// accept, and cannot be read as a Git option.
///
/// ```text
/// "main", "release/2.0"                     -> valid
/// "-x", "/main", "a..b", "a/.hidden", "x.lock" -> invalid
/// ```
#[must_use]
pub fn valid_git_branch(branch: &str) -> bool {
    !(branch.is_empty()
        || branch.len() > 255
        || branch == "@"
        || branch.starts_with(['-', '/'])
        || branch.ends_with(['/', '.'])
        || branch.contains("..")
        || branch.contains("@{")
        || branch.contains("//")
        || branch.split('/').any(|component| {
            component.starts_with('.') || component.strip_suffix(".lock").is_some()
        })
        || branch
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || "~^:?*[\\".contains(c)))
}

/// Whether a value is a full Git object hash.
#[must_use]
pub fn valid_git_commit(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Whether a path stays lexically inside a checkout. Symlinks are checked at runtime.
///
/// ```text
/// "Dockerfile", "docker/app.Dockerfile", "."  -> valid
/// "/etc/passwd", "../x", "a/.git/config"     -> invalid
/// ```
#[must_use]
pub fn valid_repository_path(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 4096
        && !value.chars().any(char::is_control)
        && !value.contains('\\')
        && !value.contains(':')
        && !value.starts_with('/')
        && value.split('/').all(|part| part != ".." && part != ".git")
}

impl Source {
    /// Validates Git repository selection and build inputs.
    pub(super) fn validate_git(
        &self,
        base: &str,
        manifest_backed: bool,
        errors: &mut Vec<ValidationError>,
    ) {
        if let Self::Git { repository, build } = self {
            let path = format!("{base}.source.repository");
            match repository {
                SourceRepository::Git(repository) => repository.validate(&path, errors),
                SourceRepository::Manifest(_) if !manifest_backed => {
                    ManifestRevision::unbacked(&path, errors);
                }
                SourceRepository::Manifest(_) => {}
            }
            build.validate(&format!("{base}.source.build"), errors);
        }
    }
}

/// Docker stage names: a leading letter, then letters, digits, `-`, `_`, or `.`.
fn valid_build_target(value: &str) -> bool {
    value.len() <= MAX_IDENTIFIER_BYTES
        && value
            .as_bytes()
            .first()
            .is_some_and(u8::is_ascii_alphabetic)
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
}
