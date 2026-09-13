//! Distinct names for generated Docker resources. Engine observations remain raw strings.

use crate::names::validated_string;
use crate::{ApplicationId, ResourceKind, ServiceName, VolumeName, docker_resource_name};

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

impl DockerNetworkName {
    /// Derives the application's private network name.
    #[must_use]
    pub fn for_application(id: &ApplicationId) -> Self {
        Self(docker_resource_name(id, ResourceKind::Network, None))
    }
}

impl DockerServiceName {
    /// Derives a service name from stable application and logical service identity.
    #[must_use]
    pub fn for_service(id: &ApplicationId, service: &ServiceName) -> Self {
        Self(docker_resource_name(
            id,
            ResourceKind::Service,
            Some(service.as_str()),
        ))
    }
}

impl DockerVolumeName {
    /// Derives a volume name from stable application and logical volume identity.
    #[must_use]
    pub fn for_volume(id: &ApplicationId, volume: &VolumeName) -> Self {
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
        let application = ApplicationId::parse("a".repeat(64)).unwrap();
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
