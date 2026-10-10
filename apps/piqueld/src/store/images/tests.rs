use super::*;
use crate::api::Actor::Daemon;
use crate::api::{Mutation, MutationResponse, PreviewMutation};
use crate::store::{Operation, OperationState};
use piqueld_core::{
    ApplicationId, EnvironmentId, GitBranch, InstanceId, ResolutionSet, ResolvedSource,
    ServiceName, compile_application, manifest::parse_template_toml,
};
use std::collections::BTreeMap;

/// The digest of `web`'s image in deployments marked `byte`.
fn image(byte: char) -> ImmutableImage {
    ImmutableImage::parse(format!(
        "ghcr.io/example/notes@sha256:{}",
        byte.to_string().repeat(64)
    ))
    .unwrap()
}

/// Saves the repository-backed `notes` application, so it can have
/// previews, and returns its production environment and a preview.
async fn production_and_preview(store: &Store) -> (EnvironmentId, EnvironmentId) {
    let template = parse_template_toml(
        "api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='notes'\n\
         [spec.manifest]\npath='app.toml'\n[spec.manifest.repository]\nurl='https://example.com/notes.git'\nbranch='main'\n\
         [[spec.services]]\nname='web'\n[spec.services.source]\ntype='image'\nimage='ghcr.io/example/notes:1'",
    )
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
    let application = ApplicationId::parse(saved.application_id).unwrap();
    let create = Mutation::Preview(PreviewMutation::Create {
        application: application.clone(),
        branch: GitBranch::parse("feat/login").unwrap(),
        slot: None,
    });
    let (MutationResponse::Preview(preview), _) = store
        .accept(Daemon, create, None, false, None)
        .await
        .unwrap()
    else {
        panic!("preview")
    };
    (EnvironmentId::default_for(&application), preview.preview.id)
}

/// Deploys `environment` and saves its prepared target, running `web` from
/// [`image`]`(byte)`.
async fn prepare(store: &Store, environment: &EnvironmentId, byte: char) -> Operation {
    let operation = store.request_deploy(environment, None).await.unwrap();
    store
        .transition_operation(
            &operation.id,
            OperationState::Requested,
            OperationState::Running,
            None,
        )
        .await
        .unwrap();
    let application = piqueld_core::parse_toml(include_str!(
        "../../../../../crates/piqueld-core/tests/fixtures/manifests/prebuilt.toml"
    ))
    .unwrap()
    .normalize(ApplicationId::parse("app-notes-test").unwrap());
    let web =
        ResolvedSource::parse_image("ghcr.io/example/notes:1.4.0", image(byte).as_str()).unwrap();
    let target = compile_application(
        &application,
        environment,
        InstanceId::parse(store.instance_id()).unwrap(),
        &ResolutionSet {
            sources: BTreeMap::from([(ServiceName::parse("web").unwrap(), web)]),
            secret_names: BTreeMap::new(),
        },
    )
    .unwrap();
    store.save_prepared(&operation, &target).await.unwrap();
    operation
}

/// Deploys `environment` with [`image`]`(byte)` until it succeeds.
async fn deploy(store: &Store, environment: &EnvironmentId, byte: char) {
    let operation = prepare(store, environment, byte).await;
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

/// The images `root` keeps.
async fn kept(store: &Store, root: RetentionRoot, keep: u32) -> BTreeSet<ImmutableImage> {
    let mut connection = store.pool.acquire().await.unwrap();
    Store::retained_on(&mut connection, root, keep)
        .await
        .unwrap()
        .iter()
        .flat_map(ResolvedApplication::images)
        .collect()
}

#[tokio::test]
async fn each_root_keeps_its_deployments_and_previews_keep_no_history() {
    let temp = tempfile::tempdir().unwrap();
    let store = Store::open(temp.path().join("db")).await.unwrap();
    let (production, preview) = production_and_preview(&store).await;
    for byte in ['a', 'b', 'c'] {
        deploy(&store, &production, byte).await;
    }
    prepare(&store, &production, 'd').await;
    deploy(&store, &preview, 'e').await;
    prepare(&store, &preview, 'f').await;

    let images = |bytes: &str| bytes.chars().map(image).collect::<BTreeSet<_>>();
    assert_eq!(kept(&store, RetentionRoot::Current, 2).await, images("ce"));
    assert_eq!(kept(&store, RetentionRoot::Latest, 2).await, images("df"));
    // The last two successful production deployments; the preview's success
    // counts for nothing, and `d` hasn't succeeded.
    assert_eq!(kept(&store, RetentionRoot::Recent, 2).await, images("bc"));
    assert_eq!(kept(&store, RetentionRoot::Recent, 0).await, images(""));
    assert_eq!(
        kept(&store, RetentionRoot::PromotionSource, 2).await,
        images("")
    );
    // Once another environment promotes from production, what production
    // runs is what promoting it deploys.
    let create = Mutation::CreateEnvironment {
        application: store
            .get(&production)
            .await
            .unwrap()
            .environment
            .application_id,
        name: piqueld_core::EnvironmentName::parse("canary").unwrap(),
        source: Some(crate::api::SourceChoice::PromoteFrom(production.clone())),
    };
    store
        .accept(Daemon, create, None, true, None)
        .await
        .unwrap();
    assert_eq!(
        kept(&store, RetentionRoot::PromotionSource, 2).await,
        images("c")
    );
    assert_eq!(store.retained_images(2).await.unwrap(), images("bcdef"));
    assert_eq!(store.retained_images(3).await.unwrap(), images("abcdef"));
}
