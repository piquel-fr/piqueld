use super::*;
use crate::api::Actor::Daemon;
use crate::api::{Mutation, MutationResponse, PreviewMutation, PromotionMutation, SourceChoice};
use piqueld_core::{ApplicationId, GitBranch, TrackedBranch, manifest::parse_template_toml};

/// Saves the repository-backed application `name`, whose environments
/// follow branches and which may have previews, and returns its ID.
async fn application(store: &Store, name: &str) -> ApplicationId {
    let template = parse_template_toml(&format!(
        "api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='{name}'\n\
         [spec.manifest]\npath='app.toml'\n[spec.manifest.repository]\nurl='https://example.com/{name}.git'\nbranch='main'"
    ))
    .unwrap();
    let (MutationResponse::Saved(saved), _) = store
        .accept(
            Daemon,
            Mutation::save(template, None, false),
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

/// Creates environment `name` of `application`, promoted from `source` when given.
async fn create(
    store: &Store,
    application: &ApplicationId,
    name: &str,
    source: Option<&EnvironmentId>,
) -> Result<EnvironmentId, StoreError> {
    let create = Mutation::CreateEnvironment {
        application: application.clone(),
        name: EnvironmentName::parse(name).unwrap(),
        source: source.map(|source| SourceChoice::PromoteFrom(source.clone())),
    };
    match store.accept(Daemon, create, None, true, None).await? {
        (MutationResponse::Environment(environment), _) => Ok(environment.id),
        _ => panic!("environment expected"),
    }
}

/// Makes `id` promoted from `source`, or tracking again without one.
async fn set_source(
    store: &Store,
    id: &EnvironmentId,
    source: Option<&EnvironmentId>,
) -> Result<(), StoreError> {
    let mutation = Mutation::Promotion(PromotionMutation::SetSource {
        id: id.clone(),
        promote_from: source.cloned(),
    });
    store.accept(Daemon, mutation, None, true, None).await?;
    Ok(())
}

/// The names in a refused source change's cycle.
fn cycle(result: Result<(), StoreError>) -> Vec<String> {
    match result {
        Err(StoreError::Promotion(PromotionError::Cycle { environments })) => environments
            .iter()
            .map(|name| name.as_str().to_owned())
            .collect(),
        other => panic!("cycle expected, got {other:?}"),
    }
}

/// Chains are allowed; cycles, self-promotion, previews, other applications'
/// environments, and environments being deleted are not.
#[tokio::test]
async fn sources_are_other_live_environments_without_cycles() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("db")).await.unwrap();
    let notes = application(&store, "notes").await;
    let production = EnvironmentId::default_for(&notes);
    let staging = create(&store, &notes, "staging", None).await.unwrap();
    let canary = create(&store, &notes, "canary", Some(&staging))
        .await
        .unwrap();
    set_source(&store, &production, Some(&canary))
        .await
        .unwrap();
    assert_eq!(
        store
            .get(&production)
            .await
            .unwrap()
            .environment
            .source
            .promoted_from(),
        Some(&canary)
    );

    assert_eq!(
        cycle(set_source(&store, &staging, Some(&production)).await),
        ["staging", "production", "canary", "staging"]
    );
    assert_eq!(
        cycle(set_source(&store, &staging, Some(&staging)).await),
        ["staging", "staging"]
    );

    let preview = Mutation::Preview(PreviewMutation::Create {
        application: notes.clone(),
        branch: GitBranch::parse("feat/login").unwrap(),
        slot: None,
    });
    let (MutationResponse::Preview(preview), _) = store
        .accept(Daemon, preview, None, false, None)
        .await
        .unwrap()
    else {
        panic!("preview")
    };
    let other = EnvironmentId::default_for(&application(&store, "shop").await);
    let unknown = EnvironmentId::parse("env-unknown-0001").unwrap();
    for source in [&preview.preview.id, &other, &unknown] {
        assert!(matches!(
            set_source(&store, &production, Some(source)).await,
            Err(StoreError::Promotion(PromotionError::SourceInvalid { environment, .. }))
                if environment == *source
        ));
        assert!(matches!(
            create(&store, &notes, "qa", Some(source)).await,
            Err(StoreError::Promotion(PromotionError::SourceInvalid { .. }))
        ));
    }
    // Nothing was changed by the refusals.
    assert_eq!(
        store.get(&staging).await.unwrap().environment.source,
        EnvironmentSource::select(
            store
                .application(&notes)
                .await
                .unwrap()
                .application
                .spec()
                .manifest
                .as_ref(),
            None
        )
        .unwrap()
    );
}

