//! Stable application and environment identities and deterministic Docker-safe names.

use crate::names::validated_string;
use sha2::{Digest, Sha256};

/// Shared persistence ID format: 8-64 lowercase ASCII letters, digits, or
/// internal hyphens.
fn valid_id(value: &str) -> bool {
    (8..=64).contains(&value.len())
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
    valid_id
);

validated_string!(
    /// Stable internal environment identity, assigned by persistence. Docker names,
    /// ownership labels, secret encryption context, and history all derive from it.
    /// Environments migrated from single-environment applications keep the
    /// application ID they had before environments existed.
    EnvironmentId, EnvironmentIdError,
    "environment IDs must be 8-64 lowercase ASCII letters, digits, or internal hyphens",
    valid_id
);

validated_string!(
    /// Stable release identity, assigned by persistence when a preparation
    /// records content its application has no release for yet.
    ReleaseId, ReleaseIdError,
    "release IDs must be 8-64 lowercase ASCII letters, digits, or internal hyphens",
    valid_id
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
}

impl ResourceKind {
    /// Kind segment hashed into, and shown in, generated Docker names.
    fn token(self) -> &'static str {
        match self {
            Self::Network => "network",
            Self::Service => "service",
            Self::Volume => "volume",
            Self::Job => "job",
        }
    }
}

/// Produces a stable, collision-resistant Docker name no longer than 63 bytes.
///
/// The readable part may be truncated; uniqueness comes from the digest suffix
/// over the full environment ID, kind, and logical name.
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
    bounded_name(
        "piqueld",
        &[id.as_str(), kind.token(), logical_name.unwrap_or("")],
        63,
    )
}

/// Length of the hexadecimal digest suffix in bounded Docker names.
const NAME_SUFFIX_LEN: usize = 12;
/// Number of hyphens bounding the readable head in bounded Docker names.
const NAME_SEPARATOR_LEN: usize = 2;

/// Returns the readable name prefix shared by all resources of an environment.
///
/// Prefixes are advisory and not unique: distinct identities can sanitize to
/// the same readable head, so ownership decisions must use labels and exact
/// names instead.
#[must_use]
pub fn docker_resource_readable_prefix(id: &EnvironmentId) -> String {
    let head_len = 63usize.saturating_sub("piqueld".len() + NAME_SUFFIX_LEN + NAME_SEPARATOR_LEN);
    let mut head = sanitize(id.as_str())
        .chars()
        .take(head_len)
        .collect::<String>();
    while head.ends_with('-') {
        head.pop();
    }
    format!("piqueld-{head}-")
}

/// Formats `{prefix}-{readable head}-{digest}` within `limit` bytes.
///
/// The digest is the first `NAME_SUFFIX_LEN` hex characters of SHA-256 over the raw
/// parts joined by NUL, so distinct inputs stay distinct even when sanitizing or
/// truncating the readable head makes them look alike. Trailing hyphens left by
/// truncation are trimmed.
fn bounded_name(prefix: &str, parts: &[&str], limit: usize) -> String {
    let identity = parts.join("\0");
    let suffix = format!("{:x}", Sha256::digest(identity.as_bytes()));
    let readable = parts
        .iter()
        .filter(|p| !p.is_empty())
        .map(|p| sanitize(p))
        .collect::<Vec<_>>()
        .join("-");
    let suffix = &suffix[..NAME_SUFFIX_LEN];
    let head_len = limit.saturating_sub(prefix.len() + suffix.len() + NAME_SEPARATOR_LEN);
    let mut head = readable.chars().take(head_len).collect::<String>();
    while head.ends_with('-') {
        head.pop();
    }
    format!("{prefix}-{head}-{suffix}")
}

/// Lowercases ASCII alphanumerics and collapses every other run of characters into
/// one hyphen, trimming hyphens at both ends.
///
/// ```text
/// "My_App--v2!" -> "my-app-v2"
/// ```
fn sanitize(value: &str) -> String {
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
