//! Typed command results. Human-only context never leaks into JSON schemas.
use super::{Escaped, HumanWriter};
use crate::support::join_or;
use piqueld_client::{
    ActionReason, ActionRisk, ApplicationView, DeploymentOrigin, EnvironmentChange,
    EnvironmentSource, EnvironmentView, LocalSnapshot, ManifestChange, PlanView,
    ReleaseAvailability, ReleasePlan, ReleaseView, ResolvedSource, RouteUrl, SecretProblem,
    ServiceImage, UrlState, VariableValue,
    observability::Timestamp,
    sync::{SyncState, short_commit},
};
use std::{collections::BTreeMap, io};
use unicode_width::UnicodeWidthStr;

// Implements `Report` with the block as `render_human`. The JSON is the value
// itself, what it dereferences to, or one of its fields:
//
// report!(Type, self, out, { ... })                    // Json = Type
// report!(Vec<Row> as [Row], self, out, { ... })       // Json = [Row]
// report!(Wrapper as Inner = field, self, out, { ... }) // Json = Inner, from `self.field`
// report!(once Type, self, out, { ... })               // a one-time secret `--quiet` shows
//
// rustfmt skips the `as` forms, so their bodies are formatted by hand.
macro_rules! report {
    (@impl $ty:ty, $json:ty, |$this:ident| $value:expr, $out:ident, $body:block $(, $once:literal)?) => {
        impl $crate::output::Report for $ty {
            type Json = $json;
            $(const ONE_TIME: bool = $once;)?
            fn json(&$this) -> &$json {
                $value
            }
            fn render_human(
                &$this,
                $out: &mut $crate::output::HumanWriter<'_>,
            ) -> std::io::Result<()> $body
        }
    };
    ($ty:ty as $json:ty = $field:tt, $this:ident, $out:ident, $body:block) => {
        $crate::output::reports::report!(@impl $ty, $json, |$this| &$this.$field, $out, $body);
    };
    ($ty:ty as $json:ty, $this:ident, $out:ident, $body:block) => {
        $crate::output::reports::report!(@impl $ty, $json, |$this| $this, $out, $body);
    };
    ($ty:ty, $this:ident, $out:ident, $body:block) => {
        $crate::output::reports::report!(@impl $ty, Self, |$this| $this, $out, $body);
    };
    (once $ty:ty, $this:ident, $out:ident, $body:block) => {
        $crate::output::reports::report!(@impl $ty, Self, |$this| $this, $out, $body, true);
    };
}
pub(crate) use report;

/// `healthy` or `unhealthy`.
const fn health(healthy: bool) -> &'static str {
    if healthy { "healthy" } else { "unhealthy" }
}

/// The application's environments, to name the ones sources and
/// deployment origins refer to by ID.
#[derive(Clone, Copy)]
pub(crate) struct Names<'a>(pub(crate) &'a [EnvironmentView]);

