//! Immutable Swarm configs piqueld publishes: the health probe and service
//! files. Each is named after its content, so publishing is idempotent and
//! new content is a new config. Services mount configs by ID, read-only and
//! root-owned, and configs no service mounts are removed afterwards.

use super::{BollardDocker, DockerError};
use base64::Engine as _;
use hyper::{Method, StatusCode};
use std::collections::{BTreeMap, BTreeSet};

/// The parts of an inspected or listed config piqueld reads.
#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(super) struct Config {
    #[serde(rename = "ID")]
    pub(super) id: String,
    pub(super) spec: ConfigSpec,
}

/// A config's name, labels, and base64 content.
#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(super) struct ConfigSpec {
    pub(super) name: String,
    #[serde(default)]
    labels: Option<BTreeMap<String, String>>,
    #[serde(default)]
    data: String,
    /// Swarm renders templated configs, so their bytes are not the content.
    #[serde(default)]
    templating: Option<serde_json::Value>,
}

/// What a config holds, which names the Docker requests made for it.
#[derive(Clone, Copy)]
pub(super) enum ConfigKind {
    /// The `piqueld-probe` binary HTTP health checks run.
    HealthProbe,
    /// A file a service mounts.
    File,
}

impl ConfigKind {
    const fn publish(self) -> &'static str {
        match self {
            Self::HealthProbe => "publish the HTTP health probe",
            Self::File => "publish a service file",
        }
    }

    const fn inspect(self) -> &'static str {
        match self {
            Self::HealthProbe => "inspect the HTTP health probe",
            Self::File => "inspect a service file",
        }
    }

    const fn list(self) -> &'static str {
        match self {
            Self::HealthProbe => "list HTTP health probes",
            Self::File => "list service files",
        }
    }

    const fn remove(self) -> &'static str {
        match self {
            Self::HealthProbe => "remove an old HTTP health probe",
            Self::File => "remove an unused service file",
        }
    }
}

impl BollardDocker {
    /// Returns the ID of the config `name` holding `data`, creating it with
    /// `labels` first when it is missing. A creation race (HTTP 409) is
    /// resolved by reading the config that won.
    ///
    /// # Errors
    /// Returns `ConfigurationConflict` when the config holds other content,
    /// `OwnershipConflict` when it lacks one of `labels`, and request errors
    /// otherwise.
    pub(super) async fn publish_config(
        &self,
        kind: ConfigKind,
        name: &str,
        labels: &BTreeMap<String, String>,
        data: &[u8],
    ) -> Result<String, DockerError> {
        let data = base64::engine::general_purpose::STANDARD.encode(data);
        if let Some(id) = self.owned_config(kind, name, labels, &data).await? {
            return Ok(id);
        }
        let body = serde_json::json!({"Name": name, "Labels": labels, "Data": data});
        match self
            .engine_request(Method::POST, "/configs/create", body.to_string().into())
            .await
        {
            Ok(_) => {}
            Err(error) if error.is_status(StatusCode::CONFLICT) => {}
            Err(error) => return Err(error.sanitized(kind.publish())),
        }
        self.owned_config(kind, name, labels, &data)
            .await?
            .ok_or(DockerError::Request(kind.publish()))
    }

    /// Returns the ID of the config `name`, or `None` when it does not exist.
    /// The name promises base64 `data`, untemplated, and `labels`; anything
    /// else was not published here.
    async fn owned_config(
        &self,
        kind: ConfigKind,
        name: &str,
        labels: &BTreeMap<String, String>,
        data: &str,
    ) -> Result<Option<String>, DockerError> {
        let bytes = match self
            .engine_request(Method::GET, &format!("/configs/{name}"), Vec::new())
            .await
        {
            Ok(bytes) => bytes,
            Err(error) if error.is_not_found() => return Ok(None),
            Err(error) => return Err(error.sanitized(kind.inspect())),
        };
        let config: Config = serde_json::from_slice(&bytes)
            .map_err(|source| DockerError::request(kind.inspect(), source))?;
        let existing = config.spec.labels.unwrap_or_default();
        if labels
            .iter()
            .any(|(key, value)| existing.get(key) != Some(value))
        {
            return Err(DockerError::OwnershipConflict);
        }
        if config.spec.data != data || config.spec.templating.is_some() {
            return Err(DockerError::ConfigurationConflict);
        }
        Ok(Some(config.id))
    }

    /// Removes the configs of `kind` carrying every one of `labels` that no
    /// service mounts, now or in the previous spec a rollback restores, and
    /// that `keep` does not keep. Docker refuses to remove configs current
    /// specs use (HTTP 400), so a config a concurrent update starts using is
    /// kept too.
    ///
    /// # Errors
    /// Returns request errors listing services or listing or removing configs.
    pub(super) async fn remove_unused_configs(
        &self,
        kind: ConfigKind,
        labels: &[String],
        keep: impl Fn(&Config) -> bool,
    ) -> Result<(), DockerError> {
        // Listed before services, so a config published after this listing
        // is never removed for lack of a service mounting it yet.
        let filters = serde_json::json!({ "label": labels }).to_string();
        let path = format!(
            "/configs?filters={}",
            url::form_urlencoded::byte_serialize(filters.as_bytes()).collect::<String>()
        );
        let bytes = self
            .engine_request(Method::GET, &path, Vec::new())
            .await
            .map_err(|error| error.sanitized(kind.list()))?;
        let configs: Vec<Config> = serde_json::from_slice(&bytes)
            .map_err(|source| DockerError::request(kind.list(), source))?;
        // Bollard's service model omits `PreviousSpec`, so read the raw list.
        let services = self
            .engine_request(Method::GET, "/services", Vec::new())
            .await
            .map_err(|error| error.sanitized("list services"))?;
        let services: Vec<serde_json::Value> = serde_json::from_slice(&services)
            .map_err(|source| DockerError::request("decode services", source))?;
        let referenced = Self::referenced_configs(&services);
        for config in configs {
            if keep(&config) || referenced.contains(&config.id) {
                continue;
            }
            match self
                .engine_request(
                    Method::DELETE,
                    &format!("/configs/{}", config.id),
                    Vec::new(),
                )
                .await
            {
                Ok(_) => {}
                Err(error) if error.is_not_found() || error.is_status(StatusCode::BAD_REQUEST) => {}
                Err(error) => return Err(error.sanitized(kind.remove())),
            }
        }
        Ok(())
    }

    /// IDs of the configs the listed `services` mount in their current or
    /// previous specs.
    pub(super) fn referenced_configs(services: &[serde_json::Value]) -> BTreeSet<String> {
        services
            .iter()
            .flat_map(|service| [service.get("Spec"), service.get("PreviousSpec")])
            .flatten()
            .filter_map(|spec| {
                spec.pointer("/TaskTemplate/ContainerSpec/Configs")?
                    .as_array()
            })
            .flatten()
            .filter_map(|config| Some(config.get("ConfigID")?.as_str()?.to_owned()))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rollback restores the previous spec, so its configs are kept.
    #[test]
    fn previous_specs_keep_their_configs() {
        let spec = |id: &str| serde_json::json!({"TaskTemplate": {"ContainerSpec": {"Configs": [{"ConfigID": id}]}}});
        let services = [
            serde_json::json!({"Spec": spec("current"), "PreviousSpec": spec("previous")}),
            serde_json::json!({"Spec": {"TaskTemplate": {"ContainerSpec": {}}}}),
        ];
        assert_eq!(
            BollardDocker::referenced_configs(&services),
            BTreeSet::from(["current".into(), "previous".into()])
        );
    }
}
