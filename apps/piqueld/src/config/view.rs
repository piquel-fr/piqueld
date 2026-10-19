//! The read-only configuration view the dashboard shows.
use super::DaemonConfig;
use crate::dns::provider::DnsProvider;
use piqueld_core::api::HostConfiguration;
use std::collections::BTreeMap;
use std::fmt::Display;

/// One group of the view: setting names and their display values.
type Group = BTreeMap<String, String>;

/// Builds a group from `(setting, value)` pairs.
fn group<const N: usize>(settings: [(&str, String); N]) -> Group {
    settings
        .into_iter()
        .map(|(setting, value)| (setting.to_owned(), value))
        .collect()
}

/// Displays an optional setting, `none` when unset.
fn or_none(value: Option<impl Display>) -> String {
    value.map_or_else(|| "none".into(), |value| value.to_string())
}

/// Displays a list of settings separated by commas.
fn joined<T: Display>(values: impl IntoIterator<Item = T>) -> String {
    values
        .into_iter()
        .map(|value| value.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

impl DaemonConfig {
    /// Projects effective settings into the read-only public dashboard response.
    ///
    /// Settings are grouped by section and rendered as display strings, e.g.
    /// `Server` -> `HTTP port` -> `7845`. Credentials appear only as their source.
    #[must_use]
    pub fn view(&self) -> HostConfiguration {
        let mut groups = BTreeMap::from([
            ("Server".into(), self.server_view()),
            ("Docker".into(), self.docker_view()),
            ("Ingress".into(), self.ingress_view()),
            ("Reconciliation".into(), self.reconciliation_view()),
            ("Retention".into(), self.retention_view()),
            ("Previews".into(), self.previews_view()),
            (
                "Local snapshots".into(),
                group([(
                    "Largest upload (bytes)",
                    self.snapshots.max_upload_bytes.to_string(),
                )]),
            ),
            (
                "Git checkouts".into(),
                group([(
                    "Largest total working trees (bytes)",
                    self.checkouts.max_bytes.to_string(),
                )]),
            ),
            ("Images".into(), self.images_view()),
            ("Authentication".into(), self.auth_view()),
            ("Tailscale".into(), self.tailscale_view()),
            ("Private ingress".into(), self.private_ingress_view()),
            ("Cloudflare Tunnel".into(), self.tunnel_view()),
            ("Observability".into(), self.observability_view()),
        ]);
        groups.extend(self.dns_view().map(|view| ("DNS providers".into(), view)));
        HostConfiguration { groups }
    }

    /// Builds the `Server` group.
    fn server_view(&self) -> Group {
        let server = &self.server;
        group([
            ("Data directory", server.data_dir.display().to_string()),
            ("Database", server.database_path().display().to_string()),
            ("API socket", server.socket_path().display().to_string()),
            ("HTTP listen mode", server.listen_mode.to_string()),
            ("HTTP port", server.port.to_string()),
        ])
    }

    /// Builds the `Docker` group.
    fn docker_view(&self) -> Group {
        group([
            ("Socket", self.docker.socket.display().to_string()),
            (
                "Initialize Swarm",
                self.docker.auto_initialize_swarm.to_string(),
            ),
        ])
    }

    /// Builds the `Reconciliation` group.
    fn reconciliation_view(&self) -> Group {
        let reconciliation = &self.reconciliation;
        group([
            (
                "Scan interval (seconds)",
                reconciliation.scan_interval_seconds.to_string(),
            ),
            (
                "Preparation timeout (seconds)",
                reconciliation.prepare_timeout_seconds.to_string(),
            ),
            (
                "Convergence timeout (seconds)",
                reconciliation.convergence_timeout_seconds.to_string(),
            ),
        ])
    }

    /// Builds the `Images` group.
    fn images_view(&self) -> Group {
        group([
            (
                "Deployments kept per environment",
                self.images.keep_deployments.to_string(),
            ),
            (
                "Cleanup interval (seconds)",
                self.images.cleanup_interval_seconds.to_string(),
            ),
        ])
    }

    /// Builds the `Previews` group.
    fn previews_view(&self) -> Group {
        let previews = &self.previews;
        group([
            ("Per application", previews.max_per_application.to_string()),
            ("Installation-wide", previews.max_total.to_string()),
            (
                "Default CPU (millicores)",
                previews.default_cpu_millis.to_string(),
            ),
            (
                "Default memory (bytes)",
                previews.default_memory_bytes.to_string(),
            ),
            ("Replicas per service", previews.max_replicas.to_string()),
        ])
    }

    /// Builds the `Retention` group.
    fn retention_view(&self) -> Group {
        let (retention, builds) = (&self.retention, &self.build_history);
        group([
            ("Deployment history", "Until application deletion".into()),
            (
                "Other finished operations (days)",
                retention.finished_operation_days.to_string(),
            ),
            ("Events (days)", retention.event_days.to_string()),
            ("Build output (days)", builds.log_retention_days.to_string()),
            (
                "Build output limit (bytes)",
                builds.log_max_bytes.to_string(),
            ),
        ])
    }

    /// Builds the `Authentication` group.
    fn auth_view(&self) -> Group {
        let days = self.auth.max_token_days;
        group([(
            "Longest token lifetime (days)",
            days.map_or_else(|| "unlimited".into(), |days| days.to_string()),
        )])
    }

    /// Builds the `Tailscale` group. The auth key appears only as its file.
    fn tailscale_view(&self) -> Group {
        let tailscale = &self.tailscale;
        group([
            ("Enabled", tailscale.enabled.to_string()),
            ("Hostname", tailscale.hostname.clone()),
            ("Auth key", or_none(tailscale.auth_key_file.as_ref())),
            (
                "tailscaled",
                tailscale.socket.as_ref().map_or_else(
                    || format!("own, state in {}", self.server.tailscale_dir().display()),
                    |socket| format!("shared, {}", socket.display()),
                ),
            ),
            ("HTTPS port", tailscale.https_port.to_string()),
            ("Public URL", self.public_url().to_owned()),
        ])
    }

    /// Builds the `Ingress` group.
    fn ingress_view(&self) -> Group {
        let ingress = &self.ingress;
        let addresses = &ingress.public_addresses;
        group([
            ("Enabled (restart required)", ingress.enabled.to_string()),
            (
                "Public addresses",
                if addresses.is_empty() {
                    "none".into()
                } else {
                    joined(addresses)
                },
            ),
            ("ACME directory", ingress.acme.directory.clone()),
            ("ACME email", or_none(ingress.acme.email.as_ref())),
            (
                "Webhook hostname",
                or_none(ingress.webhook_hostname.as_ref()),
            ),
        ])
    }

    /// Builds the `Private ingress` group. The auth key appears only as its file.
    fn private_ingress_view(&self) -> Group {
        let private = &self.ingress.private;
        let state = self.server.data_dir.join("ingress/tailscale");
        group([
            ("Enabled (restart required)", private.enabled.to_string()),
            ("Hostname", private.hostname.clone()),
            (
                "Dashboard hostname",
                or_none(private.dashboard_hostname.as_ref()),
            ),
            ("Auth key", or_none(private.auth_key.as_ref())),
            ("State directory", state.display().to_string()),
        ])
    }

    /// Builds the `Cloudflare Tunnel` group. The credentials appear only as
    /// their file.
    fn tunnel_view(&self) -> Group {
        let credentials = self.ingress.tunnel.credentials.as_ref();
        group([
            (
                "Enabled (restart required)",
                credentials.is_some().to_string(),
            ),
            (
                "Tunnel ID",
                or_none(credentials.map(|credentials| credentials.id)),
            ),
            (
                "Credentials",
                or_none(credentials.map(|credentials| &credentials.file)),
            ),
        ])
    }

    /// Builds the `DNS providers` group, numbered in configuration order, or
    /// none without providers. Credentials appear only as their files.
    fn dns_view(&self) -> Option<Group> {
        if self.dns.providers.is_empty() {
            return None;
        }
        let providers = self.dns.providers.iter().enumerate();
        Some(
            providers
                .map(|(index, config)| {
                    let provider = &config.provider;
                    let account = match provider {
                        DnsProvider::Cloudflare(cloudflare) => {
                            format!("API token {}", cloudflare.api_token())
                        }
                        DnsProvider::Ovh(ovh) => format!(
                            "{}, application key {}, application secret {}, consumer key {}",
                            ovh.endpoint,
                            ovh.application_key,
                            ovh.application_secret,
                            ovh.consumer_key
                        ),
                        #[cfg(test)]
                        DnsProvider::Challtestsrv(_) | DnsProvider::Test(_) => "test server".into(),
                    };
                    let records = if config.manage_records {
                        "manages route records"
                    } else {
                        "route records are manual"
                    };
                    (
                        format!("{}. {}", index + 1, provider.kind()),
                        format!("{account}; {records}"),
                    )
                })
                .collect(),
        )
    }

    /// Builds the `Observability` group, listing notification destinations by
    /// name only so webhook URLs never reach the dashboard, and telemetry
    /// headers by their file.
    fn observability_view(&self) -> Group {
        let notifications = &self.notifications;
        let destinations = notifications.destinations.iter().map(|destination| {
            let state = if destination.enabled {
                "enabled"
            } else {
                "disabled"
            };
            format!("{} ({state}, URL {})", destination.name, destination.url)
        });
        group([
            (
                "Telemetry endpoint",
                or_none(self.telemetry.endpoint.as_ref()),
            ),
            (
                "Telemetry headers",
                or_none(self.telemetry.headers.as_ref()),
            ),
            (
                "Telemetry containers",
                self.telemetry.containers.to_string(),
            ),
            (
                "Daemon history (days)",
                self.retention.daemon_event_days.to_string(),
            ),
            ("Audit trail (days)", self.retention.audit_days.to_string()),
            ("Notifications enabled", notifications.enabled.to_string()),
            (
                "Notification categories",
                joined(
                    notifications
                        .enabled_categories()
                        .iter()
                        .map(|category| category.as_str()),
                ),
            ),
            ("Notification destinations", joined(destinations)),
            (
                "Notification failure threshold (seconds)",
                notifications.failure_threshold_seconds.to_string(),
            ),
            (
                "Webhook retry window (seconds)",
                notifications.retry_window_seconds.to_string(),
            ),
        ])
    }
}
