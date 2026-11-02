//! Reports about environments, previews, releases and promotions.

use super::super::HumanWriter;
use super::{Names, snapshot_origin, sync};
use crate::deployments::Deployed;
use piqueld_client::{
    AcceptedPromotion, ApplicationId, ApplicationView, BranchState, CreatedPreview, DeletedPreview,
    DeploymentOrigin, DeploymentView, EnvironmentDetailView, EnvironmentSource,
    EnvironmentStatusView, EnvironmentView, LocalSnapshot, MountedSecret, Operation,
    OperationState, Page, PreviewView, ReleaseId, ReleaseView, StoredSecret,
    TelemetryDestinationStatus, links::ImportState, sync::short_commit,
};
use serde::Serialize;
use std::io;

/// One environment with its status, as listed by `env list` and inside `app list`.
#[derive(Serialize)]
pub(crate) struct EnvironmentRow {
    pub(crate) environment: EnvironmentView,
    /// `None` when the status request failed (rendered as `unavailable`).
    pub(crate) status: Option<EnvironmentStatusView>,
}

impl EnvironmentRow {
    /// Current state, `stopped` while it is, or `unavailable` when the
    /// status could not be read.
    pub(super) fn state(&self) -> String {
        if self.environment.stopped() {
            return "stopped".to_owned();
        }
        self.status
            .as_ref()
            .map_or_else(|| "unavailable".to_owned(), |s| s.state.to_string())
    }
}

report!(Vec<EnvironmentRow> as [EnvironmentRow], self, out, {
    let environments = self
        .iter()
        .map(|row| row.environment.clone())
        .collect::<Vec<_>>();
    let names = Names(&environments);
    out.table(self, "No environments.", "NAME  STATE  SOURCE  RESOLVED  ID", |out, row| {
        let environment = &row.environment;
        out.line(format_args!(
            "{}  {}  {}  {}  {}",
            environment.name,
            row.state(),
            names.source(&environment.source),
            resolved_generation(environment),
            environment.id
        ))
    })
});

/// The configuration revision an environment last resolved, or `none`.
fn resolved_generation(environment: &EnvironmentView) -> String {
    environment
        .resolved_generation
        .map_or_else(|| "none".to_owned(), |generation| generation.to_string())
}

/// A preview's branch and slot, e.g. `feat/login` or `feat/login (slot agent-2)`.
fn preview_branch(preview: &EnvironmentView) -> String {
    match preview.preview() {
        Some(identity) => match &identity.slot {
            Some(slot) => format!("{} (slot {slot})", identity.branch),
            None => identity.branch.to_string(),
        },
        None => preview.name.to_string(),
    }
}

/// An environment's observed runtime health and how long ago it was
/// observed, which grows when health scans stop.
///
/// ```text
/// ready (observed 12 s ago)
/// unknown
/// ```
fn runtime_health(status: &EnvironmentStatusView) -> String {
    let health = status.runtime_health.as_deref().unwrap_or("unknown");
    let Some(observed) = status.health_observed_at_ms else {
        return health.into();
    };
    let seconds = crate::support::now_ms().saturating_sub(observed).max(0) / 1000;
    let age = match seconds {
        0..120 => format!("{seconds} s"),
        120..7_200 => format!("{} min", seconds / 60),
        _ => format!("{} h", seconds / 3_600),
    };
    format!("{health} (observed {age} ago)")
}

/// A branch state in words, with the commits involved.
fn branch_state(state: &BranchState) -> String {
    match state {
        BranchState::Exists { head } => format!("exists at {head}"),
        BranchState::Moved { head, deployed } => {
            format!("moved to {head} (deployed {deployed})")
        }
        BranchState::Gone => "gone".into(),
        BranchState::Local { head: Some(head) } => {
            format!("local snapshot deployed; the branch is at {head}")
        }
        BranchState::Local { head: None } => {
            "local snapshot deployed; the branch was never pushed".into()
        }
        BranchState::Unknown { message } => format!("unknown: {message}"),
    }
}

// `preview list` result.
report!(Vec<PreviewView> as [PreviewView], self, out, {
    out.table(
        self,
        "No previews.",
        "BRANCH  SLOT  SLUG  STATE  BRANCH STATE  SYNC  SYNCED  LAST OPERATION  ID",
        |out, view| {
            let preview = &view.preview;
            let slot = preview
                .preview()
                .and_then(|identity| identity.slot.as_ref())
                .map_or("-", |slot| slot.as_str());
            out.line(format_args!(
                "{}  {slot}  {}  {}  {}  {}  {}  {}  {}",
                preview
                    .preview()
                    .map_or_else(|| preview.name.to_string(), |p| p.branch.to_string()),
                preview.name,
                view.status.state,
                view.branch,
                view.sync,
                preview
                    .synced
                    .as_ref()
                    .and_then(|synced| synced.commit.as_deref())
                    .map_or("-", short_commit),
                view.latest_operation
                    .as_ref()
                    .map_or_else(|| "none".to_owned(), |op| format!("{} {}", op.state, op.id)),
                preview.id
            ))
        },
    )
});

