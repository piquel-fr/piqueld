//! API records the overview's tests are built from.
use super::{ApplicationRow, EnvironmentRow};
use piqueld_client::{ApplicationState, EnvironmentStatusView};
use serde_json::json;

/// An environment named `name`, `stopped` or running, with `status`.
pub fn environment(
    name: &str,
    stopped: bool,
    status: Result<(ApplicationState, Option<&str>), &str>,
) -> EnvironmentRow {
    let id = format!("{name}-environment");
    let (status, status_error) = match status {
        Ok((state, message)) => (
            Some(EnvironmentStatusView {
                environment_id: id.clone(),
                state,
                runtime_health: None,
                health_observed_at_ms: None,
                message: message.map(str::to_owned),
                updated_at_ms: 0,
            }),
            None,
        ),
        Err(error) => (None, Some(error.to_owned())),
    };
    EnvironmentRow {
        environment: serde_json::from_value(json!({
            "id": id,
            "application_id": "app-shop0001",
            "name": name,
            "source": { "type": "saved" },
            "run_state": if stopped { "stopped" } else { "running" },
            "resolved_generation": null,
            "delete_intent": false,
            "created_at_ms": 0,
            "updated_at_ms": 0,
        }))
        .expect("environment"),
        status,
        status_error,
    }
}

/// Application `id` with `environments`, and one deployment per
/// `(operation ID, requested at)`.
pub fn application(
    id: &str,
    environments: Vec<EnvironmentRow>,
    deployments: &[(&str, i64)],
) -> ApplicationRow {
    let deployment = |(operation, created_at_ms): &(&str, i64)| {
        serde_json::from_value(json!({
            "operation": {
                "id": operation,
                "environment_id": id,
                "kind": "apply",
                "generation": 1,
                "attempt": 1,
                "state": "requested",
                "error_code": null,
                "error_message": null,
                "created_at_ms": created_at_ms,
                "updated_at_ms": created_at_ms,
                "started_at_ms": null,
                "finished_at_ms": null,
            },
            "template": {
                "id": id,
                "api_version": "piqueld.dev/v1alpha1",
                "kind": "Application",
                "metadata": { "name": "shop" },
                "spec": {
                    "services": [{ "name": "web", "source": { "type": "image", "image": "nginx:alpine" } }],
                },
            },
            "variables": {},
            "application": null,
            "succeeded_at_ms": null,
            "current_target": false,
            "last_successful": false,
        }))
        .expect("deployment")
    };
    ApplicationRow {
        application: serde_json::from_value(json!({
            "id": id,
            "name": id,
            "generation": 4,
            "delete_intent": false,
            "created_at_ms": 0,
            "updated_at_ms": 0,
            "environments": [],
        }))
        .expect("application"),
        environments,
        deployments: deployments.iter().map(deployment).collect(),
        deployment_error: None,
    }
}
