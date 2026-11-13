//! Reports about the daemon and its host: status, DNS, statistics, doctor checks and profiles.

use super::health;
use crate::{profiles::ProfileSummary, support::join_or};
use piqueld_client::{
    DnsStatus, ImageStatus, PreviewUsage, SystemStatus, TelemetryStatus,
    observability::{Bytes, DaemonStats, Timestamp, Uptime},
    system::{HostConfiguration, IngressStatus, PublicIngressStatus, SystemCheck},
};
use serde::Serialize;
use std::time::Duration;

/// Daemon status; JSON is the daemon's status, human output adds the transport
/// used and, when readiness answered, each ingress listener.
pub(crate) struct StatusReport<'a> {
    pub(crate) status: &'a SystemStatus,
    pub(crate) transport: &'a str,
    pub(crate) ingress: Option<&'a IngressStatus>,
}
report!(StatusReport<'_> as SystemStatus = status, self, out, {
    let s = self.status;
    out.line(format_args!(
        "daemon {} (version {}, API {}, instance {})",
        s.status, s.daemon_version, s.api_version, s.instance_id
    ))?;
    out.label("Transport", self.transport)?;
    s.images.render_human(out)?;
    let tailnet = &s.tailscale;
    if tailnet.enabled {
        out.label(
            "Tailnet node",
            format_args!(
                "{} ({}, certificate expires at {})",
                tailnet.dns_name.as_deref().unwrap_or("unknown"),
                tailnet.state,
                tailnet
                    .certificate_expires_at_ms
                    .map_or_else(|| "unknown".into(), |at| Timestamp(at).to_string()),
            ),
        )?;
        if !tailnet.healthy || !tailnet.public_url_matches {
            out.label("Tailnet problem", &tailnet.message)?;
        }
    }
    if let Some(ingress) = self.ingress.filter(|ingress| ingress.enabled) {
        out.label(
            "Public listener",
            format_args!("{}: {}", health(ingress.healthy), ingress.message),
        )?;
        match &ingress.public {
            PublicIngressStatus::Direct => out.label("Public ingress", "ports 80/443")?,
            PublicIngressStatus::Tunnel {
                id,
                connected,
                message,
            } => {
                out.label(
                    "Public ingress",
                    format_args!(
                        "Cloudflare Tunnel {id} ({})",
                        if *connected {
                            "connected"
                        } else {
                            "not connected"
                        }
                    ),
                )?;
                if !connected {
                    out.label("Tunnel problem", message)?;
                }
            }
        }
        let private = &ingress.private;
        if private.enabled {
            out.label(
                "Private listener",
                format_args!(
                    "apps node {} ({}, {})",
                    private.dns_name.as_deref().unwrap_or("not joined"),
                    if private.state.is_empty() {
                        "unknown"
                    } else {
                        &private.state
                    },
                    join_or(&private.addresses, "no tailnet addresses"),
                ),
            )?;
            if !private.healthy {
                out.label("Private listener problem", &private.message)?;
            }
        }
    }
    match &s.previews {
        Some(previews) => previews.render_human(out)?,
        None => out.label("Previews", "unavailable: the daemon could not count them")?,
    }
    s.telemetry.render_human(out)?;
    s.dns.render_human(out)
});

// Where the daemon's own telemetry goes and how its export is going: the
// configured endpoint and each granted environment's receiving service.
// Headers appear only as the file they were read from.
report!(TelemetryStatus, self, out, {
    if self.destinations.is_empty() {
        return out.label(
            "Telemetry",
            "not exported ([telemetry] sets no endpoint and no environment receives it)",
        );
    }
    for destination in &self.destinations {
        let state = destination.state();
        let receiver = destination.receiver.as_ref().map_or_default(|receiver| {
            match (&receiver.application, &receiver.environment) {
                (Some(application), Some(environment)) => {
                    format!("{application} {environment} at ")
                }
                _ => format!("{} at ", receiver.environment_id),
            }
        });
        let last = destination
            .last_success_ms
            .map_or_default(|at| format!(", last export at {}", Timestamp(at)));
        let headers = destination
            .headers
            .as_ref()
            .map_or_default(|headers| format!(", headers {headers}"));
        out.label(
            "Telemetry",
            format_args!(
                "{receiver}{} ({state}{last}, {} records dropped{headers})",
                destination.endpoint, destination.dropped
            ),
        )?;
    }
    out.label("Telemetry exports", self.exported())
});

