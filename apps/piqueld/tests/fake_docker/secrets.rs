//! Runtime secrets: rotation, cleanup, missing secrets and an unusable secret key.

use super::*;

#[tokio::test]
async fn secret_rotation_requires_deploy_and_service_references_follow_pinned_versions() {
    let harness = ControllerHarness::new().await;
    let applications = harness.applications();
    let first = harness.apply(manifest(), Some(0)).await;
    harness.finish(&first).await;
    let app = harness.application_of(&first.environment_id).await;
    harness
        .store_token(&first.environment_id, 0, b"version-one")
        .await;
    let deployment = harness.apply(with_token(manifest()), Some(1)).await;
    harness.finish(&deployment).await;
    let target = harness.target(&first.environment_id).await;
    let old = target.secret_names["token"].clone();
    assert_eq!(
        harness.docker.secrets.lock().await[&old].value,
        b"version-one"
    );
    harness
        .store_token(&first.environment_id, 1, b"version-two")
        .await;
    harness.scan().await;
    assert_eq!(
        harness.target(&first.environment_id).await,
        target,
        "rotation alone must not update services"
    );
    let next = applications
        .deploy(&first.environment_id, None)
        .await
        .unwrap();
    harness.finish(&next).await;
    let target = harness.target(&first.environment_id).await;
    let new = &target.secret_names["token"];
    assert_ne!(&old, new);
    assert_eq!(
        harness.docker.secrets.lock().await[new].value,
        b"version-two"
    );
    assert_eq!(
        harness
            .docker
            .observe(&first.environment_id)
            .await
            .unwrap()
            .services[0]
            .secrets,
        target.services[0].secrets
    );

    // Historical completed deployments must not block deletion forever.
    let clean = harness.apply(manifest(), Some(2)).await;
    harness.finish(&clean).await;
    applications
        .delete_stored_secret(Daemon, &app, "token", 2)
        .await
        .unwrap();
    assert!(harness.docker.secrets.lock().await.is_empty());
    assert!(harness.store.stored_secrets(&app).await.unwrap().is_empty());
}

#[tokio::test]
async fn application_deletion_journals_secret_cleanup_failures() {
    let harness = ControllerHarness::new().await;
    let applications = harness.applications();
    let first = harness.apply(manifest(), Some(0)).await;
    harness.finish(&first).await;
    harness
        .store
        .put_secret(Daemon, &first.environment_id, "token", 0, b"value".to_vec())
        .await
        .unwrap();
    harness
        .docker
        .fail_secret_removal
        .store(true, Ordering::SeqCst);
    let deletion = applications
        .delete(&first.environment_id, Some(1))
        .await
        .unwrap();
    harness.scan().await;
    let events = harness.operation_events(&deletion.id).await;
    let phases = |kind: &str| {
        events
            .iter()
            .filter(|event| event.kind == kind && event.phase.as_deref() == Some("remove_secrets"))
            .count()
    };
    assert_eq!(phases("action_requested"), 1, "{events:#?}");
    assert!(events.iter().any(|event| event.kind == "action_failed"
        && event.phase.as_deref() == Some("remove_secrets")
        && event.error_code.as_deref() == Some("docker_request_failed")));
    assert!(harness.store.get(&first.environment_id).await.is_ok());
}

/// A mounted secret nobody stored fails the deployment as `secret_missing`,
/// naming it, rather than as a journal failure.
#[tokio::test]
async fn missing_stored_secrets_fail_deployment_with_their_names() {
    let harness = ControllerHarness::new().await;
    let failed = harness.apply(with_token(manifest()), Some(0)).await;
    harness.scan().await;
    let result = harness.operation(&failed.id).await;
    assert_eq!(result.error_code.as_deref(), Some("secret_missing"));
    assert!(result.error_message.as_deref().unwrap().contains("token"));
}

#[tokio::test]
async fn secret_key_recovery_leaves_running_services_and_blocks_discarded_rollouts() {
    let harness = ControllerHarness::new().await;
    let applications = harness.applications();
    let initial = harness.apply(manifest(), Some(0)).await;
    harness.finish(&initial).await;
    let id = &initial.environment_id;
    harness.store_token(id, 0, b"original").await;
    let deployed = harness.apply(with_token(manifest()), Some(1)).await;
    harness.finish(&deployed).await;
    let target = harness.target(id).await;
    let original = harness.docker.observe(id).await.unwrap();

    std::fs::remove_file(harness.database_path.with_file_name("secrets.key")).unwrap();
    harness.store.recover_secret_key(Daemon).await.unwrap();
    let failed = applications.deploy(id, None).await.unwrap();
    harness.scan().await;
    let result = harness.operation(&failed.id).await;
    assert_eq!(result.error_code.as_deref(), Some("secret_unavailable"));
    assert!(result.error_message.as_deref().unwrap().contains("token"));
    assert_eq!(harness.target(id).await, target);
    assert_eq!(harness.docker.observe(id).await.unwrap(), original);

    harness.store_token(id, 1, b"replacement").await;
    let replacement = applications.deploy(id, None).await.unwrap();
    harness.finish(&replacement).await;
    let new_target = harness.target(id).await;
    assert_eq!(
        harness.docker.secrets.lock().await[&new_target.secret_names["token"]].value,
        b"replacement"
    );
    assert_eq!(
        harness.docker.secrets.lock().await[&target.secret_names["token"]].value,
        b"original",
        "running services keep their existing Docker secret"
    );
}

#[tokio::test]
async fn missing_secret_key_fails_rollout_before_docker_mutation() {
    let harness = ControllerHarness::new().await;
    let first = harness.apply(manifest(), Some(0)).await;
    harness.finish(&first).await;
    harness
        .store_token(&first.environment_id, 0, b"value")
        .await;
    std::fs::remove_file(harness.database_path.with_file_name("secrets.key")).unwrap();
    let deployment = harness.apply(with_token(manifest()), Some(1)).await;
    harness.scan().await;
    let operation = harness.operation(&deployment.id).await;
    assert_eq!(
        operation.error_code.as_deref(),
        Some("secret_storage_unavailable")
    );
    assert!(harness.docker.secrets.lock().await.is_empty());
    let events = harness.operation_events(&deployment.id).await;
    let service = |kind: &str| {
        events
            .iter()
            .find(|event| event.kind == kind && event.phase.as_deref() == Some("ensure_service"))
    };
    let failed = service("action_failed").expect("service action failed");
    assert_eq!(
        failed.error_code.as_deref(),
        Some("secret_storage_unavailable")
    );
    assert_eq!(
        failed.diagnostic.as_ref().unwrap().causes,
        ["Secret master key: missing"]
    );
    assert!(
        service("action_requested").is_none(),
        "no Docker request is made without secret values: {events:#?}"
    );
}
