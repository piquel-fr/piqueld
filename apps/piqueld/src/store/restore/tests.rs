use super::*;
use crate::api::Actor::Daemon;
use crate::api::{Mutation, MutationResponse, Restore};
use crate::store::{OperationState, testing::prebuilt};
use piqueld_core::{
    NormalizedApplication, ResolutionSet, ResolvedSource, ServiceName, compile_application,
    manifest::{SecretDeclaration, SecretEncoding, SecretGenerator, SecretMount},
};
use std::collections::BTreeMap;

/// `prebuilt`'s `notes`, whose `web` mounts the generated secret `token`.
fn notes() -> NormalizedApplication {
    let app = prebuilt("app-restore-test");
    let mut manifest = app.to_manifest();
    manifest.spec.services[0].secrets.push(SecretMount {
        name: "token".into(),
        target: "/run/secrets/token".into(),
    });
    manifest.spec.secrets.push(SecretDeclaration {
        name: "token".into(),
        generate: SecretGenerator::Random {
            bytes: 16,
            encoding: SecretEncoding::Hex,
        },
    });
    manifest.validate().unwrap().normalize(app.id().clone())
}

/// Deploys `app` with `web` running the digest of `byte` and the current
/// version of `token`, until it succeeds when `succeed`.
async fn deploy(
    store: &Store,
    app: &NormalizedApplication,
    byte: char,
    succeed: bool,
) -> Operation {
    let environment = EnvironmentId::default_for(app.id());
    let operation = store.request_deploy(&environment, None).await.unwrap();
    store.mark_running(&operation.id).await;
    let digest = format!(
        "ghcr.io/example/notes@sha256:{}",
        byte.to_string().repeat(64)
    );
    let web = ResolvedSource::parse_image("ghcr.io/example/notes:1.4.0", digest).unwrap();
    let resolutions = ResolutionSet {
        sources: BTreeMap::from([(ServiceName::parse("web").unwrap(), web)]),
        secret_names: store.pin_secrets(&operation, app).await.unwrap(),
    };
    let instance = store.instance_id().clone();
    let target = compile_application(app, &environment, instance, &resolutions).unwrap();
    store.save_prepared(&operation, &target).await.unwrap();
    if succeed {
        store.publish_prepared(&operation).await.unwrap();
        store
            .transition_operation(
                &operation.id,
                OperationState::Running,
                OperationState::Succeeded,
                None,
            )
            .await
            .unwrap();
    }
    operation
}

/// Restores `deployment` of environment `id`.
async fn restore(
    store: &Store,
    id: &EnvironmentId,
    deployment: &str,
) -> Result<Operation, StoreError> {
    let restore = Restore::Environment {
        id: id.clone(),
        deployment: deployment.to_owned(),
    };
    accept(store, restore).await
}

/// Accepts `restore`, returning the deployment it started.
async fn accept(store: &Store, restore: Restore) -> Result<Operation, StoreError> {
    match store
        .accept(Daemon, Mutation::Restore(restore), None, false, None)
        .await?
    {
        (MutationResponse::Operation(accepted), true) => {
            Ok(store.operation(&accepted.operation_id).await.unwrap())
        }
        other => panic!("accepted operation expected, got {other:?}"),
    }
}

/// A restore deploys the earlier deployment's own target, with its image and
/// the secret version it pinned rather than the one regenerated since, and
/// records its manifest, release and origin as a new deployment.
#[tokio::test]
async fn restores_exactly_what_the_deployment_ran() {
    let (_temp, store) = Store::temporary().await;
    let app = notes();
    store
        .save_application(&ApplicationTemplate::from(&app), None, None)
        .await
        .unwrap();
    let environment = EnvironmentId::default_for(app.id());
    let first = deploy(&store, &app, 'a', true).await;
    store
        .regenerate_secret(Daemon, &environment, "token", 1)
        .await
        .unwrap();
    let second = deploy(&store, &app, 'b', true).await;
    let (first_target, second_target) = (
        store.prepared_target(&first.id).await.unwrap().unwrap(),
        store.prepared_target(&second.id).await.unwrap().unwrap(),
    );
    assert_ne!(first_target.secret_names, second_target.secret_names);

    let restored = restore(&store, &environment, &first.id).await.unwrap();
    assert_eq!(
        store.prepared_target(&restored.id).await.unwrap(),
        Some(first_target)
    );
    let history = store.deployments(&environment, None, 3).await.unwrap();
    let [restored_view, _, first_view] = history.items.as_slice() else {
        panic!("three deployments expected, got {history:?}")
    };
    assert_eq!(restored_view.operation.id, restored.id);
    assert_eq!(
        restored_view.origin,
        DeploymentOrigin::Restore {
            deployment: first.id.clone()
        }
    );
    assert_eq!(restored_view.application, first_view.application);
    assert_eq!(restored_view.release, first_view.release);
    assert!(restored_view.release.is_some());
    assert_eq!(
        store.status(&environment).await.unwrap().state,
        ApplicationState::Pending
    );
}

/// Only a succeeded deployment of the environment itself is restored,
/// through the endpoint of its kind, and never with newer secret versions
/// than it pinned.
#[tokio::test]
async fn refuses_what_cannot_run_again() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("db")).await.unwrap();
    let app = notes();
    store
        .save_application(&ApplicationTemplate::from(&app), None, None)
        .await
        .unwrap();
    let environment = EnvironmentId::default_for(app.id());
    let succeeded = deploy(&store, &app, 'a', true).await;
    let failed = deploy(&store, &app, 'b', false).await;
    assert!(matches!(
        restore(&store, &environment, &failed.id).await,
        Err(StoreError::Restore(RestoreError::NotRestorable { deployment, .. }))
            if deployment == failed.id
    ));
    let unknown = EnvironmentId::parse("env-unknown-0001").unwrap();
    assert!(matches!(
        restore(&store, &unknown, &succeeded.id).await,
        Err(StoreError::NotFound)
    ));
    // The preview endpoint finds only previews.
    let preview = Restore::Preview {
        id: environment.clone(),
        deployment: succeeded.id.clone(),
    };
    assert!(matches!(
        accept(&store, preview).await,
        Err(StoreError::NotFound)
    ));

    // Key recovery discards the value `succeeded` pinned.
    std::fs::write(temp.path().join("secrets.key"), [42; 32]).unwrap();
    store.recover_secret_key(Daemon).await.unwrap();
    assert!(matches!(
        restore(&store, &environment, &succeeded.id).await,
        Err(StoreError::Restore(RestoreError::SecretsGone { secrets, .. })) if secrets == ["token"]
    ));
    // Nothing was accepted.
    assert_eq!(
        store
            .latest_operation_for_environment(&environment)
            .await
            .unwrap()
            .unwrap()
            .id,
        failed.id
    );
}
