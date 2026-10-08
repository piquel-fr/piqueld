//! Typed command results. Human-only context never leaks into JSON schemas.
use super::{HumanWriter, Report};
use crate::profiles::ProfileSummary;
use piqueld_client::{
    AcceptedOperation, ActionReason, ActionRisk, ApplicationLogs, ApplicationSummary,
    ApplicationView, BuildLogPage, BuildRecord, DeletedApplication, DnsStatus,
    EnvironmentDetailView, EnvironmentSource, EnvironmentStatusView, EnvironmentView, Event,
    Operation, OperationState, Page, PlanView, SavedApplication, SecretMetadata, Source,
    SystemStatus,
};
use serde::Serialize;
use std::io;

// Identity JSON is the common case; keep its implementation in one place.
// `report!(Type, self, out, { ... })` implements `Report` with `Json = Self` and the
// block as `render_human`.
macro_rules! report {
    ($ty:ty, $this:ident, $out:ident, $body:block) => {
        impl Report for $ty {
            type Json = Self;
            fn json(&self) -> &Self { self }
            fn render_human(&$this, $out: &mut HumanWriter<'_>) -> io::Result<()> $body
        }
    };
}

/// Daemon status; JSON is the daemon's status, human output adds the transport used.
pub(crate) struct StatusReport<'a> {
    pub(crate) status: &'a SystemStatus,
    pub(crate) transport: &'a str,
}
impl Report for StatusReport<'_> {
    type Json = SystemStatus;
    fn json(&self) -> &Self::Json {
        self.status
    }
    fn render_human(&self, out: &mut HumanWriter<'_>) -> io::Result<()> {
        let s = self.status;
        out.line(format_args!(
            "daemon {} (version {}, API {}, instance {})",
            s.status, s.daemon_version, s.api_version, s.instance_id
        ))?;
        out.label("Transport", self.transport)?;
        let tailnet = &s.tailscale;
        if tailnet.enabled {
            out.label(
                "Tailnet node",
                format_args!(
                    "{} ({}, certificate expires at Unix ms {})",
                    tailnet.dns_name.as_deref().unwrap_or("unknown"),
                    tailnet.state,
                    tailnet
                        .certificate_expires_at_ms
                        .map_or_else(|| "unknown".into(), |at| at.to_string()),
                ),
            )?;
            if !tailnet.healthy || !tailnet.public_url_matches {
                out.label("Tailnet problem", &tailnet.message)?;
            }
        }
        s.dns.render_human(out)
    }
}

// DNS providers with their zones and health, then DNS-01 certificates; shown
// by `status` and `dns refresh`.
report!(DnsStatus, self, out, {
    for provider in &self.providers {
        out.label(
            "DNS provider",
            format_args!(
                "{} ({}): {}",
                provider.kind,
                if provider.healthy {
                    "healthy"
                } else {
                    "unhealthy"
                },
                if provider.zones.is_empty() {
                    "no zones".into()
                } else {
                    provider.zones.join(", ")
                },
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
                "{} for {} (expires at Unix ms {})",
                certificate.name,
                if certificate.hostnames.is_empty() {
                    "no routes".into()
                } else {
                    certificate.hostnames.join(", ")
                },
                certificate
                    .expires_at_ms
                    .map_or_else(|| "never issued".into(), |at| at.to_string()),
            ),
        )?;
        if let Some(error) = &certificate.error {
            out.label("Certificate problem", error)?;
        }
    }
    Ok(())
});

/// Effective connection profiles, as a `NAME  ENDPOINT` table.
#[derive(Serialize)]
pub(crate) struct ProfilesReport<'a> {
    pub(crate) profiles: Vec<ProfileSummary<'a>>,
}
report!(ProfilesReport<'_>, self, out, {
    if self.profiles.is_empty() {
        return out.line("No profiles configured.");
    }
    out.heading("NAME  ENDPOINT")?;
    for profile in &self.profiles {
        out.line(format_args!("{}  {}", profile.name, profile.endpoint))?;
    }
    Ok(())
});

