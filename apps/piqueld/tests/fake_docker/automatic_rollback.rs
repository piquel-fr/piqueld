use super::*;
use piqueld_core::{api::DeploymentOrigin, manifest::Rollback};

/// An application with a job whose `production` environment has
/// `rollback`.
fn manifest(rollback: Option<Rollback>) -> ApplicationManifest {
    let mut manifest = manifest_with_jobs(&["migrate"], 300);
    manifest
        .spec
        .set_environment_rollback("production", rollback);
    manifest
}

/// Deploys `manifest` successfully, then again once its tag resolves to
/// another image whose rollout fails. Returns both deployments.
async fn fail_second_rollout(
    harness: &ControllerHarness,
    manifest: ApplicationManifest,
) -> (Operation, Operation) {
    let first = harness.apply(manifest, None).await;
    harness.finish(&first).await;
    harness
        .docker
        .registry
        .lock()
        .await
        .digests
        .insert("ghcr.io/example/notes:1.4.0".into(), "b".repeat(64));
    harness
        .docker
        .fail_convergence
        .store(true, Ordering::SeqCst);
    let second = harness
        .applications()
        .deploy(&first.environment_id, None)
        .await
        .unwrap();
    assert_eq!(
        harness.scan_result(&second).await,
        (OperationState::Failed, Some("service_update_failed".into()))
    );
    (first, second)
}

/// The latest operation of `harness`'s environment `id`.
async fn latest(harness: &ControllerHarness, id: &EnvironmentId) -> Operation {
    harness
        .store
        .latest_operation_for_environment(id)
        .await
        .unwrap()
        .unwrap()
}

/// The origin of deployment `id` of environment `environment`.
async fn origin(
    harness: &ControllerHarness,
    environment: &EnvironmentId,
    id: &str,
) -> DeploymentOrigin {
    harness
        .store
        .deployments(environment, None, 50)
        .await
        .unwrap()
        .items
        .into_iter()
        .find(|deployment| deployment.operation.id == id)
        .unwrap()
        .origin
}

#[tokio::test]
async fn a_failed_rollout_restores_the_last_successful_deployment() {
    let harness = ControllerHarness::new().await;
    let (first, second) = fail_second_rollout(&harness, manifest(Some(Rollback::Automatic))).await;
    let environment = &first.environment_id;
    let original = harness.operation(&first.id).await;
    let rollback = latest(&harness, environment).await;
    assert_ne!(rollback.id, second.id);
    assert_eq!(
        origin(&harness, environment, &rollback.id).await,
        DeploymentOrigin::Restore {
            deployment: first.id.clone(),
            rolled_back: Some(second.id.clone()),
        }
    );
    assert!(
        harness
            .operation_events(&second.id)
            .await
            .iter()
            .any(|event| event.kind == "rollback_requested")
    );
    // It runs the first deployment's image again, without its job.
    harness
        .docker
        .fail_convergence
        .store(false, Ordering::SeqCst);
    harness.finish(&rollback).await;
    let image = |target: &ResolvedApplication| target.services[0].image.to_string();
    assert_eq!(
        image(&harness.target(environment).await),
        image(
            &harness
                .store
                .prepared_target(&original.id)
                .await
                .unwrap()
                .unwrap()
        )
    );
    assert_eq!(harness.started_jobs().await, ["migrate", "migrate"]);
    // Repair maintains the rollback, never the failed deployment.
    harness.scan().await;
    assert_eq!(latest(&harness, environment).await.id, rollback.id);
}

/// A rollback whose own rollout fails is left failed.
#[tokio::test]
async fn a_failed_rollback_is_not_rolled_back_again() {
    let harness = ControllerHarness::new().await;
    let (first, _) = fail_second_rollout(&harness, manifest(Some(Rollback::Automatic))).await;
    let rollback = latest(&harness, &first.environment_id).await;
    assert_eq!(
        harness.scan_result(&rollback).await,
        (OperationState::Failed, Some("service_update_failed".into()))
    );
    assert_eq!(
        latest(&harness, &first.environment_id).await.id,
        rollback.id
    );
}

#[tokio::test]
async fn manual_environments_stay_on_the_failed_deployment() {
    for rollback in [None, Some(Rollback::Manual)] {
        let harness = ControllerHarness::new().await;
        let (first, second) = fail_second_rollout(&harness, manifest(rollback)).await;
        assert_eq!(latest(&harness, &first.environment_id).await.id, second.id);
    }
}

#[tokio::test]
async fn a_failure_without_an_earlier_success_says_why_it_was_not_rolled_back() {
    let harness = ControllerHarness::new().await;
    harness
        .docker
        .fail_convergence
        .store(true, Ordering::SeqCst);
    let operation = harness
        .apply(manifest(Some(Rollback::Automatic)), None)
        .await;
    assert_eq!(
        harness.scan_result(&operation).await,
        (OperationState::Failed, Some("service_update_failed".into()))
    );
    let events = harness.operation_events(&operation.id).await;
    let unavailable = events
        .iter()
        .find(|event| event.kind == "rollback_unavailable")
        .unwrap();
    assert_eq!(
        unavailable.message.as_deref(),
        Some("not rolled back automatically: no earlier deployment of the environment succeeded")
    );
    assert_eq!(
        latest(&harness, &operation.environment_id).await.id,
        operation.id
    );
}
