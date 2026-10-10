//! Typed command results. Human-only context never leaks into JSON schemas.
use super::{HumanWriter, Report};
use crate::profiles::ProfileSummary;
use piqueld_client::{
    AcceptedOperation, ActionReason, ActionRisk, ApplicationLogs, ApplicationSummary,
    ApplicationView, BranchState, BuildLogPage, BuildRecord, CreatedPreview, DeletedApplication,
    DeletedPreview, DnsStatus, EnvironmentDetailView, EnvironmentSource, EnvironmentStatusView,
    EnvironmentView, Event, ImageStatus, MountedSecret, Operation, OperationState, Page, PlanView,
    PreviewUsage, PreviewView, ReleaseAvailability, ReleaseView, ResolvedSource, SavedApplication,
    SecretMetadata, ServiceImage, Source, StoredSecret, SystemStatus,
    sync::{SyncState, WebhookSecret, WebhookView},
    system::{IngressStatus, PublicIngressStatus, RouteStatus},
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

/// Daemon status; JSON is the daemon's status, human output adds the transport
/// used and, when readiness answered, each ingress listener.
pub(crate) struct StatusReport<'a> {
    pub(crate) status: &'a SystemStatus,
    pub(crate) transport: &'a str,
    pub(crate) ingress: Option<&'a IngressStatus>,
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
        s.images.render_human(out)?;
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
        if let Some(ingress) = self.ingress.filter(|ingress| ingress.enabled) {
            out.label(
                "Public listener",
                format_args!(
                    "{}: {}",
                    if ingress.healthy {
                        "healthy"
                    } else {
                        "unhealthy"
                    },
                    ingress.message
                ),
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
                        if private.addresses.is_empty() {
                            "no tailnet addresses".into()
                        } else {
                            private.addresses.join(", ")
                        },
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
        s.dns.render_human(out)
    }
}

// The built images cleanup kept, and what it removed since the daemon started.
report!(ImageStatus, self, out, {
    match self.cleaned_at_ms {
        Some(at) => out.label(
            "Built images",
            format_args!(
                "{} kept; cleanup removed {} bytes of images since the daemon started (last run at Unix ms {at})",
                self.images, self.reclaimed_bytes
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

/// One deployed route of `route list`, with its environment's name.
#[derive(Serialize)]
pub(crate) struct RouteRow {
    pub(crate) environment: String,
    #[serde(flatten)]
    pub(crate) route: RouteStatus,
}

impl Report for Vec<RouteRow> {
    type Json = [RouteRow];
    fn json(&self) -> &Self::Json {
        self
    }
    fn render_human(&self, out: &mut HumanWriter<'_>) -> io::Result<()> {
        if self.is_empty() {
            return out.line("No deployed routes.");
        }
        out.heading("HOSTNAME  ENVIRONMENT  VISIBILITY  STATE  DESTINATION  DNS  RECORDS")?;
        for row in self {
            let route = &row.route;
            out.line(format_args!(
                "{}  {}  {}  {}  {}  {}  {}",
                route.hostname,
                row.environment,
                route.visibility,
                route.state,
                route.target,
                route.dns_state,
                route.dns
            ))?;
            if route.state != "ready" {
                out.line(format_args!("  {}", route.message))?;
            }
        }
        Ok(())
    }
}

// DNS providers with their zones and health, then DNS-01 certificates; shown
// by `status` and `dns refresh`.
report!(DnsStatus, self, out, {
    for provider in &self.providers {
        out.label(
            "DNS provider",
            format_args!(
                "{} ({}{}): {}",
                provider.kind,
                if provider.healthy {
                    "healthy"
                } else {
                    "unhealthy"
                },
                if provider.manage_records {
                    ", manages route records"
                } else {
                    ""
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
    if let Some(connection) = &app.application.spec().manifest {
        out.label("Sync", connection.sync)?;
        if let Some(check) = &app.sync_check {
            match &check.error {
                Some(error) => out.label("Last sync check", format_args!("failed: {error}"))?,
                None => out.label("Last sync check", "succeeded")?,
            }
        }
    }
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
    if !spec.variables.is_empty()
        || !spec.environments.is_empty()
        || !spec.previews.variables.is_empty()
    {
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
        for (name, value) in &spec.previews.variables {
            out.label("Variable", format_args!("{name} = {value} (in previews)"))?;
        }
    }
    Ok(())
});

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

/// Whether pushes deploy `environment`, and the branch head as of its last
/// deployment.
///
/// ```text
/// following, last synced 0123456789abcdef0123456789abcdef01234567
/// off (not opted in)
/// ```
fn sync(state: SyncState, environment: &EnvironmentView) -> String {
    match (state, &environment.synced) {
        (SyncState::Following, Some(synced)) => {
            format!("following, last synced {}", synced.commit)
        }
        (SyncState::Following, None) => "following".into(),
        (SyncState::Off, _) => "off".into(),
        (state, _) => format!("off ({state})"),
    }
}

/// A branch state in words, with the commits involved.
fn branch_state(state: &BranchState) -> String {
    match state {
        BranchState::Exists { head } => format!("exists at {head}"),
        BranchState::Moved { head, deployed } => {
            format!("moved to {head} (deployed {deployed})")
        }
        BranchState::Gone => "gone".into(),
        BranchState::Unknown { message } => format!("unknown: {message}"),
    }
}

/// `preview list` result.
impl Report for Vec<PreviewView> {
    type Json = [PreviewView];
    fn json(&self) -> &Self::Json {
        self
    }
    fn render_human(&self, out: &mut HumanWriter<'_>) -> io::Result<()> {
        if self.is_empty() {
            return out.line("No previews.");
        }
        out.heading("BRANCH  SLOT  SLUG  STATE  BRANCH STATE  SYNC  SYNCED  LAST OPERATION  ID")?;
        for view in self {
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
                    .map_or("-", |synced| &synced.commit[..synced.commit.len().min(12)]),
                view.latest_operation
                    .as_ref()
                    .map_or_else(|| "none".to_owned(), |op| format!("{} {}", op.state, op.id)),
                preview.id
            ))?;
        }
        Ok(())
    }
}

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
    out.label(
        "Runtime",
        self.status.runtime_health.as_deref().unwrap_or("unknown"),
    )?;
    out.label("Branch", branch_state(&self.branch))?;
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
/// how the deployment ended.
#[derive(Serialize)]
pub(crate) struct CreatedPreviewReport<'a> {
    #[serde(flatten)]
    pub(crate) created: &'a CreatedPreview,
    /// The deployment's final state; omitted with `--no-wait`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) outcome: Option<OperationState>,
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
        if *created {
            "created"
        } else {
            "already exists; not redeployed"
        }
    ))?;
    match self.outcome {
        Some(outcome) => out.label(
            "Operation",
            format_args!("{} {outcome}", operation.operation_id),
        ),
        None => out.label("Operation", &operation.operation_id),
    }
});

/// `preview prune` result: the previews whose deletion was accepted, with
/// their operations, since `--no-wait` reports before they finish.
impl Report for Vec<DeletedPreview> {
    type Json = [DeletedPreview];
    fn json(&self) -> &Self::Json {
        self
    }
    fn render_human(&self, out: &mut HumanWriter<'_>) -> io::Result<()> {
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
    }
}

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
    out.label(
        "Runtime",
        status.runtime_health.as_deref().unwrap_or("unknown"),
    )?;
    out.label("Source", source(&environment.source))?;
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
            .map_or("none", piqueld_client::ReleaseId::as_str),
    )?;
    out.label("Configuration revision", application.generation)?;
    out.label(
        "Resolved revision",
        environment
            .resolved_generation
            .map_or_else(|| "none".to_owned(), |v| v.to_string()),
    )?;
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
    Ok(())
});

report!(EnvironmentView, self, out, {
    out.line(format_args!(
        "Environment {} ({}), deploys from {}{}.",
        self.name,
        self.id,
        self.source,
        if self.sync { ", opted into sync" } else { "" }
    ))
});

// `app repository webhook show` result.
report!(WebhookView, self, out, {
    match &self.url {
        Some(url) => out.label("Payload URL", url)?,
        None => out.label(
            "Payload URL",
            "not served; set ingress.webhook_hostname on the daemon",
        )?,
    }
    out.label("Content type", "application/json")?;
    out.label("Events", "push")?;
    match self.secret_created_at_ms {
        Some(_) => out.label("Secret", "generated; rotate it to see a new one")?,
        None => out.label(
            "Secret",
            "none; generate one with `app repository webhook rotate`",
        )?,
    }
    Ok(())
});

// `app repository webhook rotate` result: the only time the secret is shown.
report!(WebhookSecret, self, out, {
    if let Some(url) = &self.url {
        out.label("Payload URL", url)?;
    }
    out.label("Secret", &self.secret)?;
    out.line("Configure this secret in GitHub now; piqueld never shows it again.")
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

// Each release with where its manifest came from and each service's image.
report!(Page<ReleaseView>, self, out, {
    if self.items.is_empty() {
        out.line("No releases.")?;
    }
    for release in &self.items {
        out.line(format_args!(
            "{}  {}  created {}",
            release.id,
            release.release.commit().map_or_else(
                || "saved manifest".to_owned(),
                |commit| format!("commit {commit}")
            ),
            release.created_at_ms
        ))?;
        if let Some(availability) = &release.availability {
            let services = |missing: &[ServiceImage]| {
                missing
                    .iter()
                    .map(|image| image.service.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            out.label(
                "  Images",
                match availability {
                    ReleaseAvailability::Present => "all present".into(),
                    ReleaseAvailability::Pullable { missing } => format!(
                        "missing for {}; deploying it pulls them again by digest",
                        services(missing)
                    ),
                    ReleaseAvailability::Unavailable { missing } => format!(
                        "missing for {}; it can't be deployed again",
                        services(missing)
                    ),
                },
            )?;
        }
        for (service, source) in release.release.sources() {
            out.label(
                &format!("  {service}"),
                match source {
                    ResolvedSource::Image {
                        requested,
                        digest_reference,
                    } => format!("{digest_reference} (pulled {requested})"),
                    ResolvedSource::Git {
                        requested,
                        commit,
                        image_id,
                        ..
                    } => format!(
                        "{image_id} (built from {} at {commit})",
                        repository(requested)
                    ),
                },
            )?;
        }
    }
    if let Some(cursor) = &self.next_cursor {
        out.label("Next cursor", cursor)?;
    }
    Ok(())
});

/// The repository a Git source builds from.
fn repository(source: &piqueld_client::ValidatedSource) -> String {
    match source {
        piqueld_client::ValidatedSource::Git { repository, .. } => repository.to_string(),
        piqueld_client::ValidatedSource::Image { image } => image.clone(),
    }
}

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
                    "  value unavailable; the next deploy generates a new one"
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

/// Stored secrets with their access, naming environments.
pub(crate) struct StoredSecretsReport<'a> {
    pub(crate) secrets: &'a [StoredSecret],
    pub(crate) environments: &'a [EnvironmentView],
}
impl Report for StoredSecretsReport<'_> {
    type Json = [StoredSecret];
    fn json(&self) -> &Self::Json {
        self.secrets
    }
    fn render_human(&self, out: &mut HumanWriter<'_>) -> io::Result<()> {
        for secret in self.secrets {
            let metadata = &secret.metadata;
            out.line(format_args!(
                "{}  generation {}  {}{}{}",
                metadata.name,
                metadata.generation,
                secret.access.describe(self.environments),
                if metadata.unavailable {
                    "  value unavailable; replace value and deploy"
                } else {
                    ""
                },
                if metadata.deleting {
                    "  deletion pending; retry delete"
                } else {
                    ""
                }
            ))?;
        }
        Ok(())
    }
}

/// One stored secret after a change, naming the environments that may mount it.
pub(crate) struct StoredSecretReport<'a> {
    pub(crate) secret: &'a StoredSecret,
    pub(crate) environments: &'a [EnvironmentView],
}
impl Report for StoredSecretReport<'_> {
    type Json = StoredSecret;
    fn json(&self) -> &Self::Json {
        self.secret
    }
    fn render_human(&self, out: &mut HumanWriter<'_>) -> io::Result<()> {
        out.line(format_args!(
            "Saved {} generation {}, mountable by {}. Deploy to use it.",
            self.secret.metadata.name,
            self.secret.metadata.generation,
            self.secret.access.describe(self.environments)
        ))
    }
}

report!(SecretMetadata, self, out, {
    out.line(format_args!(
        "Generated {} generation {}. Deploy the environment to use it.",
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
                .actor
                .as_ref()
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

/// Environment or preview deletion result. `outcome` is present only after
/// waiting. Environments retain their volumes; previews remove theirs.
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

    /// The deletion of a preview, which removes its volumes too.
    pub(crate) const fn removing_volumes(mut self) -> Self {
        self.volumes_retained = false;
        self
    }
}
report!(DeletionReport<'_>, self, out, {
    let (noun, volumes) = match (self.volumes_retained, self.outcome) {
        (true, _) => ("Environment", "named volumes retained"),
        (false, Some(_)) => ("Preview", "its volumes removed"),
        (false, None) => ("Preview", "its volumes will be removed"),
    };
    if self.outcome.is_some() {
        out.line(format_args!(
            "{noun} {} deleted ({volumes})",
            self.accepted.environment_id
        ))
    } else {
        out.line(format_args!(
            "Accepted operation {} ({volumes})",
            self.accepted.operation_id
        ))
    }
});

/// Application deletion result. `outcome` is present only after waiting;
/// environments retain their volumes, previews remove theirs.
#[derive(Serialize)]
pub(crate) struct ApplicationDeletionReport<'a> {
    deleted: &'a DeletedApplication,
    /// `Some("deleted")` once the application is gone; omitted with `--no-wait`.
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<&'static str>,
    /// Always true: environments' named volumes are retained. Previews' volumes
    /// are removed regardless.
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
            "Application {} deleted (environments' named volumes retained)",
            self.deleted.application_id
        ));
    }
    for operation in &self.deleted.operations {
        out.line(format_args!(
            "Accepted operation {} for {} (environments' named volumes retained)",
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
        "Secret key recovered: {} values discarded across {} secrets in {} environments and {} applications.",
        self.discarded_versions,
        self.affected_secrets,
        self.affected_environments,
        self.affected_applications,
    ))?;
    out.line(
        "Running services keep their Docker secrets. Supply replacement values, then deploy.",
    )?;
    out.line("The next stored value generates a new key; back it up with the database.")
});