/// One environment with its status, as listed by `env list` and inside `app list`.
#[derive(Serialize)]
pub(crate) struct EnvironmentRow {
    pub(crate) environment: EnvironmentView,
    /// `None` when the status request failed (rendered as `unavailable`).
    pub(crate) status: Option<EnvironmentStatusView>,
}

impl EnvironmentRow {
    /// Current state, or `unavailable` when the status could not be read.
    fn state(&self) -> String {
        self.status
            .as_ref()
            .map_or_else(|| "unavailable".to_owned(), |s| s.state.to_string())
    }
}

/// Where an environment deploys from: `saved`, or the branch it follows,
/// e.g. `main` or `main@<commit>`.
fn source(source: &EnvironmentSource) -> String {
    match source {
        EnvironmentSource::Saved => "saved".into(),
        EnvironmentSource::Branch(branch) => branch.to_string(),
    }
}

impl Report for Vec<EnvironmentRow> {
    type Json = [EnvironmentRow];
    fn json(&self) -> &Self::Json {
        self
    }
    fn render_human(&self, out: &mut HumanWriter<'_>) -> io::Result<()> {
        if self.is_empty() {
            return out.line("No environments.");
        }
        out.heading("NAME  STATE  SOURCE  RESOLVED  ID")?;
        for row in self {
            let environment = &row.environment;
            out.line(format_args!(
                "{}  {}  {}  {}  {}",
                environment.name,
                row.state(),
                source(&environment.source),
                environment
                    .resolved_generation
                    .map_or_else(|| "none".to_owned(), |v| v.to_string()),
                environment.id
            ))?;
        }
        Ok(())
    }
}

/// One `app list` row.
#[derive(Serialize)]
pub(crate) struct ApplicationRow {
    pub(crate) application: ApplicationSummary,
    pub(crate) environments: Vec<EnvironmentRow>,
}

report!(Page<ApplicationRow>, self, out, {
    if self.items.is_empty() {
        return out.line("No applications.");
    }
    out.heading("NAME  ENVIRONMENTS  GENERATION  ID")?;
    for row in &self.items {
        let environments = if row.environments.is_empty() {
            "none".to_owned()
        } else {
            row.environments
                .iter()
                .map(|environment| {
                    format!("{} {}", environment.environment.name, environment.state())
                })
                .collect::<Vec<_>>()
                .join(", ")
        };
        out.line(format_args!(
            "{}  {}  {}  {}",
            row.application.name, environments, row.application.generation, row.application.id
        ))?;
    }
    Ok(())
});

/// `app show` result: configuration, environments with their state, and
/// per-service sources.
#[derive(Serialize)]
pub(crate) struct ShowReport<'a> {
    pub(crate) application: &'a ApplicationView,
    pub(crate) environments: &'a [EnvironmentRow],
}
report!(ShowReport<'_>, self, out, {
    let app = self.application;
    out.line(format_args!(
        "{} ({})",
        app.application.metadata().name,
        app.application.id()
    ))?;
    out.blank()?;
    out.label("Configuration revision", app.generation)?;
    for row in self.environments {
        out.label(
            "Environment",
            format_args!(
                "{} ({}): {}, deploys from {}",
                row.environment.name,
                row.environment.id,
                row.state(),
                source(&row.environment.source)
            ),
        )?;
    }
    if self.environments.is_empty() {
        out.label("Environments", "none")?;
    }
    for service in &app.application.spec().services {
        out.blank()?;
        out.label("Service", &service.name)?;
        out.label("  Replicas", &service.replicas)?;
        match &service.source {
            Source::Image { image } => out.label("  Source", format_args!("image {image}"))?,
            Source::Git { repository, .. } => {
                out.label("  Source", format_args!("git {repository}"))?;
            }
        }
    }
    if !app.application.spec().volumes.is_empty() {
        out.label(
            "Named volumes",
            app.application
                .spec()
                .volumes
                .iter()
                .map(|v| v.name.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        )?;
        out.line("Named volumes are retained on deletion.")?;
    }
    let spec = app.application.spec();
    if !spec.variables.is_empty() || !spec.environments.is_empty() {
        out.blank()?;
        for (name, value) in &spec.variables {
            out.label("Variable", format_args!("{name} = {value}"))?;
        }
        for (environment, config) in &spec.environments {
            for (name, value) in &config.variables {
                out.label(
                    "Variable",
                    format_args!("{name} = {value} (in {environment})"),
                )?;
            }
        }
    }
    Ok(())
});

