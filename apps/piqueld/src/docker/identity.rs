use super::{
    APPLICATION_LABEL, ApplicationId, BTreeMap, BollardDocker, CreateImageOptionsBuilder, Docker,
    DockerError, HashMap, INSTANCE_LABEL, ImageSource, MANAGED_LABEL, ResourceKind, SERVICE_LABEL,
    TryStreamExt, docker_resource_name, docker_resource_readable_prefix,
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
            // Docker serializes an unused ConfigFrom as either null or an
            // object whose Network field is empty, depending on Engine/API
            // version. Both mean that this is not a config-derived network.
            && network
                .config_from
                .as_ref()
                .is_none_or(|config| config.network.as_deref().is_none_or(str::is_empty))
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

    /// Rechecks ownership and the canonical name before mutating a service.
    pub(super) fn owns_named_service(
        observed: &BTreeMap<String, String>,
        expected: &BTreeMap<String, String>,
        name: &str,
    ) -> bool {
        Self::owns_resource(observed, expected, ResourceKind::Service, name)
    }

    /// Rechecks ownership and the canonical name before mutating a network.
    pub(super) fn owns_private_network(
        observed: &BTreeMap<String, String>,
        expected: &BTreeMap<String, String>,
        name: &str,
    ) -> bool {
        Self::owns_resource(observed, expected, ResourceKind::Network, name)
    }

    pub(super) fn owns_resource(
        observed: &BTreeMap<String, String>,
        expected: &BTreeMap<String, String>,
        kind: ResourceKind,
        name: &str,
    ) -> bool {
        let Some(application) = expected
            .get(APPLICATION_LABEL)
            .and_then(|value| ApplicationId::parse(value.clone()).ok())
        else {
            return false;
        };
        let Some(instance) = expected
            .get(INSTANCE_LABEL)
            .and_then(|value| piqueld_core::InstanceId::parse(value.clone()).ok())
        else {
            return false;
        };
        expected.get(MANAGED_LABEL).map(String::as_str) == Some("true")
            && expected
                .get(SERVICE_LABEL)
                .is_none_or(|service| observed.get(SERVICE_LABEL) == Some(service))
            && piqueld_core::OwnershipState::for_resource(
                observed,
                &instance,
                &application,
                kind,
                name,
            ) == piqueld_core::OwnershipState::Owned
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
        .try_for_each(|_| std::future::ready(Ok(())))
        .await
        .map_err(|error| DockerError::image_resolution("pull image", error))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordinary_overlay_network_accepts_docker_empty_config_reference() {
        let mut network = bollard::models::Network {
            driver: Some("overlay".into()),
            internal: Some(false),
            attachable: Some(true),
            enable_ipv6: Some(false),
            config_only: Some(false),
            ingress: Some(false),
            config_from: Some(bollard::models::ConfigReference {
                network: Some(String::new()),
            }),
            ..Default::default()
        };

        assert!(BollardDocker::network_configuration_matches(&network));
        network.config_from = Some(bollard::models::ConfigReference {
            network: Some("shared-config".into()),
        });
        assert!(!BollardDocker::network_configuration_matches(&network));
    }

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

#[cfg(test)]
mod network_defaults_tests {
    use super::BollardDocker;
    #[test]
    fn empty_config_from_is_an_engine_default_but_named_sources_are_conflicts() {
        let mut network: bollard::models::Network=serde_json::from_value(serde_json::json!({"Driver":"overlay","Internal":false,"Attachable":true,"ConfigFrom":{"Network":""},"Options":{"com.docker.network.driver.overlay.vxlanid_list":"4100"}})).unwrap();
        assert!(BollardDocker::network_configuration_matches(&network));
        network.config_from.as_mut().unwrap().network = Some("external-template".into());
        assert!(!BollardDocker::network_configuration_matches(&network));
        network.config_from = None;
        assert!(BollardDocker::network_configuration_matches(&network));
    }
}