report!(PreviewView, self, out, {
    let preview = &self.preview;
    out.line(format_args!(
        "Preview {} of branch {} ({})",
        preview.name,
        preview_branch(preview),
        preview.id
    ))?;
    out.blank()?;
    out.label("Intent", self.status.state)?;
    out.label("Runtime", runtime_health(&self.status))?;
    out.label("Branch", branch_state(&self.branch))?;
    if let DeploymentOrigin::Local(snapshot) = &self.origin {
        out.label("Deploys", snapshot_origin(snapshot))?;
    }
    out.label("Sync", sync(self.sync, preview))?;
    if let Some(operation) = &self.latest_operation {
        out.label(
            "Last operation",
            format_args!("{} {} ({})", operation.kind, operation.state, operation.id),
        )?;
    }
    for hostname in &self.hostnames {
        out.label("URL", format_args!("https://{hostname}"))?;
    }
    if let Some(message) = &self.status.message {
        out.label("Message", message)?;
    }
    for bound in &self.bounds {
        out.label("Bounded", &bound.message)?;
    }
    Ok(())
});

/// `preview create` result: the preview, its deployment, and, after waiting,
/// how the deployment ended and its URLs.
#[derive(Serialize)]
pub(crate) struct CreatedPreviewReport<'a> {
    #[serde(flatten)]
    pub(crate) created: &'a CreatedPreview,
    #[serde(flatten)]
    pub(crate) deployed: Deployed,
}
report!(CreatedPreviewReport<'_>, self, out, {
    let CreatedPreview {
        preview,
        operation,
        created,
    } = self.created;
    out.line(format_args!(
        "Preview {} of branch {} ({}) {}",
        preview.name,
        preview_branch(preview),
        preview.id,
        match (*created, self.deployed.local) {
            (true, _) => "created",
            (false, true) => "exists",
            (false, false) => "already exists; not redeployed",
        }
    ))?;
    if let (true, Some(commit)) = (self.deployed.local, &self.deployed.commit) {
        let snapshot = LocalSnapshot {
            commit: commit.clone(),
            base: self.deployed.base.clone(),
        };
        out.label("Deploys", snapshot_origin(&snapshot))?;
    }
    match self.deployed.outcome {
        Some(outcome) => out.label(
            "Operation",
            format_args!("{} {outcome}", operation.operation_id),
        )?,
        None => out.label("Operation", &operation.operation_id)?,
    }
    out.urls(self.deployed.urls.iter().flatten())
});

// `preview prune` result: the previews whose deletion was accepted, with
// their operations, since `--no-wait` reports before they finish.
report!(Vec<DeletedPreview> as [DeletedPreview], self, out, {
    if self.is_empty() {
        return out.line("No preview deleted.");
    }
    for deleted in self {
        out.line(format_args!(
            "Deleting preview {} of branch {} ({}) with its volumes: operation {}",
            deleted.preview.name,
            preview_branch(&deleted.preview),
            deleted.preview.id,
            deleted.operation.operation_id
        ))?;
    }
    Ok(())
});

/// `env show` result: an environment's intent, the manifest it deploys, and
/// its runtime state.
#[derive(Serialize)]
pub(crate) struct EnvironmentShowReport<'a> {
    #[serde(flatten)]
    pub(crate) detail: &'a EnvironmentDetailView,
    /// The application's stored secrets, to show where mounted secrets come from.
    pub(crate) stored: &'a [StoredSecret],
}
report!(EnvironmentShowReport<'_>, self, out, {
    let EnvironmentDetailView {
        environment,
        application,
        manifest,
        status,
        ..
    } = self.detail;
    out.line(format_args!(
        "{} of {} ({})",
        environment.name,
        application.application.metadata().name,
        environment.id
    ))?;
    out.blank()?;
    out.label("Intent", status.state)?;
    if environment.stopped() {
        out.label("Stopped", "yes; `env start` or a deployment runs it again")?;
    }
    out.label("Runtime", runtime_health(status))?;
    out.label(
        "Source",
        Names(&application.environments).source(&environment.source),
    )?;
    let connection = application.application.spec().manifest.as_ref();
    out.label(
        "Sync",
        sync(environment.sync_state(connection), environment),
    )?;
    if let (EnvironmentSource::Branch(_), None) = (&environment.source, manifest) {
        out.label("Manifest", "not fetched yet; deploy to fetch it")?;
    }
    out.label(
        "Release",
        self.detail
            .release
            .as_ref()
            .map_or("none", ReleaseId::as_str),
    )?;
    out.label("Configuration revision", application.generation)?;
    out.label("Resolved revision", resolved_generation(environment))?;
    let values = manifest
        .iter()
        .flat_map(|manifest| manifest.values(&environment.target()));
    for (name, value) in values {
        match value {
            Some(value) => out.label("Variable", format_args!("{name} = {value}"))?,
            None => out.label("Variable", format_args!("{name} has no value"))?,
        }
    }
    let secrets = manifest
        .iter()
        .flat_map(|manifest| MountedSecret::list(manifest, environment, self.stored));
    for (name, mounted) in secrets {
        out.label("Secret", format_args!("{name}: {mounted}"))?;
    }
    if let Some(telemetry) = &self.detail.telemetry {
        match (&telemetry.receiver, telemetry.granted) {
            (Some(service), true) => out.label(
                "Telemetry",
                format_args!(
                    "granted; {service} receives it: {}",
                    telemetry
                        .destination
                        .as_ref()
                        .map_or("not connected yet", TelemetryDestinationStatus::state)
                ),
            )?,
            (Some(service), false) => out.label(
                "Telemetry",
                format_args!("not granted; {service} runs without the telemetry socket"),
            )?,
            (None, true) => out.label("Telemetry", "granted; no deployed service receives it")?,
            (None, false) => {}
        }
    }
    for import in self.detail.links.iter().flat_map(|links| &links.imports) {
        let state = match import.state {
            ImportState::Connected => "connected",
            ImportState::Unavailable => "waiting: not exported to this environment",
            ImportState::Full => "waiting: the exporter links to too many environments",
        };
        out.label(
            "Import",
            format_args!(
                "{} from {} {}: {state}",
                import.alias, import.application, import.environment
            ),
        )?;
    }
    for importer in self.detail.links.iter().flat_map(|links| &links.importers) {
        out.label(
            "Imported by",
            format_args!(
                "{} {}: {}",
                importer.application, importer.environment, importer.service
            ),
        )?;
    }
    out.urls(self.detail.urls.iter().flatten())
});

