//! Typed command results. Human-only context never leaks into JSON schemas.
use super::{HumanWriter, Report};
use crate::{profiles::ProfileSummary, support::desired_replicas};
use piqueld_client::{
    AcceptedOperation, ActionReason, ActionRisk, ApplicationLogs, ApplicationStatusView,
    ApplicationSummary, ApplicationView, BuildLogPage, BuildRecord, Event, Operation,
    OperationState, Page, PlanView, SavedApplication, Source, SystemStatus,
};
use serde::Serialize;
use std::io;

// Identity JSON is the common case; keep its implementation in one place.
macro_rules! report {
    ($ty:ty, $this:ident, $out:ident, $body:block) => {
        impl Report for $ty {
            type Json = Self;
            fn json(&self) -> &Self { self }
            fn render_human(&$this, $out: &mut HumanWriter<'_>) -> io::Result<()> $body
        }
    };
}

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
        out.label("Transport", self.transport)
    }
}

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

#[derive(Serialize)]
pub(crate) struct ApplicationRow {
    pub(crate) application: ApplicationSummary,
    pub(crate) status: Option<ApplicationStatusView>,
}

report!(Page<ApplicationRow>, self, out, {
    if self.items.is_empty() {
        return out.line("No applications.");
    }
    out.heading("NAME  STATE  GENERATION  ID")?;
    for row in &self.items {
        out.line(format_args!(
            "{}  {}  {}  {}",
            row.application.name,
            row.status
                .as_ref()
                .map_or_else(|| "unavailable".to_owned(), |s| s.state.to_string()),
            row.application.generation,
            row.application.id
        ))?;
    }
    Ok(())
});

#[derive(Serialize)]
pub(crate) struct ShowReport<'a> {
    pub(crate) application: &'a ApplicationView,
    pub(crate) status: &'a ApplicationStatusView,
}
report!(ShowReport<'_>, self, out, {
    let app = self.application;
    out.line(format_args!(
        "{} ({})",
        app.application.metadata().name,
        app.application.id()
    ))?;
    out.blank()?;
    out.label("Intent", self.status.state)?;
    out.label(
        "Runtime",
        self.status.runtime_health.as_deref().unwrap_or("unknown"),
    )?;
    out.label("Configuration revision", app.generation)?;
    out.label(
        "Resolved revision",
        app.resolved_generation
            .map_or_else(|| "none".to_owned(), |v| v.to_string()),
    )?;
    out.label("Replicas", desired_replicas(app))?;
    for service in &app.application.spec().services {
        out.blank()?;
        out.label("Service", &service.name)?;
        out.label("  Replicas", service.replicas)?;
        match &service.source {
            Source::Image { image } => out.label("  Source", format_args!("image {image}"))?,
            Source::Git { repository, .. } => out.label(
                "  Source",
                format_args!("git {} ({})", repository.url, repository.branch),
            )?,
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
    Ok(())
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
        out.line(format_args!(
            "{}  {}  {}  {:?}  {}",
            build.id, build.application_id, build.service, build.state, build.started_at_ms
        ))?;
    }
    if let Some(cursor) = &self.next_cursor {
        out.label("Next cursor", cursor)?;
    }
    Ok(())
});

report!(Page<Event>, self, out, {
    for event in &self.items {
        out.line(format_args!(
            "{}  {}  {}  {}  attempt {}  {} {} {} {}",
            event.id,
            event.created_at_ms,
            event.kind,
            event.operation_id.as_deref().unwrap_or("-"),
            event.attempt.map_or_else(|| "-".into(), |v| v.to_string()),
            event.phase.as_deref().unwrap_or(""),
            event.resource.as_deref().unwrap_or(""),
            event.error_code.as_deref().unwrap_or(""),
            event.message.as_deref().unwrap_or("")
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
    out.label("  Application", &self.application_id)?;
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
        "Accepted operation {} for application {}",
        self.operation_id, self.application_id
    ))
});

#[derive(Serialize)]
pub(crate) struct SavedDeploymentReport<'a> {
    pub(crate) saved: &'a SavedApplication,
    pub(crate) outcome: OperationState,
    pub(crate) operation: &'a Operation,
}
report!(SavedDeploymentReport<'_>, self, out, {
    self.operation.render_human(out)
});

#[derive(Serialize)]
pub(crate) struct OperationOutcomeReport<'a> {
    pub(crate) accepted: &'a AcceptedOperation,
    pub(crate) outcome: OperationState,
    pub(crate) operation: &'a Operation,
}
report!(OperationOutcomeReport<'_>, self, out, {
    self.operation.render_human(out)
});

#[derive(Serialize)]
pub(crate) struct DeletionReport<'a> {
    accepted: &'a AcceptedOperation,
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<&'static str>,
    volumes_retained: bool,
}
impl<'a> DeletionReport<'a> {
    pub(crate) fn accepted(accepted: &'a AcceptedOperation) -> Self {
        Self {
            accepted,
            outcome: None,
            volumes_retained: true,
        }
    }
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
            "Application {} deleted (named volumes retained)",
            self.accepted.application_id
        ))
    } else {
        out.line(format_args!(
            "Accepted operation {} (named volumes retained)",
            self.accepted.operation_id
        ))
    }
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

report!(PlanView, self, out, {
    out.label("Application", &self.application_id)?;
    if self.identical {
        out.blank()?;
        return out.line("No changes. The manifest is already current.");
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

struct Configuration;
impl Configuration {
    fn value(value: &str) -> String {
        serde_json::from_str(value).map_or_else(|_| value.to_owned(), |value| Self::render(&value))
    }
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
