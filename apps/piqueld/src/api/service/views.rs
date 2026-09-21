//! Shared application projections and bounded runtime diagnostics.
use crate::store::{ApplicationStatus, StoredApplication};
use piqueld_core::{
    ObservedApplication,
    api::{
        ApplicationStatusView, ApplicationView, DiagnosticView, ObservedApplicationView,
        ObservedServiceView,
    },
    resource::{Convergence, ObservedService, TaskDiagnostic, TaskState},
};

pub(super) fn application_view(stored: StoredApplication) -> ApplicationView {
    ApplicationView {
        generation: stored.generation,
        resolved_generation: stored.resolved_generation,
        spec_hash: stored.application.spec_hash(),
        application: stored.application,
        delete_intent: stored.delete_intent,
        created_at_ms: stored.created_at_ms,
        updated_at_ms: stored.updated_at_ms,
    }
}

pub(super) fn status_view(status: ApplicationStatus) -> ApplicationStatusView {
    ApplicationStatusView {
        application_id: status.application_id.to_string(),
        state: status.state,
        runtime_health: status.runtime_health,
        message: status.message,
        updated_at_ms: status.updated_at_ms,
    }
}

const MAX_DETAIL_DIAGNOSTICS: usize = 24;
const MAX_SERVICE_DIAGNOSTICS: usize = 8;

pub(super) fn observed_view(
    stored: &StoredApplication,
    observed: &ObservedApplication,
    reconciled: bool,
) -> ObservedApplicationView {
    let services = stored
        .resolved
        .iter()
        .flat_map(|target| &target.services)
        .map(|desired| {
            let runtime = observed
                .services
                .iter()
                .find(|service| service.name == desired.name.as_str());
            let (image, observed_replicas, healthy_replicas, convergence, diagnostics) = runtime
                .map_or_else(
                    || {
                        let diagnostics = if reconciled {
                            vec![DiagnosticView {
                                code: "service_missing".into(),
                                message:
                                    "the desired service was not found in the runtime observation"
                                        .into(),
                            }]
                        } else {
                            Vec::new()
                        };
                        (
                            None,
                            0,
                            0,
                            if reconciled {
                                Convergence::Failed
                            } else {
                                Convergence::Updating
                            },
                            diagnostics,
                        )
                    },
                    |service| {
                        (
                            Some(service.image.clone()),
                            service.replicas,
                            healthy_replicas(service),
                            service.convergence.clone(),
                            service_diagnostics(service),
                        )
                    },
                );
            ObservedServiceView {
                name: desired.logical_name.to_string(),
                image,
                desired_replicas: desired.replicas,
                observed_replicas,
                healthy_replicas,
                convergence,
                diagnostics,
            }
        })
        .collect();
    ObservedApplicationView {
        services,
        network_count: u32::try_from(observed.networks.len()).unwrap_or(u32::MAX),
        volume_count: u32::try_from(observed.volumes.len()).unwrap_or(u32::MAX),
    }
}

fn healthy_replicas(service: &ObservedService) -> u16 {
    u16::try_from(
        service
            .tasks
            .iter()
            .filter(|task| {
                task.desired_running
                    && task.state == TaskState::Running
                    && if service.healthcheck_configured {
                        task.healthy == Some(true)
                    } else {
                        task.healthy != Some(false)
                    }
            })
            .count(),
    )
    .unwrap_or(u16::MAX)
}

fn service_diagnostics(service: &ObservedService) -> Vec<DiagnosticView> {
    let mut diagnostics = Vec::new();
    if matches!(
        service.convergence,
        Convergence::Degraded | Convergence::Failed
    ) {
        let healthy_replicas = healthy_replicas(service);
        diagnostics.push(DiagnosticView {
            code: "service_not_converged".into(),
            message: format!(
                "{} of {} observed replicas are healthy",
                healthy_replicas, service.replicas
            ),
        });
    }
    diagnostics.extend(
        service
            .tasks
            .iter()
            .filter(|task| task.desired_running)
            .filter_map(|task| task.diagnostic.as_ref())
            .map(|diagnostic| match diagnostic {
                TaskDiagnostic::Failed { exit_code } => DiagnosticView {
                    code: "task_failed".into(),
                    message: exit_code.map_or_else(
                        || "a desired task exited unsuccessfully".into(),
                        |code| format!("a desired task exited with status code {code}"),
                    ),
                },
                TaskDiagnostic::Rejected => DiagnosticView {
                    code: "task_rejected".into(),
                    message: "the runtime rejected a desired task before it started".into(),
                },
            }),
    );
    diagnostics.truncate(MAX_SERVICE_DIAGNOSTICS);
    diagnostics
}

pub(super) fn detail_diagnostics(
    status: &ApplicationStatusView,
    observed: &ObservedApplicationView,
    operation: Option<&piqueld_core::Operation>,
) -> Vec<DiagnosticView> {
    let mut diagnostics = Vec::new();
    if let Some(message) = &status.message {
        diagnostics.push(DiagnosticView {
            code: "application_status".into(),
            message: message.clone(),
        });
    }
    if let Some(operation) = operation
        && let (Some(code), Some(message)) = (&operation.error_code, &operation.error_message)
    {
        diagnostics.push(DiagnosticView {
            code: code.clone(),
            message: message.clone(),
        });
    }
    diagnostics.extend(
        observed
            .services
            .iter()
            .flat_map(|service| service.diagnostics.iter().cloned()),
    );
    diagnostics.truncate(MAX_DETAIL_DIAGNOSTICS);
    diagnostics
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use piqueld_core::resource::{ObservedTask, TaskDiagnostic};

    use super::*;

    #[test]
    fn service_diagnostics_ignore_historical_tasks() {
        let service = ObservedService {
            name: "web".into(),
            image: "example/web@sha256:digest".into(),
            replicas: 1,
            environment: BTreeMap::new(),
            command: Vec::new(),
            arguments: Vec::new(),
            mounts: Vec::new(),
            healthcheck: None,
            healthcheck_configured: false,
            resources: None,
            networks: Vec::new(),
            labels: BTreeMap::new(),
            runtime_configuration_matches: true,
            tasks: vec![
                ObservedTask {
                    state: TaskState::Failed,
                    healthy: None,
                    desired_running: false,
                    diagnostic: Some(TaskDiagnostic::Failed { exit_code: Some(1) }),
                },
                ObservedTask {
                    state: TaskState::Rejected,
                    healthy: None,
                    desired_running: true,
                    diagnostic: Some(TaskDiagnostic::Rejected),
                },
            ],
            convergence: Convergence::Converged,
        };

        let diagnostics = service_diagnostics(&service);

        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].code, "task_rejected");
    }

    #[test]
    fn pending_runtime_health_is_not_reported_as_healthy() {
        let task = ObservedTask {
            state: TaskState::Running,
            healthy: None,
            desired_running: true,
            diagnostic: None,
        };
        let mut service = ObservedService {
            name: "web".into(),
            image: "example/web@sha256:digest".into(),
            replicas: 1,
            environment: BTreeMap::new(),
            command: Vec::new(),
            arguments: Vec::new(),
            mounts: Vec::new(),
            healthcheck: None,
            healthcheck_configured: true,
            resources: None,
            networks: Vec::new(),
            labels: BTreeMap::new(),
            runtime_configuration_matches: false,
            tasks: vec![task],
            convergence: Convergence::Updating,
        };

        assert_eq!(healthy_replicas(&service), 0);
        service.healthcheck_configured = false;
        assert_eq!(healthy_replicas(&service), 1);
    }
}
