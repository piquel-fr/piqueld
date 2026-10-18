//! Docker secret provisioning and file references. Secret values are never inspected.
use super::{BollardDocker, BollardResult, DesiredService, DockerError};
use base64::Engine as _;
use bollard::models::{
    Secret, SecretSpec, TaskSpecContainerSpecFile, TaskSpecContainerSpecSecrets,
};
use piqueld_core::resource::MANAGED_LABEL;
use std::collections::BTreeMap;

impl BollardDocker {
    /// Inspects a secret's metadata and verifies its ownership labels.
    ///
    /// Returns `None` when the secret does not exist, and `OwnershipConflict`
    /// unless `ownership` is managed and the managed, instance, and
    /// application labels are all present and equal on both sides.
    async fn owned_secret(
        &self,
        name: &str,
        ownership: &BTreeMap<String, String>,
    ) -> Result<Option<Secret>, DockerError> {
        let Some(secret) = self
            .docker
            .inspect_secret(name)
            .await
            .found()
            .or_request("inspect secret")?
        else {
            return Ok(None);
        };
        let labels = secret.spec.as_ref().and_then(|spec| spec.labels.as_ref());
        let owned = ownership.get(MANAGED_LABEL).map(String::as_str) == Some("true")
            && Self::environment_labels(ownership, "secret ownership").is_ok_and(|expected| {
                expected.iter().all(|(key, value)| {
                    labels
                        .and_then(|labels| labels.get(*key))
                        .map(String::as_str)
                        == Some(value)
                })
            });
        if !owned {
            return Err(DockerError::OwnershipConflict);
        }
        Ok(Some(secret))
    }
    /// Creates an owned secret unless it already exists.
    ///
    /// Secrets are immutable, so an existing owned secret is accepted without
    /// comparing its value. A creation race (HTTP 409) is resolved by
    /// rechecking ownership of the secret that won.
    pub(super) async fn provision_secret(
        &self,
        name: &str,
        value: &[u8],
        ownership: &BTreeMap<String, String>,
    ) -> Result<(), DockerError> {
        if self.owned_secret(name, ownership).await?.is_some() {
            return Ok(());
        }
        let spec = SecretSpec {
            name: Some(name.into()),
            labels: Some(ownership.clone().into_iter().collect()),
            data: Some(base64::engine::general_purpose::STANDARD.encode(value)),
            ..Default::default()
        };
        match self.docker.create_secret(spec).await {
            Ok(_) => Ok(()),
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 409, ..
            }) => self
                .owned_secret(name, ownership)
                .await?
                .map(|_| ())
                .ok_or(DockerError::Unavailable("create secret")),
            Err(error) => Err(DockerError::request("create secret", error)),
        }
    }
    /// Deletes each named secret by ID after verifying ownership; missing
    /// secrets are skipped and a foreign one aborts with `OwnershipConflict`.
    pub(super) async fn remove_owned_secrets(
        &self,
        names: &[String],
        ownership: &BTreeMap<String, String>,
    ) -> Result<(), DockerError> {
        for name in names {
            if let Some(secret) = self.owned_secret(name, ownership).await? {
                let id = secret
                    .id
                    .as_deref()
                    .ok_or(DockerError::Validation("secret ID missing"))?;
                self.docker
                    .delete_secret(id)
                    .await
                    .found()
                    .or_request("delete secret")?;
            }
        }
        Ok(())
    }
    /// Builds the container secret references for a desired service.
    ///
    /// Each secret must already exist with the service's ownership labels and
    /// is mounted by ID as a root-owned, world-readable (`0444`) file.
    pub(super) async fn secret_references(
        &self,
        desired: &DesiredService,
    ) -> Result<Vec<TaskSpecContainerSpecSecrets>, DockerError> {
        let mut references = Vec::new();
        for mount in &desired.secrets {
            let secret = self
                .owned_secret(&mount.secret_name, &desired.labels)
                .await?
                .ok_or(DockerError::Unavailable("required secret version missing"))?;
            references.push(TaskSpecContainerSpecSecrets {
                secret_id: Some(
                    secret
                        .id
                        .ok_or(DockerError::Validation("secret ID missing"))?,
                ),
                secret_name: Some(mount.secret_name.clone()),
                file: Some(TaskSpecContainerSpecFile {
                    name: Some(mount.target.clone()),
                    uid: Some("0".into()),
                    gid: Some("0".into()),
                    mode: Some(0o444),
                }),
            });
        }
        Ok(references)
    }
}