// The built images cleanup kept, and what it removed since the daemon started.
report!(ImageStatus, self, out, {
    match self.cleaned_at_ms {
        Some(at) => out.label(
            "Built images",
            format_args!(
                "{} kept; cleanup removed {} bytes of images since the daemon started (last run at {})",
                self.images,
                self.reclaimed_bytes,
                Timestamp(at)
            ),
        ),
        None => out.label("Built images", "not cleaned up yet"),
    }
});

// Previews against their limits, then each readable application's. A count
// over its limit, after the limit was lowered, is flagged.
report!(PreviewUsage, self, out, {
    let limits = &self.limits;
    let count = |count: u32, max: u32| {
        let over = if count > max {
            " (over the limit: existing previews keep running, new ones are refused)"
        } else {
            ""
        };
        format!("{count} of {max}{over}")
    };
    out.label("Previews", count(self.total, limits.max_total))?;
    out.label(
        "Preview usage",
        format_args!(
            "{} millicores and {} bytes of limits across preview replicas",
            self.cpu_millis, self.memory_bytes
        ),
    )?;
    if self.unlimited_replicas > 0 {
        out.label(
            "Unlimited replicas",
            format_args!(
                "{} preview replicas deployed before the limits applied run without them until redeployed",
                self.unlimited_replicas
            ),
        )?;
    }
    out.label(
        "Preview services",
        format_args!(
            "{} millicores and {} bytes unless they set limits; replicas capped at {}",
            limits.default_cpu_millis, limits.default_memory_bytes, limits.max_replicas
        ),
    )?;
    for application in &self.applications {
        out.label(
            &format!("Previews of {}", application.name),
            count(application.previews, limits.max_per_application),
        )?;
    }
    Ok(())
});

// DNS providers with their zones and health, then DNS-01 certificates; shown
// by `status` and `dns refresh`.
report!(DnsStatus, self, out, {
    for provider in &self.providers {
        out.label(
            "DNS provider",
            format_args!(
                "{} ({}{}): {}",
                provider.kind,
                health(provider.healthy),
                if provider.manage_records {
                    ", manages route records"
                } else {
                    ""
                },
                join_or(&provider.zones, "no zones"),
            ),
        )?;
        if !provider.healthy {
            out.label("DNS problem", &provider.message)?;
        }
    }
    for certificate in &self.certificates {
        out.label(
            "Certificate",
            format_args!(
                "{} for {} (expires at {})",
                certificate.name,
                join_or(&certificate.hostnames, "no routes"),
                certificate
                    .expires_at_ms
                    .map_or_else(|| "never issued".into(), |at| Timestamp(at).to_string()),
            ),
        )?;
        if let Some(error) = &certificate.error {
            out.label("Certificate problem", error)?;
        }
    }
    Ok(())
});

