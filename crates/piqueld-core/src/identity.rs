//! Stable application and environment identities and deterministic Docker-safe names.

use crate::names::validated_string;
use sha2::{Digest, Sha256};
use std::ops::RangeInclusive;

/// Shared ID format: `lengths` lowercase ASCII letters, digits, or internal
/// hyphens. Persistence assigns IDs of 8-64.
pub(crate) fn valid_id(value: &str, lengths: RangeInclusive<usize>) -> bool {
    lengths.contains(&value.len())
        && value
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && value
            .bytes()
            .last()
            .is_some_and(|byte| byte.is_ascii_alphanumeric())
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

validated_string!(
    /// Stable internal application identity. It is assigned by persistence and is not
    /// derived from editable application metadata. An application owns the shared
    /// manifest and its environments; it has no runtime resources of its own.
    ///
    /// ```compile_fail
    /// use piqueld_core::{ApplicationId, EnvironmentId};
    /// let environment: EnvironmentId = ApplicationId::parse("app-00000001").unwrap();
    /// ```
    ApplicationId, ApplicationIdError,
    "application IDs must be 8-64 lowercase ASCII letters, digits, or internal hyphens",
    |value: &str| valid_id(value, 8..=64)
);

validated_string!(
    /// Stable internal environment identity, assigned by persistence. Docker names,
    /// ownership labels, secret encryption context, and history all derive from it.
    /// Environments migrated from single-environment applications keep the
    /// application ID they had before environments existed.
    EnvironmentId, EnvironmentIdError,
    "environment IDs must be 8-64 lowercase ASCII letters, digits, or internal hyphens",
    |value: &str| valid_id(value, 8..=64)
);

validated_string!(
    /// Stable release identity, assigned by persistence when a preparation
    /// records content its application has no release for yet.
    ReleaseId, ReleaseIdError,
    "release IDs must be 8-64 lowercase ASCII letters, digits, or internal hyphens",
    |value: &str| valid_id(value, 8..=64)
);

impl EnvironmentId {
    /// The ID of the environment created with an application. It reuses the
    /// application's ID, as every environment migrated from a single-environment
    /// application does; further environments receive their own IDs.
    #[must_use]
    pub fn default_for(application: &ApplicationId) -> Self {
        Self(application.as_str().to_owned())
    }
}

/// Managed Docker resource category.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResourceKind {
    /// A private overlay network.
    Network,
    /// A Swarm service.
    Service,
    /// A persistent Docker volume.
    Volume,
    /// A Swarm replicated job that runs to completion.
    Job,
    /// An immutable Swarm config holding a service file's content.
    File,
}

impl ResourceKind {
    /// Kind segment hashed into, and shown in, generated Docker names.
    fn token(self) -> &'static str {
        match self {
            Self::Network => "network",
            Self::Service => "service",
            Self::Volume => "volume",
            Self::Job => "job",
            Self::File => "file",
        }
    }
}

/// Prefix of every generated Docker name.
const NAME_PREFIX: &str = "piqueld";
/// Longest generated Docker name.
const NAME_LIMIT: usize = 63;
/// Length of the hexadecimal digest suffix of generated Docker names.
const NAME_SUFFIX_LEN: usize = 12;
/// Characters left for the readable head between `piqueld-` and `-<digest>`.
const NAME_HEAD_LEN: usize = NAME_LIMIT - NAME_PREFIX.len() - NAME_SUFFIX_LEN - 2;

/// Produces a stable, collision-resistant Docker name no longer than 63 bytes,
/// formatted `piqueld-{readable head}-{digest}`.
///
/// The readable head joins the sanitized parts and may be truncated; uniqueness
/// comes from the digest: the first `NAME_SUFFIX_LEN` hex characters of SHA-256
/// over the raw environment ID, kind, and logical name joined by NUL, so
/// distinct inputs stay distinct even when sanitizing or truncating makes them
/// look alike.
///
/// ```text
/// (01jz8r7b4w-test, Service, Some("web")) -> piqueld-01jz8r7b4w-test-service-web-<12 hex>
/// (01jz8r7b4w-test, Network, None)        -> piqueld-01jz8r7b4w-test-network-<12 hex>
/// ```
#[must_use]
pub fn docker_resource_name(
    id: &EnvironmentId,
    kind: ResourceKind,
    logical_name: Option<&str>,
) -> String {
    let parts = [id.as_str(), kind.token(), logical_name.unwrap_or("")];
    let digest = format!("{:x}", Sha256::digest(parts.join("\0")));
    let readable = parts
        .iter()
        .filter(|part| !part.is_empty())
        .map(|part| sanitize(part))
        .collect::<Vec<_>>()
        .join("-");
    format!(
        "{NAME_PREFIX}-{}-{}",
        readable_head(&readable),
        &digest[..NAME_SUFFIX_LEN]
    )
}

/// Returns the readable name prefix shared by all resources of an environment.
///
/// Prefixes are advisory and not unique: distinct identities can sanitize to
/// the same readable head, so ownership decisions must use labels and exact
/// names instead.
#[must_use]
pub fn docker_resource_readable_prefix(id: &EnvironmentId) -> String {
    format!("{NAME_PREFIX}-{}-", readable_head(&sanitize(id.as_str())))
}

/// Truncates a readable name to `NAME_HEAD_LEN` characters, trimming the
/// hyphens truncation leaves at its end.
fn readable_head(readable: &str) -> String {
    let head = readable.chars().take(NAME_HEAD_LEN).collect::<String>();
    head.trim_end_matches('-').to_owned()
}

/// Lowercases ASCII alphanumerics and collapses every other run of characters into
/// one hyphen, trimming hyphens at both ends.
///
/// ```text
/// "My_App--v2!" -> "my-app-v2"
/// ```
pub(crate) fn sanitize(value: &str) -> String {
    let mut output = String::new();
    for c in value.chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() {
            output.push(c);
        } else if !output.ends_with('-') {
            output.push('-');
        }
    }
    output.trim_matches('-').to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_bounded_safe_stable_and_distinct() {
        let id = EnvironmentId::parse("01jz8r7b4w-test").unwrap();
        let a = docker_resource_name(&id, ResourceKind::Service, Some(&"a".repeat(100)));
        let b = docker_resource_name(
            &id,
            ResourceKind::Service,
            Some(&format!("{}b", "a".repeat(99))),
        );
        assert_eq!(
            a,
            docker_resource_name(&id, ResourceKind::Service, Some(&"a".repeat(100)))
        );
        assert_ne!(a, b);
        assert!(
            a.len() <= 63
                && a.bytes()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        );
    }

    #[test]
    fn readable_prefix_matches_names_with_a_trailing_hyphen_at_the_limit() {
        let id = EnvironmentId::parse(format!("{}-a", "a".repeat(41))).unwrap();
        let name = docker_resource_name(&id, ResourceKind::Network, None);
        assert!(name.starts_with(&docker_resource_readable_prefix(&id)));
    }

    #[test]
    fn deserialization_preserves_the_id_invariant() {
        assert!(serde_json::from_str::<EnvironmentId>(r#""01jz8r7b4w-test""#).is_ok());
        assert!(serde_json::from_str::<EnvironmentId>(r#""--------""#).is_err());
        assert!(serde_json::from_str::<EnvironmentId>(r#""UPPERCASE""#).is_err());
    }

    #[test]
    fn parsing_returns_a_typed_error() {
        assert_eq!(
            EnvironmentId::parse("UPPERCASE").unwrap_err(),
            EnvironmentIdError
        );
    }
}
