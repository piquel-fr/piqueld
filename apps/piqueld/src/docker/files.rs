//! Service files: each file's content is published as an immutable Swarm
//! config named after its environment and content (see
//! [`piqueld_core::DockerConfigName::for_file`]), and mounted read-only and
//! root-owned at its target. Changed content is a new config, so only the
//! services mounting it roll out; configs no service mounts any more are
//! removed once an operation finishes.

use super::configs::ConfigKind;
use super::{BollardDocker, DesiredService, DockerError, HealthProbe, TaskSpecContainerSpec};
use bollard::models::{TaskSpecContainerSpecConfigs, TaskSpecContainerSpecFile1};
use piqueld_core::resource::{
    APPLICATION_LABEL, FILE_LABEL, INSTANCE_LABEL, MANAGED_LABEL, ObservedFile,
};
use std::collections::BTreeMap;

impl BollardDocker {
    /// Returns the references mounting `desired`'s files, publishing missing
    /// configs first.
    ///
    /// # Errors
    /// Returns `OwnershipConflict` or `ConfigurationConflict` when a config
    /// with a file's name belongs to someone else or holds other content, and
    /// request errors otherwise.
    pub(super) async fn file_references(
        &self,
        desired: &DesiredService,
    ) -> Result<Vec<TaskSpecContainerSpecConfigs>, DockerError> {
        let labels = Self::file_labels(&desired.labels)?;
        let mut references = Vec::with_capacity(desired.files.len());
        for file in &desired.files {
            let name = file.config_name.as_str();
            let id = self
                .publish_config(ConfigKind::File, name, &labels, file.content.as_bytes())
                .await?;
            references.push(TaskSpecContainerSpecConfigs {
                config_id: Some(id),
                config_name: Some(name.to_owned()),
                file: Some(TaskSpecContainerSpecFile1 {
                    name: Some(file.target.clone()),
                    uid: Some("0".into()),
                    gid: Some("0".into()),
                    mode: Some(file.mode.bits()),
                }),
                runtime: None,
            });
        }
        Ok(references)
    }

    /// Removes the configs of the files of the environment `ownership`
    /// labels that no service mounts, now or in the previous spec a rollback
    /// restores.
    ///
    /// # Errors
    /// Returns request errors listing services or listing or removing configs.
    pub(super) async fn remove_unused_files(
        &self,
        ownership: &BTreeMap<String, String>,
    ) -> Result<(), DockerError> {
        let labels = Self::file_labels(ownership)?
            .into_iter()
            .map(|(key, value)| format!("{key}={value}"))
            .collect::<Vec<_>>();
        self.remove_unused_configs(ConfigKind::File, &labels, |_| false)
            .await
    }

    /// The environment ownership labels of `ownership`, and the file marker.
    fn file_labels(
        ownership: &BTreeMap<String, String>,
    ) -> Result<BTreeMap<String, String>, DockerError> {
        let mut labels = [MANAGED_LABEL, INSTANCE_LABEL, APPLICATION_LABEL]
            .into_iter()
            .map(|key| {
                let value = ownership
                    .get(key)
                    .ok_or(DockerError::Validation("service file ownership"))?;
                Ok((key.to_owned(), value.clone()))
            })
            .collect::<Result<BTreeMap<_, _>, DockerError>>()?;
        labels.insert(FILE_LABEL.to_owned(), "true".to_owned());
        Ok(labels)
    }

    /// The configs `container` mounts as service files: every config except
    /// the health probe, which no file's target may shadow.
    pub(super) fn observed_files(container: &TaskSpecContainerSpec) -> Vec<ObservedFile> {
        Self::file_configs(container)
            .filter_map(|config| {
                let file = config.file.as_ref()?;
                Some(ObservedFile {
                    config_name: config.config_name.clone()?,
                    target: file.name.clone()?,
                    mode: file.mode.unwrap_or_default(),
                })
            })
            .collect()
    }

    /// Whether every service file is mounted by ID as a root-owned file.
    /// Names, targets, and modes are compared with the desired files.
    pub(super) fn files_match_policy(container: &TaskSpecContainerSpec) -> bool {
        Self::file_configs(container).all(|config| {
            config.config_id.as_ref().is_some_and(|id| !id.is_empty())
                && config.config_name.is_some()
                && config.runtime.is_none()
                && config.file.as_ref().is_some_and(|file| {
                    file.name.is_some()
                        && file.uid.as_deref() == Some("0")
                        && file.gid.as_deref() == Some("0")
                })
        })
    }

    /// The config references of `container` other than the health probe.
    fn file_configs(
        container: &TaskSpecContainerSpec,
    ) -> impl Iterator<Item = &TaskSpecContainerSpecConfigs> {
        container
            .configs
            .iter()
            .flatten()
            .filter(|config| !HealthProbe::at_probe_path(config))
    }
}