impl Names<'_> {
    /// Environment `id`'s name, or its ID once it is gone.
    fn environment(self, id: &str) -> String {
        self.0
            .iter()
            .find(|environment| environment.id.as_str() == id)
            .map_or_else(|| id.to_owned(), |environment| environment.name.to_string())
    }

    /// Where an environment deploys from: `saved`, the branch it follows,
    /// e.g. `main` or `main@<commit>`, or `promoted from <name>`.
    fn source(self, source: &EnvironmentSource) -> String {
        match source {
            EnvironmentSource::Saved => "saved".into(),
            EnvironmentSource::Branch(branch) => branch.to_string(),
            EnvironmentSource::Promoted(from) => {
                format!(
                    "promoted from {}",
                    self.environment(from.environment.as_str())
                )
            }
        }
    }

    /// Where an environment deploys from, in a sentence: the saved
    /// manifest, branch `main`, or releases promoted from `staging`.
    fn describe(self, source: &EnvironmentSource) -> String {
        match source {
            EnvironmentSource::Promoted(from) => format!(
                "releases promoted from {}",
                self.environment(from.environment.as_str())
            ),
            EnvironmentSource::Saved | EnvironmentSource::Branch(_) => source.to_string(),
        }
    }

    /// Where a deployment came from, in a phrase.
    fn origin(self, origin: &DeploymentOrigin) -> String {
        match origin {
            DeploymentOrigin::Build => "built from its source".into(),
            DeploymentOrigin::Promotion {
                environment,
                deployment,
            } => format!(
                "promoted from {}'s deployment {deployment}",
                self.environment(environment.as_str())
            ),
            DeploymentOrigin::Release => "deployed again by release ID".into(),
            DeploymentOrigin::Restore {
                deployment,
                rolled_back: None,
            } => format!("restored from deployment {deployment}"),
            DeploymentOrigin::Restore {
                deployment,
                rolled_back: Some(failed),
            } => format!("rolled back from deployment {failed} to deployment {deployment}"),
            DeploymentOrigin::Local(snapshot) => snapshot_origin(snapshot),
        }
    }

    /// One release: its commit, image availability, each service's image
    /// and provenance, and every deployment that received it.
    fn release(self, out: &mut HumanWriter<'_>, release: &ReleaseView) -> io::Result<()> {
        out.line(format_args!(
            "{}  {}  created {}",
            release.id,
            release.release.commit().map_or_else(
                || "saved manifest".to_owned(),
                |commit| format!("commit {commit}")
            ),
            Timestamp(release.created_at_ms)
        ))?;
        if let Some(availability) = &release.availability {
            let services =
                |missing: &[ServiceImage]| join_or(missing.iter().map(|image| &image.service), "");
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
        for promotion in &release.promotions {
            out.label(
                "  Deployed to",
                format_args!(
                    "{} as {}, {}",
                    self.environment(promotion.environment_id.as_str()),
                    promotion.deployment_id,
                    self.origin(&promotion.origin)
                ),
            )?;
        }
        Ok(())
    }

    /// A plan's release: the release, where it comes from, the volumes it
    /// creates empty, and every secret the environment can't use yet.
    fn release_plan(self, out: &mut HumanWriter<'_>, plan: &ReleasePlan) -> io::Result<()> {
        out.blank()?;
        out.heading("Release:")?;
        self.release(out, &plan.release)?;
        out.label("  Origin", self.origin(&plan.origin))?;
        for volume in &plan.new_volumes {
            out.label("  New volume", format_args!("{volume} (created empty)"))?;
        }
        for problem in &plan.secrets {
            let (secret, problem) = match problem {
                SecretProblem::Missing { secret } => {
                    (secret, "missing from the application's store")
                }
                SecretProblem::AccessDenied { secret } => {
                    (secret, "its access list excludes this environment")
                }
                SecretProblem::Unavailable { secret } => {
                    (secret, "key recovery discarded its value; store a new one")
                }
                SecretProblem::Deleting { secret } => (secret, "it is being deleted"),
            };
            out.label("  Secret", format_args!("{secret}: {problem}"))?;
        }
        Ok(())
    }

    /// Renders a plan: manifest changes, an action summary with per-action
    /// risk and reason, a release's, then diagnostics.
    fn plan(self, out: &mut HumanWriter<'_>, plan: &PlanView) -> io::Result<()> {
        out.label("Application", &plan.application_id)?;
        if plan.identical {
            out.blank()?;
            out.line(if plan.operation.is_some() {
                "Configuration matches the latest deployment snapshot."
            } else {
                "Configuration matches the saved configuration."
            })?;
        }
        if !plan.changes.is_empty() {
            out.blank()?;
            out.heading("Changes:")?;
            out.manifest_changes(&plan.changes)?;
        }
        if !plan.environments.is_empty() {
            out.blank()?;
            out.heading("Environments:")?;
            out.environment_changes(&plan.environments)?;
        }
        let summary = plan.plan.summary();
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
        if let Some(note) = plan.runtime_coverage.note() {
            out.line(format_args!("  {note}"))?;
        }
        for (index, action) in plan.plan.actions.iter().enumerate() {
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
                ActionReason::DeploymentJob => "deployment job".into(),
            };
            out.line(format_args!("      {risk} · {reason}"))?;
        }
        if let Some(release) = &plan.release {
            self.release_plan(out, release)?;
        }
        out.variables(&plan.variables)?;
        if !plan.rollouts.is_empty() {
            out.blank()?;
            out.heading("Rollout:")?;
            for rollout in &plan.rollouts {
                out.line(format_args!(
                    "  {}: {} ({}) · monitor {}s",
                    rollout.service,
                    rollout.order,
                    rollout.order_source.as_str(),
                    rollout.monitor_seconds
                ))?;
            }
        }
        for diagnostic in &plan.plan.diagnostics {
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
    }
}

/// A report whose human rendering names environments; its JSON is the
/// value's own.
pub(crate) struct Named<'a, T> {
    value: &'a T,
    names: Names<'a>,
}

