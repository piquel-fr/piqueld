//! `piqueld-probe` in services: the daemon embeds the static probe and
//! publishes it as a Swarm config, which services whose health check runs it
//! mount at [`piqueld_probe::CONTAINER_PATH`]. Swarm stores and distributes
//! the bytes, so nothing depends on host paths.

use super::configs::ConfigKind;
use super::{BollardDocker, DockerError, TaskSpecContainerSpec};
use bollard::models::{TaskSpecContainerSpecConfigs, TaskSpecContainerSpecFile1};
use piqueld_core::resource::MANAGED_LABEL;
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, fmt::Write as _, sync::LazyLock};

/// The probe the build script compiled for the daemon's architecture.
const BINARY: &[u8] = include_bytes!(env!("PIQUELD_PROBE_BINARY"));

/// The embedded probe's Swarm config. Configs are immutable, so it is named by
/// content: a daemon whose probe differs publishes a new config, services roll
/// onto it through their health-gated update, and image cleanup removes the
/// old one once no service mounts it.
pub(super) struct HealthProbe;

impl HealthProbe {
    /// Label marking the configs piqueld publishes for the probe.
    pub(super) const LABEL: &str = "io.piqueld.health-probe";
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

    /// Whether a config reference mounts a file at the probe's path, which
    /// no service file may use.
    pub(super) fn at_probe_path(config: &TaskSpecContainerSpecConfigs) -> bool {
        config.file.as_ref().and_then(|file| file.name.as_deref())
            == Some(piqueld_probe::CONTAINER_PATH)
    }

    /// Whether `container` mounts exactly this daemon's probe config at the
    /// probe's path when its health check runs the probe, and nothing there
    /// otherwise. A config from another probe version, such as one an older
    /// daemon published, is drift. Service files are checked separately.
    pub(super) fn matches(container: &TaskSpecContainerSpec) -> bool {
        let runs_probe = container
            .health_check
            .as_ref()
            .and_then(BollardDocker::observed_health)
            .is_some_and(|health| health.execution().runs_probe());
        let probes = container
            .configs
            .iter()
            .flatten()
            .filter(|config| Self::at_probe_path(config))
            .collect::<Vec<_>>();
        match probes.as_slice() {
            [] => !runs_probe,
            [config] => {
                runs_probe
                    && config.config_id.as_ref().is_some_and(|id| !id.is_empty())
                    && **config == Self::reference(config.config_id.clone().unwrap_or_default())
            }
            _ => false,
        }
    }
}

impl BollardDocker {
    /// Returns the reference mounting the probe config, publishing the config
    /// first when it is missing.
    ///
    /// # Errors
    /// Returns `ConfigurationConflict` when a config with the probe's name
    /// holds other content, and request errors otherwise.
    pub(super) async fn health_probe_reference(
        &self,
    ) -> Result<TaskSpecContainerSpecConfigs, DockerError> {
        let labels = BTreeMap::from([
            (MANAGED_LABEL.to_owned(), "true".to_owned()),
            (HealthProbe::LABEL.to_owned(), "true".to_owned()),
        ]);
        self.publish_config(
            ConfigKind::HealthProbe,
            HealthProbe::name(),
            &labels,
            BINARY,
        )
        .await
        .map(HealthProbe::reference)
    }

    /// Removes probe configs of other versions that no service mounts, now or
    /// in the previous spec a rollback restores. Retained configs go once the
    /// services mounting them roll onto the current probe and on again.
    ///
    /// # Errors
    /// Returns request errors listing services or listing or removing configs.
    pub(super) async fn remove_stale_health_probes(&self) -> Result<(), DockerError> {
        self.remove_unused_configs(
            ConfigKind::HealthProbe,
            &[HealthProbe::LABEL.to_owned()],
            |config| config.name() == HealthProbe::name(),
        )
        .await
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
            start_period_seconds: 0,
            retries: 3,
        };
        let command = HealthCheck::Command {
            command: vec!["true".into()],
            interval_seconds: 10,
            timeout_seconds: 3,
            start_period_seconds: 0,
            retries: 3,
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

        // Service files mount other configs elsewhere, checked separately.
        let file = TaskSpecContainerSpecConfigs {
            config_name: Some("piqueld-app-file-0000".into()),
            file: Some(TaskSpecContainerSpecFile1 {
                name: Some("/etc/app.conf".into()),
                ..probe.file.clone().unwrap()
            }),
            ..probe.clone()
        };

        assert!(matches(&http, vec![probe.clone()]));
        assert!(matches(&http, vec![file.clone(), probe.clone()]));
        assert!(matches(&command, vec![file]));
        assert!(!matches(&http, Vec::new()));
        assert!(!matches(&http, vec![older]));
        assert!(!matches(&http, vec![writable]));
        assert!(!matches(&http, vec![HealthProbe::reference(String::new())]));
        assert!(!matches(&http, vec![probe.clone(), probe.clone()]));
        assert!(!matches(&command, vec![probe]));
        assert!(matches(&command, Vec::new()));
    }

    #[test]
    fn the_embedded_probe_fits_a_swarm_config() {
        // Swarm rejects configs over 500 KiB on some Engine versions.
        assert!(BINARY.len() <= 500 * 1024, "{} bytes", BINARY.len());
        assert!(BINARY.starts_with(b"\x7fELF"));
        assert!(HealthProbe::name().starts_with("piqueld-health-probe-"));
    }
}