/// `env show` result: an environment's intent, the manifest it deploys, and
/// its runtime state.
#[derive(Serialize)]
#[serde(transparent)]
pub(crate) struct EnvironmentShowReport<'a>(pub(crate) &'a EnvironmentDetailView);
report!(EnvironmentShowReport<'_>, self, out, {
    let EnvironmentDetailView {
        environment,
        application,
        manifest,
        status,
        ..
    } = self.0;
    out.line(format_args!(
        "{} of {} ({})",
        environment.name,
        application.application.metadata().name,
        environment.id
    ))?;
    out.blank()?;
    out.label("Intent", status.state)?;
    out.label(
        "Runtime",
        status.runtime_health.as_deref().unwrap_or("unknown"),
    )?;
    out.label("Source", source(&environment.source))?;
    if let (EnvironmentSource::Branch(_), None) = (&environment.source, manifest) {
        out.label("Manifest", "not fetched yet; deploy to fetch it")?;
    }
    out.label("Configuration revision", application.generation)?;
    out.label(
        "Resolved revision",
        environment
            .resolved_generation
            .map_or_else(|| "none".to_owned(), |v| v.to_string()),
    )?;
    let values = manifest
        .iter()
        .flat_map(|manifest| manifest.values(&environment.name));
    for (name, value) in values {
        match value {
            Some(value) => out.label("Variable", format_args!("{name} = {value}"))?,
            None => out.label("Variable", format_args!("{name} has no value"))?,
        }
    }
    Ok(())
});

report!(EnvironmentView, self, out, {
    out.line(format_args!(
        "Environment {} ({}), deploys from {}.",
        self.name, self.id, self.source
    ))
});

report!(ApplicationLogs, self, out, {
    for log in &self.items {
        out.value(format_args!(
            "{} {} {} {} | ",
            log.timestamp, log.service, log.task_id, log.stream
        ))?;
        out.log_text(&log.message)?;
        out.blank()?;
    }
    Ok(())
});

// Build output is joined before cleaning, then printed as log text.
report!(BuildLogPage, self, out, {
    // ANSI sequences can span capture chunks within this fetched page.
    let text = self
        .items
        .iter()
        .map(|chunk| chunk.text.as_str())
        .collect::<String>();
    out.log_text(&piqueld_client::LogRecord::clean_message(&text))
});

report!(Page<BuildRecord>, self, out, {
    for build in &self.items {
        // Job runs share build history; name the job and its service.
        let subject = build.job.as_ref().map_or_else(
            || build.service.clone(),
            |job| format!("job {job} ({})", build.service),
        );
        out.line(format_args!(
            "{}  {}  {}  {:?}  {}",
            build.id, build.environment_id, subject, build.state, build.started_at_ms
        ))?;
    }
    if let Some(cursor) = &self.next_cursor {
        out.label("Next cursor", cursor)?;
    }
    Ok(())
});

/// Secret metadata list, flagging unreadable values and pending deletions.
impl Report for Vec<SecretMetadata> {
    type Json = [SecretMetadata];
    fn json(&self) -> &Self::Json {
        self
    }
    fn render_human(&self, out: &mut HumanWriter<'_>) -> io::Result<()> {
        for secret in self {
            out.line(format_args!(
                "{}  generation {}{}{}",
                secret.name,
                secret.generation,
                if secret.unavailable {
                    "  value unavailable; replace value and deploy"
                } else {
                    ""
                },
                if secret.deleting {
                    "  deletion pending; retry delete"
                } else {
                    ""
                }
            ))?;
        }
        Ok(())
    }
}

