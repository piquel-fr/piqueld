//! Preview lifecycle: idempotent creation without the application revision,
//! separation from environments, and shared secret access.
use super::*;
use crate::api::Actor::Daemon;
use crate::api::{Mutation, MutationResponse, PreviewMutation};
use piqueld_core::{
    GitBranch, PreviewSlot,
    access::Scope,
    api::{CreatedPreview, EnvironmentAccess, PreviewLimit, SecretAccess},
    manifest::{PreviewLimits, SecretMount, parse_template_toml},
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
async fn concurrent_creations_never_pass_a_preview_limit_and_repeats_never_count() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("db"))
        .await
        .unwrap()
        .with_previews(PreviewLimits {
            max_per_application: 2,
            ..PreviewLimits::default()
        });
    let application = save(&store, false).await;

    let slots = (0..5)
        .map(|agent| format!("agent-{agent}"))
        .collect::<Vec<_>>();
    let results = futures_util::future::join_all(
        slots
            .iter()
            .map(|slot| create(&store, &application, "feat/login", Some(slot))),
    )
    .await;
    let (created, refused): (Vec<_>, Vec<_>) = results.into_iter().partition(Result::is_ok);
    let created = created.into_iter().map(Result::unwrap).collect::<Vec<_>>();
    assert_eq!((created.len(), refused.len()), (2, 3));
    // The error lists the previews counted, so an agent can choose one to delete.
    let Err(StoreError::PreviewLimitReached(reached)) = &refused[0] else {
        panic!("{:?}", refused[0]);
    };
    assert_eq!(
        (reached.limit, reached.max),
        (PreviewLimit::PerApplication, 2)
    );
    let mut listed = reached
        .previews
        .iter()
        .map(|counted| {
            (
                counted.id.clone(),
                counted.preview.slug.to_string(),
                counted.last_deployment.as_ref().unwrap().id.clone(),
            )
        })
        .collect::<Vec<_>>();
    let mut expected = created
        .iter()
        .map(|created| {
            (
                created.preview.id.clone(),
                created.preview.name.to_string(),
                created.operation.operation_id.clone(),
            )
        })
        .collect::<Vec<_>>();
    listed.sort();
    expected.sort();
    assert_eq!(listed, expected);

    // Repeating a creation returns the existing preview, even at the limit.
    let slot = created[0].preview.preview().unwrap().slot.clone().unwrap();
    let again = create(&store, &application, "feat/login", Some(slot.as_str()))
        .await
        .unwrap();
    assert!(!again.created);

    // Lowering a limit keeps every preview and refuses new ones; status
    // shows the overage.
    let lowered = store.clone().with_previews(PreviewLimits {
        max_total: 1,
        ..PreviewLimits::default()
    });
    assert!(
        !create(&lowered, &application, "feat/login", Some(slot.as_str()))
            .await
            .unwrap()
            .created
    );
    assert!(matches!(
        create(&lowered, &application, "feat/other", None).await,
        Err(StoreError::PreviewLimitReached(reached)) if reached.limit == PreviewLimit::Total
    ));
    let usage = lowered.preview_usage(&Scope::All).await.unwrap();
    assert_eq!((usage.total, usage.limits.max_total), (2, 1));
    assert_eq!(usage.applications[0].previews, 2);

    // A preview being deleted counts until it is gone.
    store
        .accept(
            Daemon,
            Mutation::Preview(PreviewMutation::Delete {
                id: created[0].preview.id.clone(),
            }),
            None,
            false,
            None,
        )
        .await
        .unwrap();
    let Err(StoreError::PreviewLimitReached(reached)) =
        create(&store, &application, "feat/other", None).await
    else {
        panic!("a preview being deleted still counts");
    };
    assert!(reached.previews.iter().any(|counted| counted.deleting));
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

#[tokio::test]
async fn pruning_keeps_a_preview_once_the_repository_changed() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("db")).await.unwrap();
    let application = save(&store, false).await;
    let preview = create(&store, &application, "feat/login", None)
        .await
        .unwrap()
        .preview;
    let prune = |repository: &str| {
        Mutation::Preview(PreviewMutation::Prune {
            id: preview.id.clone(),
            repository: repository.into(),
        })
    };
    // The branch was found gone from a repository the application no longer reads.
    assert!(matches!(
        store
            .accept(
                Daemon,
                prune("https://example.com/old.git"),
                None,
                false,
                None
            )
            .await,
        Err(StoreError::IdentityConflict)
    ));
    assert!(!store.get(&preview.id).await.unwrap().delete_intent());
    store
        .accept(
            Daemon,
            prune("https://example.com/notes.git"),
            None,
            false,
            None,
        )
        .await
        .unwrap();
    assert!(store.get(&preview.id).await.unwrap().delete_intent());
}