/// A promoted environment never builds, and deploying it says how to
/// promote instead. Tracking again follows the branch `spec.manifest` names,
/// and connecting or disconnecting the repository leaves promotions alone.
#[tokio::test]
async fn a_promoted_environment_never_builds_until_it_tracks_again() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("db")).await.unwrap();
    let notes = application(&store, "notes").await;
    let production = EnvironmentId::default_for(&notes);
    let staging = create(&store, &notes, "staging", None).await.unwrap();
    set_source(&store, &production, Some(&staging))
        .await
        .unwrap();

    for deploy in [
        store
            .accept(
                Daemon,
                Mutation::deploy(production.clone()),
                None,
                true,
                None,
            )
            .await
            .map(|_| ()),
        store.request_deploy(&production, None).await.map(|_| ()),
    ] {
        assert!(matches!(
            deploy,
            Err(StoreError::Promotion(PromotionError::Promoted { environment }))
                if environment.as_str() == "production"
        ));
    }
    assert!(
        store
            .latest_operation_for_environment(&production)
            .await
            .unwrap()
            .is_none()
    );

    set_source(&store, &production, None).await.unwrap();
    let tracking = store.get(&production).await.unwrap().environment.source;
    assert_eq!(tracking.branch().map(TrackedBranch::branch), Some("main"));
    store
        .accept(Daemon, Mutation::deploy(production), None, true, None)
        .await
        .unwrap();
}

/// Deleting an environment others promote from is refused, naming them,
/// until none does; deleting the application deletes them all.
#[tokio::test]
async fn deleting_a_promotion_source_needs_its_dependents_gone() {
    let directory = tempfile::tempdir().unwrap();
    let store = Store::open(directory.path().join("db")).await.unwrap();
    let notes = application(&store, "notes").await;
    let staging = create(&store, &notes, "staging", None).await.unwrap();
    let canary = create(&store, &notes, "canary", Some(&staging))
        .await
        .unwrap();
    let qa = create(&store, &notes, "qa", Some(&staging)).await.unwrap();
    let delete = |id: &EnvironmentId| Mutation::Delete { id: id.clone() };

    match store
        .accept(Daemon, delete(&staging), None, true, None)
        .await
    {
        Err(StoreError::Promotion(PromotionError::InUse { environments })) => assert_eq!(
            environments
                .iter()
                .map(EnvironmentName::as_str)
                .collect::<Vec<_>>(),
            ["canary", "qa"]
        ),
        other => panic!("refusal expected, got {other:?}"),
    }
    store
        .accept(Daemon, delete(&canary), None, true, None)
        .await
        .unwrap();
    set_source(&store, &qa, None).await.unwrap();
    store
        .accept(Daemon, delete(&staging), None, true, None)
        .await
        .unwrap();

    let chained = create(&store, &notes, "review", Some(&qa)).await.unwrap();
    let (MutationResponse::Deleted(deleted), _) = store
        .accept(
            Daemon,
            Mutation::DeleteApplication {
                id: notes,
                environments: ["canary", "production", "qa", "review", "staging"]
                    .map(|name| EnvironmentName::parse(name).unwrap())
                    .into(),
            },
            None,
            true,
            None,
        )
        .await
        .unwrap()
    else {
        panic!("deleted")
    };
    assert!(
        deleted
            .operations
            .iter()
            .any(|operation| operation.environment_id == chained.as_str())
    );
}
