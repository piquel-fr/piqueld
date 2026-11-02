//! Reports about what the daemon did: operations, deployments, builds, logs, events and deletions.

use crate::deployments::Deployed;
use piqueld_client::{
    AcceptedOperation, ApplicationLogs, BuildLogPage, BuildRecord, DeletedApplication, Event,
    Operation, OperationState, Page, SavedApplication,
};
use serde::Serialize;

// `wait` result: the deployment is as ready as asked.
report!(Deployed, self, out, {
    out.line(format_args!(
        "Deployment {} of {} is ready",
        self.deployment_id, self.slug
    ))?;
    out.urls(self.urls.iter().flatten())
});

/// A deploy accepted with `--no-wait`: the accepted operation's fields, and
/// the deployment's.
#[derive(Serialize)]
pub(crate) struct AcceptedDeploymentReport {
    pub(crate) generation: u64,
    #[serde(flatten)]
    pub(crate) deployed: Deployed,
}
report!(AcceptedDeploymentReport, self, out, {
    out.line(format_args!(
        "Accepted operation {} for environment {}",
        self.deployed.operation_id, self.deployed.environment_id
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
    out.next_cursor(self.next_cursor.as_deref())
});

/// Name of a deleted secret or notification destination.
#[derive(Serialize)]
pub(crate) struct NameDeletionReport<'a> {
    pub(crate) deleted: &'a str,
}
report!(NameDeletionReport<'_>, self, out, {
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
    out.next_cursor(self.next_cursor.as_deref())
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
    /// The first deployment's outcome and operation.
    pub(crate) outcome: OperationState,
    pub(crate) operation: &'a Operation,
    /// Every deployment, one per environment that builds.
    pub(crate) operations: &'a [Operation],
}
report!(SavedDeploymentReport<'_>, self, out, {
    for (index, operation) in self.operations.iter().enumerate() {
        if index > 0 {
            out.blank()?;
        }
        operation.render_human(out)?;
    }
    Ok(())
});

/// Accepted reconcile/deploy followed by its awaited outcome and the
/// deployment's fields; renders like the operation, with its URLs.
#[derive(Serialize)]
pub(crate) struct OperationOutcomeReport<'a> {
    pub(crate) accepted: &'a AcceptedOperation,
    pub(crate) operation: Operation,
    #[serde(flatten)]
    pub(crate) deployed: Deployed,
}
report!(OperationOutcomeReport<'_>, self, out, {
    self.operation.render_human(out)?;
    out.urls(self.deployed.urls.iter().flatten())
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
            outcome: Some("deleted"),
            ..Self::accepted(accepted)
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
            outcome: Some("deleted"),
            ..Self::accepted(deleted)
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
