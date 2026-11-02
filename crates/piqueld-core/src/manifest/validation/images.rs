//! Image reference syntax: registries, repositories, tags and digests.

use crate::names::is_lowercase_label;

/// Whether a value follows Docker's image reference grammar, with extra hardening.
///
/// Accepts `[registry/]repository[:tag][@digest]` and rejects whitespace,
/// control characters, URL schemes (`//`), and `?`/`#`. A first component is a
/// registry when it contains `.` or `:`, or is `localhost`.
///
/// ```text
/// "nginx", "ghcr.io/org/app:v1", "localhost:5000/app@sha256:..."  -> valid
/// "https://ghcr.io/app", "Nginx", "app:", "a//b"                  -> invalid
/// ```
pub(crate) fn valid_image_reference(value: &str) -> bool {
    if value.is_empty()
        || value.len() > 512
        || value
            .chars()
            .any(|character| character.is_whitespace() || character.is_control())
        || value.contains("//")
        || value.contains(['?', '#'])
    {
        return false;
    }
    let mut digest_parts = value.split('@');
    let name_and_tag = digest_parts.next().unwrap_or_default();
    if let Some(digest) = digest_parts.next()
        && (digest_parts.next().is_some() || !valid_image_digest(digest))
    {
        return false;
    }
    let last_slash = name_and_tag.rfind('/');
    let tag_separator = name_and_tag
        .rfind(':')
        .filter(|index| last_slash.is_none_or(|slash| *index > slash));
    let (name, tag) = tag_separator.map_or((name_and_tag, None), |index| {
        (&name_and_tag[..index], Some(&name_and_tag[index + 1..]))
    });
    if tag.is_some_and(|tag| {
        tag.is_empty()
            || tag.len() > 128
            || !tag
                .bytes()
                .next()
                .is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
            || !tag
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'.' | b'-'))
    }) {
        return false;
    }
    if name.is_empty() || name.len() > 255 {
        return false;
    }
    if name.split('/').any(str::is_empty) {
        return false;
    }
    let repository = match registry_component(name) {
        Some(registry) if !valid_registry_authority(registry) => return false,
        Some(registry) => &name[registry.len() + 1..],
        None => name,
    };
    repository.split('/').all(valid_repository_component)
}

/// Whether a registry authority is `localhost` or DNS-like labels, with an
/// optional nonzero port.
fn valid_registry_authority(value: &str) -> bool {
    let (host, port) = value
        .rsplit_once(':')
        .map_or((value, None), |(host, port)| (host, Some(port)));
    if port.is_some_and(|port| port.parse::<u16>().map_or(true, |port| port == 0)) {
        return false;
    }
    let lowered = host.to_ascii_lowercase();
    lowered == "localhost"
        || lowered
            .split('.')
            .all(|label| is_lowercase_label(label, 1..=usize::MAX))
}

/// Returns the leading registry host of an image name, if it has one: a
/// first path component containing `.` or `:`, or `localhost`.
///
/// ```text
/// "ghcr.io/org/app" -> "ghcr.io"
/// "org/app", "app"  -> none
/// ```
pub(crate) fn registry_component(name: &str) -> Option<&str> {
    let (first, _) = name.split_once('/')?;
    (first.contains(['.', ':']) || first.eq_ignore_ascii_case("localhost")).then_some(first)
}

/// Lowercases the registry host of an image reference; other parts are case-sensitive.
/// Invalid references and references without a registry are returned unchanged.
///
/// ```text
/// "GHCR.io/org/app:v1" -> "ghcr.io/org/app:v1"
/// ```
pub(super) fn canonicalize_image_reference(value: &str) -> String {
    if !valid_image_reference(value) {
        return value.to_owned();
    }
    let (name, digest) = value
        .split_once('@')
        .map_or((value, None), |(name, digest)| (name, Some(digest)));
    let Some(registry) = registry_component(name) else {
        return value.to_owned();
    };
    let lowered = registry.to_ascii_lowercase();
    if lowered == registry {
        return value.to_owned();
    }
    let normalized_name = format!("{lowered}{}", &name[registry.len()..]);
    match digest {
        Some(digest) => format!("{normalized_name}@{digest}"),
        None => normalized_name,
    }
}

/// Whether a repository path component is lowercase alphanumeric runs joined by
/// one of the Docker separators: `.`, `_`, `__`, or one or more `-`.
fn valid_repository_component(value: &str) -> bool {
    let bytes = value.as_bytes();
    if !bytes
        .first()
        .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        || !bytes
            .last()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    {
        return false;
    }
    let mut index = 0;
    while index < bytes.len() {
        while index < bytes.len()
            && (bytes[index].is_ascii_lowercase() || bytes[index].is_ascii_digit())
        {
            index += 1;
        }
        if index == bytes.len() {
            return true;
        }
        let separator_start = index;
        while index < bytes.len() && matches!(bytes[index], b'.' | b'_' | b'-') {
            index += 1;
        }
        let separator = &value[separator_start..index];
        if separator.is_empty()
            || index == bytes.len()
            || !(separator == "."
                || separator == "_"
                || separator == "__"
                || separator.bytes().all(|byte| byte == b'-'))
        {
            return false;
        }
    }
    true
}

/// Whether a value matches the OCI digest grammar `algorithm:encoded`.
fn valid_image_digest(value: &str) -> bool {
    let Some((algorithm, encoded)) = value.split_once(':') else {
        return false;
    };
    let algorithm_parts = algorithm.split(['_', '+', '.', '-']).collect::<Vec<_>>();
    algorithm_parts.iter().all(|part| {
        part.bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase())
            && part
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
    }) && encoded.len() >= 32
        && encoded
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'=' | b'_' | b'-'))
}
