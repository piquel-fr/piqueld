//! Distinct names for generated Docker resources. Engine observations remain raw strings.

use crate::manifest::FileMode;
use crate::names::validated_string;
use crate::resource::LINK_LABEL;
use crate::{EnvironmentId, JobName, ResourceKind, ServiceName, VolumeName, docker_resource_name};
use std::collections::BTreeMap;

validated_string!(
    /// A Docker network name, distinct from logical names and other resource kinds.
    DockerNetworkName, DockerNetworkNameError,
    "managed Docker network names must be 1-63 lowercase letters, digits, or internal hyphens",
    crate::resource::valid_logical_name
);
validated_string!(
    /// A Docker service name, distinct from its logical service name.
    ///
    /// ```compile_fail
    /// use piqueld_core::{DockerNetworkName, DockerServiceName};
    /// fn attach_network(_: DockerNetworkName) {}
    /// fn wrong_kind(service: DockerServiceName) { attach_network(service); }
    /// ```
    DockerServiceName, DockerServiceNameError,
    "managed Docker service names must be 1-63 lowercase letters, digits, or internal hyphens",
    crate::resource::valid_logical_name
);
validated_string!(
    /// A Docker volume name, distinct from its logical volume name.
    DockerVolumeName, DockerVolumeNameError,
    "managed Docker volume names must be 1-63 lowercase letters, digits, or internal hyphens",
    crate::resource::valid_logical_name
);

validated_string!(
    /// A Docker config name, distinct from other resource kinds.
    DockerConfigName, DockerConfigNameError,
    "managed Docker config names must be 1-63 lowercase letters, digits, or internal hyphens",
    crate::resource::valid_logical_name
);

impl DockerConfigName {
    /// Names the config holding a file's `content`, mounted with `mode`, for
    /// an environment. Configs are immutable, so the name follows the
    /// content: new content is a new config, and unchanged content keeps its
    /// config across deployments. Docker materializes a config once per
    /// container, so files sharing content but not mode need their own.
    #[must_use]
    pub fn for_file(id: &EnvironmentId, content: &str, mode: FileMode) -> Self {
        Self(docker_resource_name(
            id,
            ResourceKind::File,
            Some(&format!(
                "{}-{mode}",
                crate::manifest::files::content_digest(content)
            )),
        ))
    }
}

impl DockerNetworkName {
    /// Whether this is the canonical private or ingress network for an application.
    #[must_use]
    pub fn is_for_application(&self, id: &EnvironmentId) -> bool {
        self == &Self::for_application(id) || self == &Self::for_ingress(id)
    }

    /// Whether this is a network `owner` owns, given its `labels`: its private
    /// or ingress network, or a link network to the exporter its link label names.
    #[must_use]
    pub fn is_canonical(&self, owner: &EnvironmentId, labels: &BTreeMap<String, String>) -> bool {
        self.is_for_application(owner)
            || labels
                .get(LINK_LABEL)
                .and_then(|exporter| EnvironmentId::parse(exporter.clone()).ok())
                .is_some_and(|exporter| self == &Self::for_link(owner, &exporter))
    }

    /// Derives the network `importer` owns for its link to `exporter`, which
    /// the exporter's linked services join.
    #[must_use]
    pub fn for_link(importer: &EnvironmentId, exporter: &EnvironmentId) -> Self {
        Self(docker_resource_name(
            importer,
            ResourceKind::Network,
            Some(&format!("link-{exporter}")),
        ))
    }

    /// Derives a separate network for this application's exposed services.
    #[must_use]
    pub fn for_ingress(id: &EnvironmentId) -> Self {
        Self(docker_resource_name(
            id,
            ResourceKind::Network,
            Some("ingress"),
        ))
    }

    /// Derives the application's private network name.
    #[must_use]
    pub fn for_application(id: &EnvironmentId) -> Self {
        Self(docker_resource_name(id, ResourceKind::Network, None))
    }
}

impl DockerServiceName {
    /// Derives a service name from stable application and logical service identity.
    #[must_use]
    pub fn for_service(id: &EnvironmentId, service: &ServiceName) -> Self {
        Self(docker_resource_name(
            id,
            ResourceKind::Service,
            Some(service.as_str()),
        ))
    }

    /// Derives a job's service name, disjoint from every logical service's name.
    #[must_use]
    pub fn for_job(id: &EnvironmentId, job: &JobName) -> Self {
        Self(docker_resource_name(
            id,
            ResourceKind::Job,
            Some(job.as_str()),
        ))
    }
}

impl DockerVolumeName {
    /// Derives a volume name from stable application and logical volume identity.
    #[must_use]
    pub fn for_volume(id: &EnvironmentId, volume: &VolumeName) -> Self {
        Self(docker_resource_name(
            id,
            ResourceKind::Volume,
            Some(volume.as_str()),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_names_preserve_existing_identity_and_round_trip() {
        let application = EnvironmentId::parse("a".repeat(64)).unwrap();
        let logical = "b".repeat(63);
        let service =
            DockerServiceName::for_service(&application, &ServiceName::parse(&logical).unwrap());
        let volume =
            DockerVolumeName::for_volume(&application, &VolumeName::parse(&logical).unwrap());
        let network = DockerNetworkName::for_application(&application);
        assert_eq!(
            service.as_str(),
            docker_resource_name(&application, ResourceKind::Service, Some(&logical))
        );
        assert_ne!(service.as_str(), volume.as_str());
        assert_ne!(volume.as_str(), network.as_str());
        assert_eq!(DockerServiceName::parse(service.as_str()).unwrap(), service);
        assert_eq!(DockerVolumeName::parse(volume.as_str()).unwrap(), volume);
        assert_eq!(DockerNetworkName::parse(network.as_str()).unwrap(), network);
        assert_eq!(
            serde_json::from_str::<DockerServiceName>(&serde_json::to_string(&service).unwrap())
                .unwrap(),
            service
        );
    }
}
