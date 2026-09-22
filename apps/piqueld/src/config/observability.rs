use super::ConfigError;
use serde::Deserialize;

/// Metrics-only HTTP endpoints. An empty list disables exposure.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct MetricsConfig {
    /// Explicit socket addresses; administrative API routes are never served here.
    pub listen: Vec<std::net::SocketAddr>,
}

/// Startup-loaded global notification policy.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(default, deny_unknown_fields)]
// Independent user-configurable category switches are not mutually exclusive states.
#[allow(clippy::struct_excessive_bools)]
pub struct NotificationConfig {
    /// Master switch. Re-enabling never replays old events.
    pub enabled: bool,
    /// Notify on build failures.
    pub build_failures: bool,
    /// Notify on deployment attempt failures.
    pub deployment_failures: bool,
    /// Notify on sustained service degradation.
    pub service_degradation: bool,
    /// Notify on shared dependency and internal failures.
    pub daemon_failures: bool,
    /// Notify when an alerted condition clears.
    pub recovery: bool,
    /// Sustained failure observation threshold.
    pub failure_threshold_seconds: u64,
    /// Maximum automatic delivery retry window.
    pub retry_window_seconds: u64,
    /// Named delivery destinations.
    pub destinations: Vec<WebhookDestination>,
}
impl Default for NotificationConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            build_failures: true,
            deployment_failures: true,
            service_degradation: true,
            daemon_failures: true,
            recovery: true,
            failure_threshold_seconds: 120,
            retry_window_seconds: 86_400,
            destinations: Vec::new(),
        }
    }
}
impl NotificationConfig {
    pub(super) fn validate(&self) -> Result<(), ConfigError> {
        if self.failure_threshold_seconds > 86_400
            || !(1..=604_800).contains(&self.retry_window_seconds)
        {
            return Err(ConfigError::Invalid("notification threshold must be 0..=86400 seconds and retry window 1..=604800 seconds".into()));
        }
        let mut names = std::collections::BTreeSet::new();
        for destination in &self.destinations {
            if destination.name.is_empty()
                || destination.name.len() > 63
                || !names.insert(&destination.name)
            {
                return Err(ConfigError::Invalid(
                    "webhook names must be unique and contain 1..=63 bytes".into(),
                ));
            }
            let url = reqwest::Url::parse(&destination.url)
                .map_err(|_| ConfigError::Invalid("webhook URL is invalid".into()))?;
            let loopback_http = url.scheme() == "http"
                && url.host_str().is_some_and(|host| {
                    host == "localhost"
                        || host
                            .trim_matches(['[', ']'])
                            .parse::<std::net::IpAddr>()
                            .is_ok_and(|ip| ip.is_loopback())
                });
            if !(url.scheme() == "https" || loopback_http)
                || url.host_str().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.fragment().is_some()
            {
                return Err(ConfigError::Invalid(
                    "webhook URL requires HTTPS (or HTTP on loopback), a host, and no userinfo or fragment".into(),
                ));
            }
        }
        Ok(())
    }
    pub(crate) fn category_enabled(&self, category: &str) -> bool {
        self.enabled
            && match category {
                "build_failures" => self.build_failures,
                "deployment_failures" => self.deployment_failures,
                "service_degradation" => self.service_degradation,
                "daemon_failures" => self.daemon_failures,
                "recovery" => self.recovery,
                _ => false,
            }
    }
    pub(crate) fn enabled_categories(&self) -> Vec<&'static str> {
        [
            "build_failures",
            "deployment_failures",
            "service_degradation",
            "daemon_failures",
            "recovery",
        ]
        .into_iter()
        .filter(|category| self.category_enabled(category))
        .collect()
    }
}
/// Webhook payload format.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum WebhookKind {
    /// Structured versioned JSON containing the event.
    #[default]
    Json,
    /// Discord-compatible message, with mentions disabled.
    Discord,
}
/// One configured destination. Its URL is deliberately excluded from debug output.
#[derive(Clone, Deserialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct WebhookDestination {
    /// Stable public name, used in the delivery ledger.
    pub name: String,
    /// HTTP(S) endpoint; may contain a credential and must never be displayed.
    pub url: String,
    /// Whether pending and new deliveries are enabled.
    #[serde(default = "enabled")]
    pub enabled: bool,
    /// Receiver payload format.
    #[serde(default)]
    pub kind: WebhookKind,
}
const fn enabled() -> bool {
    true
}
impl std::fmt::Debug for WebhookDestination {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebhookDestination")
            .field("name", &self.name)
            .field("enabled", &self.enabled)
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webhook_urls_require_tls_except_on_loopback() {
        for (url, accepted) in [
            ("https://hooks.example.com/secret", true),
            ("http://localhost:8080/secret", true),
            ("http://127.0.0.1:8080/secret", true),
            ("http://127.0.0.2/secret", true),
            ("http://[::1]:8080/secret", true),
            ("http://hooks.example.com/secret", false),
            ("http://192.168.1.2/secret", false),
            ("http://[2001:db8::1]/secret", false),
            ("http://localhost.example.com/secret", false),
            ("https://user:password@hooks.example.com/secret", false),
            ("https://hooks.example.com/secret#fragment", false),
            ("file:///secret", false),
        ] {
            let config = NotificationConfig {
                destinations: vec![WebhookDestination {
                    name: "test".into(),
                    url: url.into(),
                    enabled: true,
                    kind: WebhookKind::Json,
                }],
                ..NotificationConfig::default()
            };
            assert_eq!(config.validate().is_ok(), accepted, "{url}");
        }
    }
}
