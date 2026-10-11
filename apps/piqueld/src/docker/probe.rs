//! `piqueld-probe` in services: the daemon embeds the static probe and
//! publishes it as a Swarm config, which services whose health check runs it
//! mount at [`piqueld_probe::CONTAINER_PATH`]. Swarm stores and distributes
//! the bytes, so nothing depends on host paths.

use super::{BollardDocker, DockerError, TaskSpecContainerSpec};
use base64::Engine as _;
use bollard::models::{TaskSpecContainerSpecConfigs, TaskSpecContainerSpecFile1};
use hyper::{Method, StatusCode};
use piqueld_core::resource::MANAGED_LABEL;
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, fmt::Write as _, sync::LazyLock};

/// The probe the build script compiled for the daemon's architecture.
const BINARY: &[u8] = include_bytes!(env!("PIQUELD_PROBE_BINARY"));

/// The embedded probe's Swarm config. Configs are immutable, so it is named by
/// content: a daemon whose probe differs publishes a new config, services roll
/// onto it through their health-gated update, and image cleanup removes the
/// old one once no service mounts it.
pub(super) struct HealthProbe;

/// The parts of an inspected or listed config piqueld reads.
#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Config {
    #[serde(rename = "ID")]
    id: String,
    spec: ConfigSpec,
}

/// A config's name and base64 content.
#[derive(serde::Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ConfigSpec {
    name: String,
    #[serde(default)]
    data: String,
}

impl HealthProbe {
    /// Label marking the configs piqueld publishes for the probe.
    const LABEL: &str = "io.piqueld.health-probe";
    /// Root-owned, and readable and executable by every container user.
    const MODE: u32 = 0o555;

    /// `piqueld-health-probe-<first 16 hex digits of the binary's SHA-256>`.
    fn name() -> &'static str {
        static NAME: LazyLock<String> = LazyLock::new(|| {
            Sha256::digest(BINARY)[..8].iter().fold(
                String::from("piqueld-health-probe-"),
                |mut name, byte| {
                    let _ = write!(name, "{byte:02x}");
                    name
                },
            )
        });
        &NAME
    }

    /// The reference mounting the probe config `id` at the container path.
    fn reference(id: String) -> TaskSpecContainerSpecConfigs {
        TaskSpecContainerSpecConfigs {
            config_id: Some(id),
            config_name: Some(Self::name().into()),
            file: Some(TaskSpecContainerSpecFile1 {
                name: Some(piqueld_probe::CONTAINER_PATH.into()),
                uid: Some("0".into()),
                gid: Some("0".into()),
                mode: Some(Self::MODE),
            }),
            runtime: None,
        }
    }

    /// IDs of the configs the listed `services` mount in their current or
    /// previous specs.
    fn referenced(services: &[serde_json::Value]) -> BTreeSet<String> {
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

    /// Whether `container` mounts exactly this daemon's probe config when its
    /// health check runs the probe, and no config otherwise. A config from
    /// another probe version, such as one an older daemon published, is drift.
    pub(super) fn matches(container: &TaskSpecContainerSpec) -> bool {
        let runs_probe = container
            .health_check
            .as_ref()
            .and_then(BollardDocker::observed_health)
            .is_some_and(|health| health.execution().runs_probe());
        match container.configs.as_deref().unwrap_or_default() {
            [] => !runs_probe,
            [config] => {
                runs_probe
                    && config.config_id.as_ref().is_some_and(|id| !id.is_empty())
                    && *config == Self::reference(config.config_id.clone().unwrap_or_default())
            }
            _ => false,
        }
    }
}

impl BollardDocker {
    /// Returns the reference mounting the probe config, publishing the config
    /// first when it is missing. A creation race (HTTP 409) is resolved by
    /// reading the config that won.
    ///
    /// # Errors
    /// Returns `ConfigurationConflict` when a config with the probe's name
    /// holds other content, and request errors otherwise.
    pub(super) async fn health_probe_reference(
        &self,
    ) -> Result<TaskSpecContainerSpecConfigs, DockerError> {
        if let Some(id) = self.health_probe_config().await? {
            return Ok(HealthProbe::reference(id));
        }
        let body = serde_json::json!({
            "Name": HealthProbe::name(),
            "Labels": {MANAGED_LABEL: "true", HealthProbe::LABEL: "true"},
            "Data": base64::engine::general_purpose::STANDARD.encode(BINARY),
        });
        match self
            .engine_request(Method::POST, "/configs/create", body.to_string().into())
            .await
        {
            Ok(_) => {}
            Err(error) if error.is_status(StatusCode::CONFLICT) => {}
            Err(error) => return Err(error.sanitized("publish the HTTP health probe")),
        }
        self.health_probe_config()
            .await?
            .map(HealthProbe::reference)
            .ok_or(DockerError::Request("publish the HTTP health probe"))
    }