// `resources`: the measurements the dashboard's Daemon status page shows.
// Measurements the host cannot provide are unavailable rather than zero.
report!(DaemonStats, self, out, {
    let unavailable = || "unavailable on this host".to_owned();
    out.label("Measured", Timestamp(self.sampled_at_ms))?;
    out.label("Uptime", Uptime(Duration::from_secs(self.uptime_seconds)))?;
    out.label(
        "Memory",
        self.memory_bytes
            .map_or_else(unavailable, |bytes| format!("{} resident", Bytes(bytes))),
    )?;
    // CPU usage compares two measurements, so the daemon's first has none.
    out.label(
        "CPU",
        match (self.cpu_percent, self.cpu_seconds) {
            (Some(percent), _) => {
                format!("{percent:.1}% of one core, averaged since the previous measurement")
            }
            (None, Some(_)) => "not measured yet; ask again in 5 seconds".to_owned(),
            (None, None) => unavailable(),
        },
    )?;
    out.label(
        "Database",
        format_args!(
            "{} (WAL {})",
            Bytes(self.database_bytes),
            Bytes(self.wal_bytes)
        ),
    )?;
    out.label(
        "Available disk",
        self.available_disk_bytes.map_or_else(unavailable, |bytes| {
            format!("{} on the data directory's filesystem", Bytes(bytes))
        }),
    )?;
    out.label("Retained events", self.events)?;
    out.label("Diagnostic occurrences", self.diagnostics)?;
    out.label(
        "Retained build output",
        Bytes::from_i64(self.build_output_bytes),
    )?;
    out.label(
        "Operations",
        format_args!(
            "{} running, {} queued",
            self.running_operations, self.queued_operations
        ),
    )?;
    out.label(
        "Notification deliveries",
        format_args!(
            "{} awaiting a retry, {} failed",
            self.pending_deliveries, self.failed_deliveries
        ),
    )
});

// `settings`: each group of host settings, as the dashboard's Host settings
// page shows them.
report!(HostConfiguration, self, out, {
    for (index, (group, settings)) in self.groups.iter().enumerate() {
        if index > 0 {
            out.blank()?;
        }
        out.heading(&format!("{group}:"))?;
        for (setting, value) in settings {
            out.label(&format!("  {setting}"), value)?;
        }
    }
    Ok(())
});

/// How this CLI reaches the daemon, as `doctor` reports it.
#[derive(Serialize)]
pub(crate) struct Connection {
    /// `Unix socket <path>` or `TCP <url>`.
    pub(crate) endpoint: String,
    /// Where the endpoint came from, such as a profile.
    pub(crate) endpoint_source: String,
    /// Where the bearer token comes from, or `none`.
    pub(crate) credential: String,
}

/// `doctor`'s result: the connection, then the daemon's checks, which are
/// `null` when the daemon could not be asked.
///
/// ```text
/// Endpoint: Unix socket /run/piqueld/piqueld.sock (built-in default)
/// Credential: none
/// pass  Public URL: https://ops.example.com answers from this host …
/// warn  Tailnet node: …
///       Next step: …
/// ```
#[derive(Serialize)]
pub(crate) struct DoctorReport<'a> {
    pub(crate) connection: &'a Connection,
    pub(crate) checks: Option<&'a [SystemCheck]>,
}
report!(DoctorReport<'_>, self, out, {
    let connection = self.connection;
    out.label(
        "Endpoint",
        format_args!("{} ({})", connection.endpoint, connection.endpoint_source),
    )?;
    out.label("Credential", &connection.credential)?;
    let Some(checks) = self.checks else {
        return Ok(());
    };
    out.line(
        "Reaching the daemon proves this connection, not that browsers can reach the public URL.",
    )?;
    for check in checks {
        out.line(format_args!(
            "{:<4}  {}: {}",
            check.status.as_str(),
            check.id.label(),
            check.message
        ))?;
        if let Some(step) = &check.next_step {
            out.value("      ")?;
            out.label("Next step", step)?;
        }
    }
    Ok(())
});

/// Effective connection profiles, as a `NAME  ENDPOINT` table marking the default.
#[derive(Serialize)]
pub(crate) struct ProfilesReport<'a> {
    pub(crate) profiles: Vec<ProfileSummary<'a>>,
}
report!(ProfilesReport<'_>, self, out, {
    out.table(
        &self.profiles,
        "No profiles configured.",
        ["NAME", "ENDPOINT"],
        |profile| {
            let marker = if profile.default { "  (default)" } else { "" };
            [
                profile.name.to_string(),
                format!("{}{marker}", profile.endpoint),
            ]
        },
    )
});