impl<'a, T> Named<'a, T> {
    /// `value`, rendered naming the environments of `application`.
    pub(crate) fn new(value: &'a T, application: &'a ApplicationView) -> Self {
        Self {
            value,
            names: Names(&application.environments),
        }
    }
}

// Implements `Report` for `Named<T>`, rendering with `names` in scope.
macro_rules! named {
    ($ty:ty, $value:ident, $names:ident, $out:ident, $body:block) => {
        impl $crate::output::Report for $crate::output::reports::Named<'_, $ty> {
            type Json = $ty;
            fn json(&self) -> &$ty {
                self.value
            }
            fn render_human(&self, $out: &mut HumanWriter<'_>) -> io::Result<()> {
                let ($value, $names) = (self.value, self.names);
                $body
            }
        }
    };
}

/// Whether pushes deploy `environment`, and the branch head as of its last
/// deployment.
///
/// ```text
/// following, last synced 0123456789abcdef0123456789abcdef01234567
/// following, last synced before the branch was pushed
/// off (not opted in)
/// ```
fn sync(state: SyncState, environment: &EnvironmentView) -> String {
    match (state, &environment.synced) {
        (SyncState::Following, Some(synced)) => match &synced.commit {
            Some(commit) => format!("following, last synced {commit}"),
            None => "following, last synced before the branch was pushed".into(),
        },
        (SyncState::Following, None) => "following".into(),
        (SyncState::Off, _) => "off".into(),
        (state, _) => format!("off ({state})"),
    }
}

/// A local snapshot's provenance in a phrase.
///
/// ```text
/// local snapshot 0123456789ab on 89abcdef0123
/// local snapshot 0123456789ab, uploaded complete
/// ```
fn snapshot_origin(snapshot: &LocalSnapshot) -> String {
    let commit = short_commit(&snapshot.commit);
    match &snapshot.base {
        Some(base) => format!("local snapshot {commit} on {}", short_commit(base)),
        None => format!("local snapshot {commit}, uploaded complete"),
    }
}

/// One table row: its cells, and an optional note written on its own
/// indented line beneath it. Callers without notes return the cells alone.
pub(crate) struct Row<const N: usize> {
    pub(crate) cells: [String; N],
    pub(crate) note: Option<String>,
}

impl<const N: usize> From<[String; N]> for Row<N> {
    fn from(cells: [String; N]) -> Self {
        Self { cells, note: None }
    }
}

impl<const N: usize> Row<N> {
    /// The cells, each padded to its column's display width and two spaces
    /// apart, without trailing whitespace.
    ///
    /// ```text
    /// web     /etc/app.conf  0644  repository app.conf
    /// worker  /run/key       0400  inline, 12 bytes
    /// ```
    fn aligned(&self, widths: &[usize; N]) -> String {
        let mut line = String::new();
        for (column, (cell, width)) in self.cells.iter().zip(widths).enumerate() {
            if column > 0 {
                line.push_str("  ");
            }
            line.push_str(cell);
            line.extend(std::iter::repeat_n(' ', width - cell.width()));
        }
        line.truncate(line.trim_end().len());
        line
    }
}