    /// Returns the probe config's ID, or `None` when it does not exist.
    async fn health_probe_config(&self) -> Result<Option<String>, DockerError> {
        let bytes = match self
            .engine_request(
                Method::GET,
                &format!("/configs/{}", HealthProbe::name()),
                Vec::new(),
            )
            .await
        {
            Ok(bytes) => bytes,
            Err(error) if error.is_not_found() => return Ok(None),
            Err(error) => return Err(error.sanitized("inspect the HTTP health probe")),
        };
        let config: Config = serde_json::from_slice(&bytes)
            .map_err(|source| DockerError::request("decode the HTTP health probe", source))?;
        // The name promises the content; anything else was not published here.
        if config.spec.data != base64::engine::general_purpose::STANDARD.encode(BINARY) {
            return Err(DockerError::ConfigurationConflict);
        }
        Ok(Some(config.id))
    }

    /// Removes probe configs of other versions that no service mounts, now or
    /// in the previous spec a rollback restores: Docker only refuses to remove
    /// configs current specs use (HTTP 400). Retained configs go once the
    /// services mounting them roll onto the current probe and on again.
    ///
    /// # Errors
    /// Returns request errors listing services or listing or removing configs.
    pub(super) async fn remove_stale_health_probes(&self) -> Result<(), DockerError> {
        // Bollard's service model omits `PreviousSpec`, so read the raw list.
        let services = self
            .engine_request(Method::GET, "/services", Vec::new())
            .await
            .map_err(|error| error.sanitized("list services"))?;
        let services: Vec<serde_json::Value> = serde_json::from_slice(&services)
            .map_err(|source| DockerError::request("decode services", source))?;
        let referenced = HealthProbe::referenced(&services);
        let filters = serde_json::json!({"label": [HealthProbe::LABEL]}).to_string();
        let path = format!(
            "/configs?filters={}",
            url::form_urlencoded::byte_serialize(filters.as_bytes()).collect::<String>()
        );
        let bytes = self
            .engine_request(Method::GET, &path, Vec::new())
            .await
            .map_err(|error| error.sanitized("list HTTP health probes"))?;
        let configs: Vec<Config> = serde_json::from_slice(&bytes)
            .map_err(|source| DockerError::request("decode HTTP health probes", source))?;
        for config in configs {
            if config.spec.name == HealthProbe::name() || referenced.contains(&config.id) {
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
                Err(error) => return Err(error.sanitized("remove an old HTTP health probe")),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::HealthCheck;
    use super::*;

    /// The probe config belongs exactly when the health check runs the probe,
    /// and only as this daemon's version mounted at the container path.
    #[test]
    fn the_probe_config_must_match_the_health_check() {
        let http = HealthCheck::Http {
            port: 8080,
            path: "/health".into(),
            interval_seconds: 10,
            timeout_seconds: 3,
        };
        let command = HealthCheck::Command {
            command: vec!["true".into()],
            interval_seconds: 10,
            timeout_seconds: 3,
        };
        let matches = |health: &HealthCheck, configs: Vec<TaskSpecContainerSpecConfigs>| {
            HealthProbe::matches(&TaskSpecContainerSpec {
                health_check: Some(BollardDocker::health_config(health)),
                configs: Some(configs),
                ..Default::default()
            })
        };
        let probe = HealthProbe::reference("config-id".into());
        let older = TaskSpecContainerSpecConfigs {
            config_name: Some("piqueld-health-probe-0000000000000000".into()),
            ..probe.clone()
        };
        let mut writable = probe.clone();
        writable.file.as_mut().unwrap().mode = Some(0o777);

        assert!(matches(&http, vec![probe.clone()]));
        assert!(!matches(&http, Vec::new()));
        assert!(!matches(&http, vec![older]));
        assert!(!matches(&http, vec![writable]));
        assert!(!matches(&http, vec![HealthProbe::reference(String::new())]));
        assert!(!matches(&http, vec![probe.clone(), probe.clone()]));
        assert!(!matches(&command, vec![probe]));
        assert!(matches(&command, Vec::new()));
    }

    /// A rollback restores the previous spec, so its probe config is kept.
    #[test]
    fn previous_specs_keep_their_probe_configs() {
        let spec = |id: &str| serde_json::json!({"TaskTemplate": {"ContainerSpec": {"Configs": [{"ConfigID": id}]}}});
        let services = [
            serde_json::json!({"Spec": spec("current"), "PreviousSpec": spec("previous")}),
            serde_json::json!({"Spec": {"TaskTemplate": {"ContainerSpec": {}}}}),
        ];
        assert_eq!(
            HealthProbe::referenced(&services),
            BTreeSet::from(["current".into(), "previous".into()])
        );
    }

    #[test]
    fn the_embedded_probe_fits_a_swarm_config() {
        // Swarm rejects configs over 500 KiB on some Engine versions.
        assert!(BINARY.len() <= 500 * 1024, "{} bytes", BINARY.len());
        assert!(BINARY.starts_with(b"\x7fELF"));
        assert!(HealthProbe::name().starts_with("piqueld-health-probe-"));
    }
}