report!(SecretMetadata, self, out, {
    out.line(format_args!(
        "Saved {} generation {}. Deploy the application to use it.",
        self.name, self.generation
    ))
});

/// Name of a deleted secret.
#[derive(Serialize)]
pub(crate) struct SecretDeletionReport<'a> {
    pub(crate) deleted: &'a str,
}
report!(SecretDeletionReport<'_>, self, out, {
    out.line(format_args!("Deleted {}.", self.deleted))
});

report!(Page<Event>, self, out, {
    for event in &self.items {
        out.line(format_args!(
            "{}  {}  {}  {}  attempt {}  {} {} {} {}{}",
            event.id,
            event.created_at_ms,
            event.kind,
            event.operation_id.as_deref().unwrap_or("-"),
            event.attempt.map_or_else(|| "-".into(), |v| v.to_string()),
            event.phase.as_deref().unwrap_or(""),
            event.resource.as_deref().unwrap_or(""),
            event.error_code.as_deref().unwrap_or(""),
            event.message.as_deref().unwrap_or(""),
            event
                .actor_user_id
                .as_deref()
                .map_or_else(String::new, |actor| format!("  by {actor}"))
        ))?;
    }
    if let Some(cursor) = &self.next_cursor {
        out.label("Next cursor", cursor)?;
    }
    Ok(())
});

report!(Operation, self, out, {
    out.label("Operation", &self.id)?;
    out.label("  State", self.state)?;
    out.label("  Environment", &self.environment_id)?;
    if let Some(phase) = &self.phase {
        out.label("  Phase", phase.replace('_', " "))?;
    }
    if let Some(resource) = &self.resource {
        out.label("  Resource", resource)?;
    }
    Ok(())
});

report!(SavedApplication, self, out, {
    if let Some(id) = &self.operation_id {
        out.line(format_args!(
            "Accepted deployment {id} for application {}",
            self.application_id
        ))
    } else {
        out.line(format_args!(
            "Saved application {} (configuration revision {}). Not deployed.",
            self.application_id, self.generation
        ))
    }
});

report!(AcceptedOperation, self, out, {
    out.line(format_args!(
        "Accepted operation {} for environment {}",
        self.operation_id, self.environment_id
    ))
});

/// Save followed by an awaited deployment; renders like the finished operation.
#[derive(Serialize)]
pub(crate) struct SavedDeploymentReport<'a> {
    pub(crate) saved: &'a SavedApplication,
    pub(crate) outcome: OperationState,
    pub(crate) operation: &'a Operation,
}
report!(SavedDeploymentReport<'_>, self, out, {
    self.operation.render_human(out)
});

/// Accepted reconcile/deploy followed by its awaited outcome; renders like the operation.
#[derive(Serialize)]
pub(crate) struct OperationOutcomeReport<'a> {
    pub(crate) accepted: &'a AcceptedOperation,
    pub(crate) outcome: OperationState,
    pub(crate) operation: &'a Operation,
}
report!(OperationOutcomeReport<'_>, self, out, {
    self.operation.render_human(out)
});

/// Environment deletion result. `outcome` is present only after waiting; volumes
/// are always retained.
#[derive(Serialize)]
pub(crate) struct DeletionReport<'a> {
    accepted: &'a AcceptedOperation,
    /// `Some("deleted")` once the environment is gone; omitted with `--no-wait`.
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<&'static str>,
    volumes_retained: bool,
}
impl<'a> DeletionReport<'a> {
    /// Deletion accepted but not awaited.
    pub(crate) fn accepted(accepted: &'a AcceptedOperation) -> Self {
        Self {
            accepted,
            outcome: None,
            volumes_retained: true,
        }
    }
    /// Deletion awaited until the environment disappeared.
    pub(crate) fn completed(accepted: &'a AcceptedOperation) -> Self {
        Self {
            accepted,
            outcome: Some("deleted"),
            volumes_retained: true,
        }
    }
}
report!(DeletionReport<'_>, self, out, {
    if self.outcome.is_some() {
        out.line(format_args!(
            "Environment {} deleted (named volumes retained)",
            self.accepted.environment_id
        ))
    } else {
        out.line(format_args!(
            "Accepted operation {} (named volumes retained)",
            self.accepted.operation_id
        ))
    }
});

