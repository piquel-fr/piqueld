//! Preview lifecycle: idempotent creation without the application revision,
//! separation from environments, and shared secret access.
use super::*;
use crate::api::Actor::Daemon;
use crate::api::{Mutation, MutationResponse, PreviewMutation};
use piqueld_core::{
    GitBranch, PreviewSlot,
    api::{CreatedPreview, EnvironmentAccess, SecretAccess},
    manifest::{SecretMount, parse_template_toml},
};

/// The `notes` application, repository-backed unless `saved`, whose `web`
/// service mounts the stored secret `stripe`.
fn template(saved: bool) -> ApplicationTemplate {
    let connection = if saved {
        ""
    } else {
        "[spec.manifest]\npath='app.toml'\n[spec.manifest.repository]\nurl='https://example.com/notes.git'\nbranch='main'\n"
    };
    let mut manifest = parse_template_toml(&format!(
        "api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='notes'\n{connection}[[spec.services]]\nname='web'\n[spec.services.source]\ntype='image'\nimage='nginx:alpine'"
    ))
    .unwrap()
    .normalize(ApplicationId::parse("app-pending-01").unwrap())
    .to_manifest();
    manifest.spec.services[0].secrets = vec![SecretMount {
        name: "stripe".into(),
        target: "/run/secrets/stripe".into(),
    }];
    manifest
        .validate_template()
        .unwrap()
        .normalize(ApplicationId::parse("app-pending-01").unwrap())
}

/// Saves `template(saved)` and returns its ID.
async fn save(store: &Store, saved: bool) -> ApplicationId {
    let (MutationResponse::Saved(saved), _) = store
        .accept(
            Daemon,
            Mutation::Save {
                application: Box::new(template(saved)),
                expected_application_id: None,
                deploy: false,
            },
            Some(0),
            false,
            None,
        )
        .await
        .unwrap()
    else {
        panic!("saved")
    };
    ApplicationId::parse(saved.application_id).unwrap()
}

/// Creates the preview of `branch` and `slot`, without a revision.
async fn create(
    store: &Store,
    application: &ApplicationId,
    branch: &str,
    slot: Option<&str>,
) -> Result<CreatedPreview, StoreError> {
    let mutation = Mutation::Preview(PreviewMutation::Create {
        application: application.clone(),
        branch: GitBranch::parse(branch).unwrap(),
        slot: slot.map(|slot| PreviewSlot::parse(slot).unwrap()),
    });
    match store.accept(Daemon, mutation, None, false, None).await? {
        (MutationResponse::Preview(created), _) => Ok(*created),
        _ => panic!("preview"),
    }
}

#[tokio::test]
async fn creating_a_preview_is_idempotent_on_branch_and_slot_and_leaves_the_revision() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("db")).await.unwrap();
    let application = save(&store, false).await;

    // Concurrent agents creating the same preview get one preview and one
    // deployment between them.
    let (first, second) = tokio::join!(
        create(&store, &application, "feat/login", None),
        create(&store, &application, "feat/login", None),
    );
    let (first, second) = (first.unwrap(), second.unwrap());
    assert_eq!(first.preview.id, second.preview.id);
    assert_eq!(first.operation.operation_id, second.operation.operation_id);
    assert_ne!(first.created, second.created);
    // Repeating it later still returns the same deployment, never a new one.
    let again = create(&store, &application, "feat/login", None)
        .await
        .unwrap();
    assert!(!again.created);
    assert_eq!(again.operation.operation_id, first.operation.operation_id);

    let slotted = create(&store, &application, "feat/login", Some("agent-2"))
        .await
        .unwrap();
    assert!(slotted.created);
    assert_ne!(slotted.preview.id, first.preview.id);
    assert_ne!(slotted.preview.name, first.preview.name);

    // Previews never needed or advanced the application revision.
    assert_eq!(store.application(&application).await.unwrap().generation, 1);
    assert_eq!(store.previews(&application).await.unwrap().len(), 2);
}

#[tokio::test]
async fn previews_need_a_repository_and_environment_changes_never_reach_them() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("saved")).await.unwrap();
    let saved = save(&store, true).await;
    assert!(matches!(
        create(&store, &saved, "feat/login", None).await,
        Err(StoreError::PreviewRequiresRepository)
    ));

    let store = Store::open(temp.path().join("db")).await.unwrap();
    let application = save(&store, false).await;
    let preview = create(&store, &application, "feat/login", None)
        .await
        .unwrap()
        .preview;
    let production = EnvironmentId::default_for(&application);
    let environments = store.environments(&application).await.unwrap();
    assert_eq!(environments.len(), 1);
    assert_eq!(environments[0].id, production);

    let generation = Some(1);
    for mutation in [
        Mutation::deploy(preview.id.clone()),
        Mutation::Delete {
            id: preview.id.clone(),
        },
        Mutation::Reconcile {
            id: preview.id.clone(),
        },
        Mutation::RenameEnvironment {
            id: preview.id.clone(),
            name: EnvironmentName::parse("renamed").unwrap(),
        },
        Mutation::SetBranch {
            id: preview.id.clone(),
            branch: TrackedBranch::new("main".into(), None).unwrap(),
        },
        Mutation::Preview(PreviewMutation::Deploy {
            id: production.clone(),
        }),
        Mutation::Preview(PreviewMutation::Delete {
            id: production.clone(),
        }),
    ] {
        let result = store
            .accept(Daemon, mutation, generation, false, None)
            .await;
        assert!(
            matches!(result, Err(StoreError::NotFound)),
            "acted on the wrong kind: {result:?}"
        );
    }
    // A preview's slug is its name, so no environment can take it.
    assert!(matches!(
        store
            .accept(
                Daemon,
                Mutation::CreateEnvironment {
                    application: application.clone(),
                    name: preview.name.clone(),
                    branch: None,
                },
                generation,
                false,
                None,
            )
            .await,
        Err(StoreError::AlreadyExists)
    ));
}

#[tokio::test]
async fn previews_mount_shared_secrets_only_when_their_access_allows_previews() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("db")).await.unwrap();
    let application = save(&store, false).await;
    let preview = create(&store, &application, "feat/login", None)
        .await
        .unwrap()
        .preview;
    let stored = store.get(&preview.id).await.unwrap();
    let rendered = stored
        .render(&template(false), "operation-1")
        .unwrap()
        .application;

    // Every environment, by default, but no previews.
    store
        .put_stored_secret(Daemon, &application, "stripe", 0, b"key".to_vec(), None)
        .await
        .unwrap();
    assert!(matches!(
        store.check_secret_access(&preview, &rendered).await,
        Err(StoreError::SecretAccessDenied { secret, .. }) if secret == "stripe"
    ));
    // A preview is never one of the environments a list names.
    let named = SecretAccess {
        environments: EnvironmentAccess::Only([preview.id.clone()].into()),
        previews: false,
    };
    assert!(
        store
            .set_secret_access(Daemon, &application, "stripe", &named)
            .await
            .is_err()
    );
    let previews = SecretAccess {
        environments: EnvironmentAccess::Only([].into()),
        previews: true,
    };
    store
        .set_secret_access(Daemon, &application, "stripe", &previews)
        .await
        .unwrap();
    store
        .check_secret_access(&preview, &rendered)
        .await
        .unwrap();
}
