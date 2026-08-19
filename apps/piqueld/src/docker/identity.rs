use super::{
    APPLICATION_LABEL, ApplicationId, BTreeMap, BollardDocker, INSTANCE_LABEL, MANAGED_LABEL,
    ResourceKind, SERVICE_LABEL, SPEC_HASH_LABEL, docker_resource_name,
};

impl BollardDocker {
    /// Returns whether a Docker resource can belong to this application.
    ///
    /// Resource names are truncated for Docker, so ownership labels remain the
    /// authoritative fallback when the readable name prefix is ambiguous.
    pub(super) fn relevant(
        name: &str,
        labels: &BTreeMap<String, String>,
        app: &ApplicationId,
    ) -> bool {
        let readable_application = app.as_str().chars().take(42).collect::<String>();
        let prefix = format!("piqueld-{readable_application}-");
        name.starts_with(&prefix)
            || labels
                .get(APPLICATION_LABEL)
                .is_some_and(|value| value == app.as_str())
    }

    /// Checks the immutable overlay-network settings supported by piqueld.
    ///
    /// Docker may add its VXLAN identifier to `options` after creation; that
    /// backend-assigned value is the only tolerated option.
    pub(super) fn network_configuration_matches(
        network: &bollard::models::Network,
        expected_ingress: bool,
    ) -> bool {
        network.driver.as_deref() == Some("overlay")
            && !network.internal.unwrap_or(false)
            && network.attachable.unwrap_or(false)
            && !network.enable_ipv6.unwrap_or(false)
            && !network.config_only.unwrap_or(false)
            && network.config_from.is_none()
            && network.ingress.unwrap_or(false) == expected_ingress
            && network.options.as_ref().is_none_or(|options| {
                options
                    .keys()
                    .all(|key| key == "com.docker.network.driver.overlay.vxlanid_list")
            })
    }

    /// Checks the local, option-free volume settings supported by piqueld.
    pub(super) fn volume_configuration_matches(volume: &bollard::models::Volume) -> bool {
        volume.driver == "local"
            && volume.options.is_empty()
            && volume.cluster_volume.is_none()
            && volume.scope.is_none_or(|scope| {
                matches!(
                    scope,
                    bollard::models::VolumeScopeEnum::EMPTY
                        | bollard::models::VolumeScopeEnum::LOCAL
                )
            })
    }

    /// Checks the ownership labels shared by an observed and desired resource.
    pub(super) fn owns(
        observed: &BTreeMap<String, String>,
        expected: &BTreeMap<String, String>,
    ) -> bool {
        expected.get(MANAGED_LABEL).map(String::as_str) == Some("true")
            && observed.get(MANAGED_LABEL).map(String::as_str) == Some("true")
            && expected.get(INSTANCE_LABEL).is_some()
            && observed.get(INSTANCE_LABEL) == expected.get(INSTANCE_LABEL)
            && expected
                .get(APPLICATION_LABEL)
                .is_none_or(|value| observed.get(APPLICATION_LABEL) == Some(value))
            && expected.get(APPLICATION_LABEL).is_none_or(|_| {
                observed
                    .get(SPEC_HASH_LABEL)
                    .is_some_and(|hash| Self::valid_spec_hash(hash))
            })
            && expected
                .get(SERVICE_LABEL)
                .is_none_or(|value| observed.get(SERVICE_LABEL) == Some(value))
    }

    /// Rechecks ownership and the canonical name before deleting a service.
    pub(super) fn owns_named_service(
        observed: &BTreeMap<String, String>,
        expected: &BTreeMap<String, String>,
        name: &str,
    ) -> bool {
        let Some(application) = expected
            .get(APPLICATION_LABEL)
            .and_then(|value| ApplicationId::parse(value.clone()).ok())
        else {
            return false;
        };
        let Some(service) = observed.get(SERVICE_LABEL) else {
            return false;
        };
        Self::valid_logical_name(service)
            && Self::owns(observed, expected)
            && docker_resource_name(&application, ResourceKind::Service, Some(service)) == name
    }

    /// Rechecks ownership and the canonical name before deleting a network.
    pub(super) fn owns_private_network(
        observed: &BTreeMap<String, String>,
        expected: &BTreeMap<String, String>,
        name: &str,
    ) -> bool {
        let Some(application) = expected
            .get(APPLICATION_LABEL)
            .and_then(|value| ApplicationId::parse(value.clone()).ok())
        else {
            return false;
        };
        Self::owns(observed, expected)
            && !observed.contains_key(SERVICE_LABEL)
            && docker_resource_name(&application, ResourceKind::Network, None) == name
    }

    /// Returns whether a managed spec label is a valid SHA-256 digest.
    pub(super) fn valid_spec_hash(value: &str) -> bool {
        piqueld_core::Sha256Digest::parse(value).is_ok()
    }

    /// Returns whether a logical resource name is safe for Docker naming.
    pub(super) fn valid_logical_name(value: &str) -> bool {
        (1..=63).contains(&value.len())
            && value
                .bytes()
                .next()
                .is_some_and(|byte| byte.is_ascii_lowercase())
            && value
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
            && !value.ends_with('-')
    }

    /// Returns whether an image reference contains a complete SHA-256 digest.
    pub(super) fn valid_digest(value: &str) -> bool {
        value.rsplit_once("@sha256:").is_some_and(|(_, d)| {
            d.len() == 64
                && d.bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        })
    }

    /// Returns the canonical repository portion of an image reference.
    pub(super) fn image_repository(reference: &str) -> Option<String> {
        let raw = reference.split('@').next()?;
        let slash = raw.rfind('/');
        let raw = match raw.rfind(':') {
            Some(colon) if slash.is_none_or(|s| colon > s) => &raw[..colon],
            _ => raw,
        };
        let first = raw.split('/').next()?;
        Some(if let Some(path) = raw.strip_prefix("docker.io/") {
            if path.contains('/') {
                raw.into()
            } else {
                format!("docker.io/library/{path}")
            }
        } else if raw.contains('/') && (first.contains(['.', ':']) || first == "localhost") {
            raw.into()
        } else if raw.contains('/') {
            format!("docker.io/{raw}")
        } else {
            format!("docker.io/library/{raw}")
        })
    }
}