named!(EnvironmentView, environment, names, out, {
    out.line(format_args!(
        "Environment {} ({}), deploys from {}{}{}{}.",
        environment.name,
        environment.id,
        names.describe(&environment.source),
        if environment.sync {
            ", opted into sync"
        } else {
            ""
        },
        if environment.telemetry {
            ", granted telemetry"
        } else {
            ""
        },
        if environment.stopped() {
            ", stopped"
        } else {
            ""
        }
    ))
});

// Each release with where its manifest came from and each service's image.
named!(Page<ReleaseView>, page, names, out, {
    if page.items.is_empty() {
        out.line("No releases.")?;
    }
    for release in &page.items {
        names.release(out, release)?;
    }
    out.next_cursor(page.next_cursor.as_deref())
});

// Each deployment, newest first, with its outcome, origin and release.
named!(Page<DeploymentView>, page, names, out, {
    if page.items.is_empty() {
        out.line("No deployments.")?;
    }
    for deployment in &page.items {
        let marks = [
            (deployment.current_target, "current"),
            (deployment.last_successful, "last successful"),
        ]
        .into_iter()
        .filter_map(|(marked, mark)| marked.then_some(mark))
        .collect::<Vec<_>>();
        let operation = &deployment.operation;
        out.line(format_args!(
            "{}  {}  created {}{}",
            operation.id,
            operation.state,
            operation.created_at_ms,
            if marks.is_empty() {
                String::new()
            } else {
                format!("  ({})", marks.join(", "))
            }
        ))?;
        out.label("  Origin", names.origin(&deployment.origin))?;
        out.label(
            "  Release",
            deployment
                .release
                .as_ref()
                .map_or_else(|| "none".to_owned(), ToString::to_string),
        )?;
    }
    out.next_cursor(page.next_cursor.as_deref())
});

/// `env promote` result: the deployment, the release it pinned and where it
/// came from, and, once waited for, its outcome.
#[derive(Serialize)]
pub(crate) struct PromotionReport<'a> {
    application_id: &'a ApplicationId,
    environment_id: &'a str,
    /// Also the operation's ID.
    deployment_id: &'a str,
    operation_id: &'a str,
    release_id: &'a ReleaseId,
    origin: &'a DeploymentOrigin,
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<OperationState>,
    #[serde(skip_serializing_if = "Option::is_none")]
    operation: Option<&'a Operation>,
    #[serde(skip)]
    names: Names<'a>,
}

impl<'a> PromotionReport<'a> {
    /// An accepted promotion, not waited for.
    pub(crate) fn new(application: &'a ApplicationView, accepted: &'a AcceptedPromotion) -> Self {
        Self {
            application_id: application.application.id(),
            environment_id: &accepted.operation.environment_id,
            deployment_id: &accepted.operation.operation_id,
            operation_id: &accepted.operation.operation_id,
            release_id: &accepted.release_id,
            origin: &accepted.origin,
            outcome: None,
            operation: None,
            names: Names(&application.environments),
        }
    }

    /// The promotion once its deployment finished.
    pub(crate) const fn finished(mut self, operation: &'a Operation) -> Self {
        self.outcome = Some(operation.state);
        self.operation = Some(operation);
        self
    }
}

report!(PromotionReport<'_>, self, out, {
    out.line(format_args!(
        "Promoting release {} into environment {}: {}, deployment {}.",
        self.release_id,
        self.names.environment(self.environment_id),
        self.names.origin(self.origin),
        self.deployment_id
    ))?;
    match self.operation {
        Some(operation) => operation.render_human(out),
        None => Ok(()),
    }
});
