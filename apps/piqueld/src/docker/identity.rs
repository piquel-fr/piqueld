use super::{
    APPLICATION_LABEL, ApplicationId, BTreeMap, BollardDocker, CreateImageOptionsBuilder, Docker,
    DockerError, HashMap, INSTANCE_LABEL, ImageSource, MANAGED_LABEL, ResourceKind, SERVICE_LABEL,
    SPEC_HASH_LABEL, TryStreamExt, docker_resource_name, docker_resource_readable_prefix,
    valid_logical_name,
};

impl BollardDocker {
    /// Builds the Docker label filter selecting one application's resources.
    pub(super) fn application_label_filter(
        application: &ApplicationId,
    ) -> HashMap<String, Vec<String>> {
        HashMap::from([(
            "label".to_owned(),
            vec![format!("{APPLICATION_LABEL}={application}")],
        )])
    }

    /// Builds the complementary name filter used to detect canonical resources
    /// whose ownership labels are missing or belong to another application.
    pub(super) fn application_name_filter(
        application: &ApplicationId,
    ) -> HashMap<String, Vec<String>> {
        HashMap::from([(
            "name".to_owned(),
            vec![docker_resource_readable_prefix(application)],
        )])
    }

    /// Returns whether a Docker resource can belong to this application.
    ///
    /// Resource names are truncated for Docker, so ownership labels remain the
    /// authoritative fallback when the readable name prefix is ambiguous.
    pub(super) fn relevant(
        name: &str,
        labels: &BTreeMap<String, String>,
        app: &ApplicationId,
    ) -> bool {
        if let Some(owner) = labels.get(APPLICATION_LABEL) {
            if owner == app.as_str() {
                return true;
            }
            if let Ok(owner) = ApplicationId::parse(owner.clone()) {
                let kind = if labels.contains_key(SERVICE_LABEL) {
                    ResourceKind::Service
                } else {
                    ResourceKind::Network
                };
                let logical_name = labels.get(SERVICE_LABEL).map(String::as_str);
                if name == docker_resource_name(&owner, kind, logical_name)
                    && name != docker_resource_name(app, kind, logical_name)
                {
                    return false;
                }
            }
        }
        // Unlabelled resources and volumes with truncated logical names cannot
        // be disambiguated from an application ID alone. Keep them fail-closed.
        name.starts_with(&docker_resource_readable_prefix(app))
    }

    /// Checks the immutable overlay-network settings supported by piqueld.
    ///
    /// Docker may add its VXLAN identifier to `options` after creation; that
    /// backend-assigned value is the only tolerated option.
    pub(super) fn network_configuration_matches(network: &bollard::models::Network) -> bool {
        network.driver.as_deref() == Some("overlay")
            && !network.internal.unwrap_or(false)
            && network.attachable.unwrap_or(false)
            && !network.enable_ipv6.unwrap_or(false)
            && !network.config_only.unwrap_or(false)
            && network.config_from.is_none()
            && !network.ingress.unwrap_or(false)
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
        valid_logical_name(service)
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

    /// Returns whether an image reference contains a complete SHA-256 digest.
    pub(super) fn valid_digest(value: &str) -> bool {
        value.rsplit_once("@sha256:").is_some_and(|(_, d)| {
            d.len() == 64
                && d.bytes()
                    .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        })
    }
}

#[async_trait::async_trait]
impl ImageSource for Docker {
    async fn repo_digests(&self, reference: &str) -> Result<Option<Vec<String>>, DockerError> {
        match self.inspect_image(reference).await {
            Ok(image) => Ok(Some(image.repo_digests.unwrap_or_default())),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(None),
            Err(error) => Err(DockerError::image_resolution("inspect image", error)),
        }
    }

    async fn pull(&self, reference: &str) -> Result<(), DockerError> {
        self.create_image(
            Some(
                CreateImageOptionsBuilder::default()
                    .from_image(reference)
                    .build(),
            ),
            None,
            None,
        )
        .try_collect::<Vec<_>>()
        .await
        .map(|_| ())
        .map_err(|error| DockerError::image_resolution("pull image", error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_prefix_does_not_include_foreign_canonical_resources() {
        let prefix = "a".repeat(42);
        let app = ApplicationId::parse(format!("{prefix}-one")).unwrap();
        let foreign = ApplicationId::parse(format!("{prefix}-two")).unwrap();
        assert_eq!(
            docker_resource_readable_prefix(&app),
            docker_resource_readable_prefix(&foreign)
        );
        for (kind, logical_name) in [
            (ResourceKind::Network, None),
            (ResourceKind::Service, Some("web")),
        ] {
            let mut labels = BTreeMap::from([(APPLICATION_LABEL.into(), foreign.to_string())]);
            if let Some(logical_name) = logical_name {
                labels.insert(SERVICE_LABEL.into(), logical_name.into());
            }
            let foreign_name = docker_resource_name(&foreign, kind, logical_name);
            assert!(!BollardDocker::relevant(&foreign_name, &labels, &app));
            let own_name = docker_resource_name(&app, kind, logical_name);
            assert!(BollardDocker::relevant(&own_name, &labels, &app));
            assert!(BollardDocker::relevant(&own_name, &BTreeMap::new(), &app));
        }
    }
}