impl HumanWriter<'_> {
    /// Writes `empty` when there are no `rows`, else the bold `headings` and
    /// one line of cells per row, each column as wide as its widest cell.
    /// Cells are escaped before they are measured, so escaped control
    /// characters can't misalign the columns.
    pub(crate) fn table<T, R: Into<Row<N>>, const N: usize>(
        &mut self,
        rows: &[T],
        empty: &str,
        headings: [&str; N],
        row: impl Fn(&T) -> R,
    ) -> io::Result<()> {
        if rows.is_empty() {
            return self.line(empty);
        }
        let headings = Row::from(headings.map(|heading| Escaped(heading).to_string()));
        let rows = rows
            .iter()
            .map(|item| {
                let row = row(item).into();
                Row {
                    cells: row.cells.map(|cell| Escaped(cell).to_string()),
                    note: row.note,
                }
            })
            .collect::<Vec<_>>();
        let mut widths = [0; N];
        for row in std::iter::once(&headings).chain(&rows) {
            for (width, cell) in widths.iter_mut().zip(&row.cells) {
                *width = (*width).max(cell.width());
            }
        }
        writeln!(self.writer, "\x1b[1m{}\x1b[0m", headings.aligned(&widths))?;
        for row in &rows {
            writeln!(self.writer, "{}", row.aligned(&widths))?;
            if let Some(note) = &row.note {
                self.line(format_args!("  {note}"))?;
            }
        }
        Ok(())
    }

    /// The `Next cursor` label of a page that has more items.
    pub(crate) fn next_cursor(&mut self, cursor: Option<&str>) -> io::Result<()> {
        match cursor {
            Some(cursor) => self.label("Next cursor", cursor),
            None => Ok(()),
        }
    }

    /// One `URL` label per URL, with its name and state, and what keeps it
    /// pending; every command that shows URLs lists them this way.
    fn urls<'u>(&mut self, urls: impl IntoIterator<Item = &'u RouteUrl>) -> io::Result<()> {
        for url in urls {
            let name = url
                .name
                .as_ref()
                .map_or_else(String::new, |name| format!(" ({name})"));
            match url.state {
                UrlState::Ready => self.label("URL", format_args!("{}{name} ready", url.url))?,
                UrlState::Pending => self.label(
                    "URL",
                    format_args!("{}{name} pending: {}", url.url, url.waiting_for()),
                )?,
            }
        }
        Ok(())
    }

    /// Renders manifest field changes, one per line: `+` added, `-` removed,
    /// `~` changed.
    fn manifest_changes(&mut self, changes: &[ManifestChange]) -> io::Result<()> {
        for change in changes {
            let (marker, value) = match (&change.before, &change.after) {
                (None, Some(after)) => ('+', ChangeValue::render(after)),
                (Some(before), None) => ('-', ChangeValue::render(before)),
                (Some(before), Some(after)) => (
                    '~',
                    format!(
                        "{} → {}",
                        ChangeValue::render(before),
                        ChangeValue::render(after)
                    ),
                ),
                (None, None) => ('~', "absent".into()),
            };
            self.change(marker, &change.field, &value)?;
        }
        Ok(())
    }

    /// Renders how the manifest's declarations change environments, one per
    /// line: `+` creates one, `!` reports one it no longer declares, and `~`
    /// changes one.
    fn environment_changes(&mut self, changes: &[EnvironmentChange]) -> io::Result<()> {
        for change in changes {
            let marker = match change {
                EnvironmentChange::Create { .. } => '+',
                EnvironmentChange::Undeclared { .. } => '!',
                EnvironmentChange::Source { .. } | EnvironmentChange::Sync { .. } => '~',
            };
            self.change(marker, change.environment().as_str(), &change.summary())?;
        }
        Ok(())
    }

    /// Lists the rendered value of each reference, when there are any.
    fn variables(&mut self, variables: &BTreeMap<String, VariableValue>) -> io::Result<()> {
        if variables.is_empty() {
            return Ok(());
        }
        self.blank()?;
        self.heading("Variables:")?;
        for (reference, value) in variables {
            self.line(format_args!("  {reference} = {value}"))?;
        }
        Ok(())
    }
}

/// The repository a Git source builds from.
fn repository(source: &piqueld_client::ValidatedSource) -> String {
    match source {
        piqueld_client::ValidatedSource::Git { repository, .. } => repository.to_string(),
        piqueld_client::ValidatedSource::Image { image } => image.clone(),
    }
}

/// Human rendering of plan change values, which the daemon sends as JSON text.
struct ChangeValue;

impl ChangeValue {
    /// Renders a change value, falling back to the raw text when it is not JSON.
    fn render(value: &str) -> String {
        serde_json::from_str(value).map_or_else(|_| value.to_owned(), |value| Self::prose(&value))
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
    fn prose(value: &serde_json::Value) -> String {
        use serde_json::Value;
        match value {
            Value::Null => "none".into(),
            Value::String(value) => value.clone(),
            Value::Array(values) => join_or(values.iter().map(Self::prose), "none"),
            Value::Object(values) => {
                if values.get("type").and_then(Value::as_str) == Some("image")
                    && let Some(image) = values.get("image").and_then(Value::as_str)
                {
                    return format!("image {image}");
                }
                join_or(
                    values
                        .iter()
                        .map(|(k, v)| format!("{}: {}", k.replace('_', " "), Self::prose(v))),
                    "none",
                )
            }
            value => value.to_string(),
        }
    }
}

mod applications;
mod environments;
mod operations;
mod secrets;
mod system;
pub(crate) use applications::*;
pub(crate) use environments::*;
pub(crate) use operations::*;
pub(crate) use secrets::*;
pub(crate) use system::*;
