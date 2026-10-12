//! Immutable Swarm configs piqueld publishes: the health probe and service
//! files. Each is named after its content, so publishing is idempotent and
//! new content is a new config. Services mount configs by ID, read-only and
//! root-owned, and configs no service mounts are removed afterwards.

use super::{BollardDocker, DockerError};
use base64::Engine as _;
use hyper::{Method, StatusCode};
use std::collections::{BTreeMap, BTreeSet};

/// The parts of an inspected config piqueld reads.
#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Config {
    #[serde(rename = "ID")]
    id: String,
    spec: ConfigSpec,
}

/// A config's labels, base64 content, and templating.
#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ConfigSpec {
    #[serde(default)]
    labels: Option<BTreeMap<String, String>>,
    #[serde(default)]
    data: String,
    /// Swarm renders templated configs, so their bytes are not the content.
    #[serde(default)]
    templating: Option<serde_json::Value>,
}

/// A listed config's identity. Listings carry every config's payload, which
/// is skipped while decoding.
#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(super) struct ListedConfig {
    #[serde(rename = "ID")]
    id: String,
    spec: ListedConfigSpec,
}

/// A listed config's name.
#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ListedConfigSpec {
    name: String,
}

impl ListedConfig {
    /// The config's name.
    pub(super) fn name(&self) -> &str {
        &self.spec.name
    }
}

/// The configs a listed service mounts in its current and previous specs.
/// Everything else in them, such as environments, is skipped while decoding.
#[derive(Default, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(super) struct ServiceConfigs {
    #[serde(default)]
    spec: Option<SpecConfigs>,
    #[serde(default)]
    previous_spec: Option<SpecConfigs>,
}

/// `Spec.TaskTemplate.ContainerSpec.Configs[].ConfigID` of a service spec.
#[derive(Default, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SpecConfigs {
    #[serde(default)]
    task_template: Option<TaskConfigs>,
}

/// The container of a task template.
#[derive(Default, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct TaskConfigs {
    #[serde(default)]
    container_spec: Option<ContainerConfigs>,
}

/// The config references of a container.
#[derive(Default, serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ContainerConfigs {
    #[serde(default)]
    configs: Option<Vec<ConfigReference>>,
}

/// One config reference's ID.
#[derive(Default, serde::Deserialize)]
struct ConfigReference {
    #[serde(rename = "ConfigID", default)]
    config_id: Option<String>,
}

impl ServiceConfigs {
    /// IDs of the configs this service's current and previous specs mount.
    fn ids(self) -> impl Iterator<Item = String> {
        [self.spec, self.previous_spec]
            .into_iter()
            .flatten()
            .filter_map(|spec| spec.task_template?.container_spec?.configs)
            .flatten()
            .filter_map(|config| config.config_id)
    }
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
        keep: impl Fn(&ListedConfig) -> bool,
    ) -> Result<(), DockerError> {
        // Listed before services, so a config published after this listing
        // is never removed for lack of a service mounting it yet.
        let filters = serde_json::json!({ "label": labels }).to_string();
        let path = format!(
            "/configs?filters={}",
            url::form_urlencoded::byte_serialize(filters.as_bytes()).collect::<String>()
        );
        // Both listings stream, as they carry every payload and spec.
        let mut configs = self
            .engine_list::<ListedConfig>(&path)
            .await
            .map_err(|error| error.sanitized(kind.list()))?;
        configs.retain(|config| !keep(config));
        if configs.is_empty() {
            return Ok(());
        }
        // Bollard's service model omits `PreviousSpec`, so read the raw list.
        let referenced = Self::referenced_configs(
            self.engine_list::<ServiceConfigs>("/services")
                .await
                .map_err(|error| error.sanitized("list services"))?,
        );
        for config in configs {
            if referenced.contains(&config.id) {
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
    fn referenced_configs(services: Vec<ServiceConfigs>) -> BTreeSet<String> {
        services.into_iter().flat_map(ServiceConfigs::ids).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A rollback restores the previous spec, so its configs are kept.
    #[test]
    fn previous_specs_keep_their_configs() {
        let spec = |id: &str| serde_json::json!({"TaskTemplate": {"ContainerSpec": {"Configs": [{"ConfigID": id}]}}});
        let services = serde_json::from_value(serde_json::json!([
            {"Spec": spec("current"), "PreviousSpec": spec("previous")},
            {"Spec": {"TaskTemplate": {"ContainerSpec": {"Env": ["LARGE=…"], "Configs": null}}}},
        ]))
        .unwrap();
        assert_eq!(
            BollardDocker::referenced_configs(services),
            BTreeSet::from(["current".into(), "previous".into()])
        );
    }
}
