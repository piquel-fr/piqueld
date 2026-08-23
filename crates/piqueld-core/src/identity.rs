//! Stable application identity and deterministic Docker-safe names.

use serde::{Deserialize, Deserializer, Serialize, de};
use sha2::{Digest, Sha256};
use std::{fmt, str::FromStr};
use thiserror::Error;
use utoipa::ToSchema;

/// An error returned when an application identifier violates its storage invariant.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[error("application IDs must be 8-64 lowercase ASCII letters, digits, or internal hyphens")]
pub struct ApplicationIdError;

/// Stable internal application identity. It is assigned by persistence and is not
/// derived from editable application metadata.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
#[serde(transparent)]
pub struct ApplicationId(String);

impl ApplicationId {
    /// Parses a storage-assigned identifier.
    ///
    /// # Errors
    /// Returns an error when the identifier is outside its safe alphabet or length.
    pub fn parse(value: impl Into<String>) -> Result<Self, ApplicationIdError> {
        let value = value.into();
        if (8..=64).contains(&value.len())
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
        {
            Ok(Self(value))
        } else {
            Err(ApplicationIdError)
        }
    }

    /// Returns the persisted wire representation.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for ApplicationId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(de::Error::custom)
    }
}

impl fmt::Display for ApplicationId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl FromStr for ApplicationId {
    type Err = ApplicationIdError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
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
}

impl ResourceKind {
    fn token(self) -> &'static str {
        match self {
            Self::Network => "network",
            Self::Service => "service",
            Self::Volume => "volume",
        }
    }
}

/// Produces a stable, collision-resistant Docker name no longer than 63 bytes.
#[must_use]
pub fn docker_resource_name(
    id: &ApplicationId,
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

/// Returns the readable name prefix shared by all resources of an application.
///
/// Prefixes are advisory and not unique: distinct identities can sanitize to
/// the same readable head, so ownership decisions must use labels and exact
/// names instead.
#[must_use]
pub fn docker_resource_readable_prefix(id: &ApplicationId) -> String {
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
        let id = ApplicationId::parse("01jz8r7b4w-test").unwrap();
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
        let id = ApplicationId::parse(format!("{}-a", "a".repeat(41))).unwrap();
        let name = docker_resource_name(&id, ResourceKind::Network, None);
        assert!(name.starts_with(&docker_resource_readable_prefix(&id)));
    }

    #[test]
    fn deserialization_preserves_the_id_invariant() {
        assert!(serde_json::from_str::<ApplicationId>(r#""01jz8r7b4w-test""#).is_ok());
        assert!(serde_json::from_str::<ApplicationId>(r#""--------""#).is_err());
        assert!(serde_json::from_str::<ApplicationId>(r#""UPPERCASE""#).is_err());
    }

    #[test]
    fn parsing_returns_a_typed_error() {
        assert_eq!(
            ApplicationId::parse("UPPERCASE").unwrap_err(),
            ApplicationIdError
        );
    }
}
