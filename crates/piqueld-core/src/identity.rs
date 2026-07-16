//! Stable application identity and deterministic Docker-safe names.
#![allow(missing_docs)]

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
    Network,
    Service,
    Volume,
    Secret,
}

impl ResourceKind {
    fn token(self) -> &'static str {
        match self {
            Self::Network => "network",
            Self::Service => "service",
            Self::Volume => "volume",
            Self::Secret => "secret",
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

/// Produces a stable Traefik router name no longer than 63 bytes.
#[must_use]
pub fn router_name(id: &ApplicationId, host: &str, service: &str, port: u16) -> String {
    bounded_name(
        "piqueld-router",
        &[id.as_str(), host, service, &port.to_string()],
        63,
    )
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
    let suffix = &suffix[..12];
    let head_len = limit.saturating_sub(prefix.len() + suffix.len() + 2);
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
