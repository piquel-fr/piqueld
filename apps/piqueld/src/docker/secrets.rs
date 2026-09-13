//! Docker secret provisioning and file references. Secret values are never inspected.
use super::{BollardDocker, DesiredService, DockerError};
use base64::Engine as _;
use bollard::models::{
    Secret, SecretSpec, TaskSpecContainerSpecFile, TaskSpecContainerSpecSecrets,
};
use piqueld_core::resource::{APPLICATION_LABEL, INSTANCE_LABEL, MANAGED_LABEL};
use std::collections::BTreeMap;

impl BollardDocker {
    async fn owned_secret(
        &self,
        name: &str,
        ownership: &BTreeMap<String, String>,
    ) -> Result<Option<Secret>, DockerError> {
        match self.docker.inspect_secret(name).await {
            Ok(secret) => {
                let labels: BTreeMap<String, String> = secret
                    .spec
                    .as_ref()
                    .and_then(|s| s.labels.as_ref())
                    .map(|l| l.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
                    .unwrap_or_default();
                if ownership.get(MANAGED_LABEL).map(String::as_str) != Some("true")
                    || [MANAGED_LABEL, INSTANCE_LABEL, APPLICATION_LABEL]
                        .iter()
                        .any(|key| {
                            ownership.get(*key).is_none() || labels.get(*key) != ownership.get(*key)
                        })
                {
                    return Err(DockerError::OwnershipConflict);
                }
                Ok(Some(secret))
            }
            Err(bollard::errors::Error::DockerResponseServerError {
                status_code: 404, ..
            }) => Ok(None),
            Err(error) => Err(DockerError::request("inspect secret", error)),
        }
    }
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
                match self.docker.delete_secret(id).await {
                    Ok(())
                    | Err(bollard::errors::Error::DockerResponseServerError {
                        status_code: 404,
                        ..
                    }) => (),
                    Err(error) => return Err(DockerError::request("delete secret", error)),
                }
            }
        }
        Ok(())
    }
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