/// Application deletion result. `outcome` is present only after waiting;
/// volumes are always retained.
#[derive(Serialize)]
pub(crate) struct ApplicationDeletionReport<'a> {
    deleted: &'a DeletedApplication,
    /// `Some("deleted")` once the application is gone; omitted with `--no-wait`.
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<&'static str>,
    volumes_retained: bool,
}
impl<'a> ApplicationDeletionReport<'a> {
    /// Deletion accepted but not awaited.
    pub(crate) fn accepted(deleted: &'a DeletedApplication) -> Self {
        Self {
            deleted,
            outcome: None,
            volumes_retained: true,
        }
    }
    /// Deletion awaited until the application disappeared.
    pub(crate) fn completed(deleted: &'a DeletedApplication) -> Self {
        Self {
            deleted,
            outcome: Some("deleted"),
            volumes_retained: true,
        }
    }
}
report!(ApplicationDeletionReport<'_>, self, out, {
    if self.outcome.is_some() {
        return out.line(format_args!(
            "Application {} deleted (named volumes retained)",
            self.deleted.application_id
        ));
    }
    for operation in &self.deleted.operations {
        out.line(format_args!(
            "Accepted operation {} for environment {} (named volumes retained)",
            operation.operation_id, operation.environment_id
        ))?;
    }
    Ok(())
});

/// Local `app validate` success; JSON carries the manifest's application name.
#[derive(Serialize)]
pub(crate) struct ValidManifestReport {
    pub(crate) application: String,
}
report!(ValidManifestReport, self, out, {
    out.line(format_args!(
        "Manifest is valid for application {:?}.",
        self.application
    ))
});

/// Saved TOML in human mode; a JSON string in machine mode.
pub(crate) struct ManifestReport(pub(crate) String);
impl Report for ManifestReport {
    type Json = str;
    fn json(&self) -> &str {
        &self.0
    }
    fn render_human(&self, out: &mut HumanWriter<'_>) -> io::Result<()> {
        out.line(self.0.trim_end())
    }
}

// Plan rendering: manifest changes, an action summary with per-action risk and
// reason, then diagnostics.
report!(PlanView, self, out, {
    out.label("Application", &self.application_id)?;
    if self.identical {
        out.blank()?;
        out.line(if self.operation.is_some() {
            "Configuration matches the latest deployment snapshot."
        } else {
            "Configuration matches the saved configuration."
        })?;
    }
    if !self.changes.is_empty() {
        out.blank()?;
        out.heading("Changes:")?;
        for change in &self.changes {
            let (marker, value) = match (&change.before, &change.after) {
                (None, Some(after)) => ('+', Configuration::value(after)),
                (Some(before), None) => ('-', Configuration::value(before)),
                (Some(before), Some(after)) => (
                    '~',
                    format!(
                        "{} → {}",
                        Configuration::value(before),
                        Configuration::value(after)
                    ),
                ),
                (None, None) => ('~', "absent".into()),
            };
            out.change(marker, &change.field, &value)?;
        }
    }
    let summary = self.plan.summary();
    out.blank()?;
    out.label(
        "Actions",
        format_args!(
            "{} total · {} runtime mutations · {} destructive · {} blocking",
            summary.action_count,
            summary.mutation_count,
            summary.destructive_count,
            summary.blocking_conflicts
        ),
    )?;
    for (index, action) in self.plan.actions.iter().enumerate() {
        let mut description = action.human_description().to_ascii_lowercase();
        if let Some(first) = description.get_mut(..1) {
            first.make_ascii_uppercase();
        }
        out.line(format_args!("  {:>2}. {description}", index + 1))?;
        let risk = match action.kind.risk() {
            ActionRisk::None => "no risk",
            ActionRisk::Availability => "availability",
            ActionRisk::DataAdjacent => "data-adjacent",
            ActionRisk::Destructive => "destructive",
        };
        let reason = match &action.reason {
            ActionReason::Missing => "missing".into(),
            ActionReason::Drift { fields } if fields.is_empty() => "drift".into(),
            ActionReason::Drift { fields } => format!("drift ({})", fields.join(", ")),
            ActionReason::Obsolete => "obsolete".into(),
            ActionReason::ConvergencePending => "convergence pending".into(),
            ActionReason::ResolutionRequired => "resolution required".into(),
            ActionReason::ApplicationDeletion => "application deletion".into(),
            ActionReason::VolumeRetentionPolicy => "volume retention policy".into(),
        };
        out.line(format_args!("      {risk} · {reason}"))?;
    }
    Configuration::variables(out, &self.variables)?;
    if !self.rollouts.is_empty() {
        out.blank()?;
        out.heading("Rollout:")?;
        for rollout in &self.rollouts {
            out.line(format_args!(
                "  {}: {} ({}) · monitor {}s",
                rollout.service,
                rollout.order,
                rollout.order_source.as_str(),
                rollout.monitor_seconds
            ))?;
        }
    }
    for diagnostic in &self.plan.diagnostics {
        out.blank()?;
        out.label(
            "Diagnostic",
            format_args!("{} [{}]", diagnostic.code, diagnostic.resource),
        )?;
        out.line(format_args!(
            "  {}{}",
            diagnostic.message,
            if diagnostic.blocking {
                " (blocking)"
            } else {
                ""
            }
        ))?;
    }
    Ok(())
});

