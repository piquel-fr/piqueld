//! Typed values owned by validated applications, exposed through immutable accessors.

use super::input::{self, HealthCheck, RepositoryManifest, ResourceLimits, Source};
use super::{ValidationError, ValidationErrors};
use crate::{ApplicationName, ServiceName, VolumeName};
use serde::Serialize;
use std::collections::BTreeMap;
use utoipa::ToSchema;

/// Validated application metadata.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, ToSchema)]
pub struct ValidatedMetadata {
    /// User-facing application name.
    pub name: ApplicationName,
}

/// Validated application resource lists.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, ToSchema)]
pub struct ValidatedSpec {
    /// Optional repository that supplies this application's manifest on Deploy.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest: Option<RepositoryManifest>,
    /// Declared services.
    pub services: Vec<ValidatedService>,
    /// Declared named volumes.
    pub volumes: Vec<ValidatedVolume>,
}

/// Validated application service.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, ToSchema)]
pub struct ValidatedService {
    /// Logical service name.
    pub name: ServiceName,
    /// Explicit image or build source.
    pub source: Source,
    /// Desired replica count.
    pub replicas: u16,
    /// Environment variables keyed by name.
    pub environment: BTreeMap<String, String>,
    /// Container entrypoint command.
    pub command: Vec<String>,
    /// Arguments passed to the command.
    pub arguments: Vec<String>,
    /// Persistent volume mounts.
    pub mounts: Vec<ValidatedMount>,
    /// Optional container health check.
    pub healthcheck: Option<HealthCheck>,
    /// Optional CPU and memory limits.
    pub resources: Option<ResourceLimits>,
}

/// Validated named volume.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
pub struct ValidatedVolume {
    /// Logical volume name.
    pub name: VolumeName,
}

/// A persistent volume mount in a service.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, ToSchema)]
pub struct ValidatedMount {
    /// Referenced logical volume name.
    pub volume: VolumeName,
    /// Container target path.
    pub target: String,
    /// Whether the mount is read-only.
    pub read_only: bool,
}

impl ValidationErrors {
    fn invalid_name(path: impl Into<String>, source: impl std::fmt::Display) -> Self {
        Self(vec![ValidationError {
            code: crate::codes::NAME_INVALID.into(),
            path: path.into(),
            message: source.to_string(),
        }])
    }
}

impl ValidatedMetadata {
    pub(super) fn from_input(value: input::Metadata) -> Result<Self, ValidationErrors> {
        Ok(Self {
            name: ApplicationName::parse(value.name)
                .map_err(|source| ValidationErrors::invalid_name("metadata.name", source))?,
        })
    }
}

impl ValidatedSpec {
    pub(super) fn from_input(value: input::ApplicationSpec) -> Result<Self, ValidationErrors> {
        Ok(Self {
            manifest: value.manifest,
            services: value
                .services
                .into_iter()
                .enumerate()
                .map(|(index, service)| ValidatedService::from_input(service, index))
                .collect::<Result<_, _>>()?,
            volumes: value
                .volumes
                .into_iter()
                .enumerate()
                .map(|(index, volume)| {
                    Ok(ValidatedVolume {
                        name: VolumeName::parse(volume.name).map_err(|source| {
                            ValidationErrors::invalid_name(
                                format!("spec.volumes[{index}].name"),
                                source,
                            )
                        })?,
                    })
                })
                .collect::<Result<_, ValidationErrors>>()?,
        })
    }

    pub(super) fn to_input(&self) -> input::ApplicationSpec {
        input::ApplicationSpec {
            manifest: self.manifest.clone(),
            services: self
                .services
                .iter()
                .map(ValidatedService::to_input)
                .collect(),
            volumes: self
                .volumes
                .iter()
                .map(|volume| input::Volume {
                    name: volume.name.to_string(),
                })
                .collect(),
        }
    }
}

impl ValidatedService {
    fn from_input(value: input::Service, index: usize) -> Result<Self, ValidationErrors> {
        Ok(Self {
            name: ServiceName::parse(value.name).map_err(|source| {
                ValidationErrors::invalid_name(format!("spec.services[{index}].name"), source)
            })?,
            source: value.source,
            replicas: value.replicas,
            environment: value.environment,
            command: value.command,
            arguments: value.arguments,
            mounts: value
                .mounts
                .into_iter()
                .enumerate()
                .map(|(mount_index, mount)| {
                    Ok(ValidatedMount {
                        volume: VolumeName::parse(mount.volume).map_err(|source| {
                            ValidationErrors::invalid_name(
                                format!("spec.services[{index}].mounts[{mount_index}].volume"),
                                source,
                            )
                        })?,
                        target: mount.target,
                        read_only: mount.read_only,
                    })
                })
                .collect::<Result<_, ValidationErrors>>()?,
            healthcheck: value.healthcheck,
            resources: value.resources,
        })
    }

    fn to_input(&self) -> input::Service {
        input::Service {
            name: self.name.to_string(),
            source: self.source.clone(),
            replicas: self.replicas,
            environment: self.environment.clone(),
            command: self.command.clone(),
            arguments: self.arguments.clone(),
            mounts: self
                .mounts
                .iter()
                .map(|mount| input::Mount {
                    volume: mount.volume.to_string(),
                    target: mount.target.clone(),
                    read_only: mount.read_only,
                })
                .collect(),
            healthcheck: self.healthcheck.clone(),
            resources: self.resources.clone(),
        }
    }
}
