//! Fresh-database coverage for the supported durable lifecycle.

use piqueld::store::{ApplicationState, SCHEMA_VERSION, SqliteStore, WorkState};
use piqueld_core::resource::{ResolutionSet, ResolvedSource, compile_application};
use piqueld_core::{ApplicationId, InstanceId, parse_toml};
use std::os::unix::fs::PermissionsExt;

fn application() -> piqueld_core::NormalizedApplication {
    parse_toml(include_str!(
        "../../../crates/piqueld-core/tests/fixtures/manifests/prebuilt.toml"
    ))
    .expect("fixture is valid")
    .normalize(ApplicationId::parse("app-persist-01").expect("fixture ID is valid"))
}

fn resolved(
    application: &piqueld_core::NormalizedApplication,
    instance_id: &str,
) -> piqueld_core::resource::ResolvedApplication {
    let resolutions = ResolutionSet {
        sources: [(
            "web".into(),
            ResolvedSource::Image {
                requested: "ghcr.io/example/notes:1.4.0".into(),
                digest_reference: format!("ghcr.io/example/notes@sha256:{}", "a".repeat(64)),
            },
        )]
        .into_iter()
        .collect(),
    };
    compile_application(
        application,
        InstanceId::parse(instance_id).expect("store instance ID is valid"),
        &resolutions,
    )
    .expect("fixture resolves")
}

#[tokio::test]
async fn fresh_database_persists_resolved_state_and_retains_volumes() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let store = SqliteStore::open(directory.path().join("control-plane.db"))
        .await
        .expect("fresh database opens");
    assert_eq!(SCHEMA_VERSION, 1);

    let application = application();
    let resolved = resolved(&application, store.instance_id());
    let created = store
        .create(&application, &resolved, &["ensure_network".into()])
        .await
        .expect("application is created");

    let stored = store
        .get(&application.id)
        .await
        .expect("application is readable");
    assert_eq!(stored.resolved, resolved);
    assert_eq!(stored.generation, 1);
    assert!(!stored.delete_intent);
    let status = store
        .status(&application.id)
        .await
        .expect("status is readable");
    assert_eq!(status.state, ApplicationState::Pending);

    let (operation, steps) = store
        .operation_with_steps(&created.operation_id)
        .await
        .expect("operation is readable");
    assert_eq!(operation.state, WorkState::Pending);
    assert_eq!(steps.len(), 1);
    assert_eq!(steps[0].action, "ensure_network");

    let deleted = store
        .request_delete(&application.id, 1, &["remove_service".into()])
        .await
        .expect("delete intent is durable");
    assert_eq!(deleted.generation, 2);
    assert_eq!(
        store
            .status(&application.id)
            .await
            .expect("status is readable")
            .state,
        ApplicationState::Deleting
    );

    store
        .finalize_delete(&application.id, deleted.generation)
        .await
        .expect("delete tombstone is durable");
    assert!(matches!(
        store.get(&application.id).await,
        Err(piqueld::store::StoreError::NotFound)
    ));
    assert!(
        store
            .list(None, 50)
            .await
            .expect("application list is readable")
            .items
            .is_empty()
    );
}

#[tokio::test]
async fn generation_updates_and_create_idempotency_are_durable() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let store = SqliteStore::open(directory.path().join("control-plane.db"))
        .await
        .expect("fresh database opens");
    let application = application();
    let resolved = resolved(&application, store.instance_id());
    let key_hash = format!("sha256:{}", "b".repeat(64));
    let request_hash = application.spec_hash();
    let first = store
        .create_idempotent(
            &application,
            &resolved,
            &["ensure_network".into()],
            &key_hash,
            &request_hash,
        )
        .await
        .expect("idempotent create is durable");
    let replay = store
        .create_idempotent(
            &application,
            &resolved,
            &["ensure_network".into()],
            &key_hash,
            &request_hash,
        )
        .await
        .expect("idempotent retry is readable");
    assert_eq!(first, replay);

    let replaced = store
        .replace(&application, &resolved, 1, &["ensure_network".into()])
        .await
        .expect("replacement increments the generation");
    assert_eq!(replaced.generation, 2);
    assert!(matches!(
        store
            .replace(&application, &resolved, 1, &["ensure_network".into()])
            .await,
        Err(piqueld::store::StoreError::GenerationConflict {
            expected: 1,
            actual: 2
        })
    ));
}

#[tokio::test]
async fn missing_database_parent_is_created_without_rewriting_existing_parent_mode() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let parent = directory.path().join("nested").join("state");
    let database = parent.join("control-plane.db");
    SqliteStore::open(&database)
        .await
        .expect("missing database parents are created");
    assert!(parent.is_dir());

    let before = std::fs::symlink_metadata(&parent)
        .expect("created parent metadata")
        .permissions();
    SqliteStore::open(&database)
        .await
        .expect("existing database parent remains usable");
    let after = std::fs::symlink_metadata(&parent)
        .expect("existing parent metadata")
        .permissions();
    assert_eq!(before.mode(), after.mode());
}