/// Human rendering of plan change values, which the daemon sends as JSON text.
struct Configuration;
impl Configuration {
    /// Lists the rendered value of each reference, when there are any.
    fn variables(
        out: &mut HumanWriter<'_>,
        variables: &std::collections::BTreeMap<String, piqueld_client::VariableValue>,
    ) -> io::Result<()> {
        if variables.is_empty() {
            return Ok(());
        }
        out.blank()?;
        out.heading("Variables:")?;
        for (reference, value) in variables {
            out.line(format_args!("  {reference} = {value}"))?;
        }
        Ok(())
    }

    /// Renders a change value, falling back to the raw text when it is not JSON.
    fn value(value: &str) -> String {
        serde_json::from_str(value).map_or_else(|_| value.to_owned(), |value| Self::render(&value))
    }
    /// Flattens JSON into compact prose: empty values become `none`, image sources
    /// are shortened, and object keys lose underscores.
    ///
    /// ```text
    /// null / [] / {}                             →  none
    /// {"type":"image","image":"nginx:1"}        →  image nginx:1
    /// {"read_only":true,"volume":"data"}         →  read only: true, volume: data
    /// ["a","b"]                                  →  a, b
    /// ```
    fn render(value: &serde_json::Value) -> String {
        use serde_json::Value;
        match value {
            Value::Null => "none".into(),
            Value::String(value) => value.clone(),
            Value::Array(values) if values.is_empty() => "none".into(),
            Value::Array(values) => values
                .iter()
                .map(Self::render)
                .collect::<Vec<_>>()
                .join(", "),
            Value::Object(values) => {
                if values.get("type").and_then(Value::as_str) == Some("image")
                    && let Some(image) = values.get("image").and_then(Value::as_str)
                {
                    return format!("image {image}");
                }
                if values.is_empty() {
                    return "none".into();
                }
                values
                    .iter()
                    .map(|(k, v)| format!("{}: {}", k.replace('_', " "), Self::render(v)))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
            value => value.to_string(),
        }
    }
}

report!(piqueld_client::SecretKeyRecovery, self, out, {
    out.line(format_args!(
        "Secret key recovered: {} values discarded across {} secrets in {} environments.",
        self.discarded_versions, self.affected_secrets, self.affected_environments,
    ))?;
    out.line(
        "Running services keep their Docker secrets. Supply replacement values, then deploy.",
    )?;
    out.line("The next stored value generates a new key; back it up with the database.")
});
