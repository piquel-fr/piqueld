use super::*;
use piqueld::reconcile::ImagesInUse;
use piqueld_core::{ServiceName, api::ImageStatus};

/// A local image with ID `byte`s and `labels`.
fn image(byte: char, labels: &[(&str, &str)]) -> LocalImage {
    LocalImage {
        id: Sha256Digest::parse(format!("sha256:{}", byte.to_string().repeat(64))).unwrap(),
        repo_digests: Vec::new(),
        labels: labels
            .iter()
            .map(|(key, value)| ((*key).into(), (*value).into()))
            .collect(),
        size: 10,
        used: false,
    }
}

impl ControllerHarness {
    /// Deploys the fixture with `web` built from Git, as image `b`s.
    async fn deploy_git_build(&self, fixture: &git_fixture::GitBuildFixture) -> Operation {
        let mut input = manifest();
        input.spec.services[0].source = fixture.source.clone();
        self.apply(input, Some(0)).await
    }

    /// IDs of the images left in the engine.
    async fn image_ids(&self) -> Vec<char> {
        self.docker
            .local_images
            .lock()
            .await
            .iter()
            .map(|image| image.id.as_str().chars().last().unwrap())
            .collect()
    }
}

#[tokio::test]
async fn cleanup_removes_only_this_installations_unretained_builds() {
    let harness = ControllerHarness::new().await;
    let fixture = git_fixture::GitBuildFixture::new();
    let operation = harness.deploy_git_build(&fixture).await;
    harness.finish(&operation).await;
    let ours = harness.store.instance_id().to_owned();
    let owned = [(MANAGED_LABEL, "true"), (INSTANCE_LABEL, ours.as_str())];
    let used = LocalImage {
        used: true,
        ..image('f', &owned)
    };
    harness.docker.local_images.lock().await.extend([
        image('c', &owned),
        // Another installation or development instance on the same engine.
        image(
            'd',
            &[(MANAGED_LABEL, "true"), (INSTANCE_LABEL, "instance-other")],
        ),
        // Built by hand, or by piqueld before builds were labelled.
        image('e', &[]),
        used,
    ]);

    assert!(harness.controller.clean_images().await.unwrap());
    // `b` runs in production; `f` is a stopped container's.
    assert_eq!(harness.image_ids().await, ['b', 'd', 'e', 'f']);
    let status = harness.controller.image_status().borrow().clone();
    assert_eq!((status.images, status.reclaimed_bytes), (2, 10));
    let events = harness.store.events(None, None, 100).await.unwrap().items;
    let removed = events
        .iter()
        .filter(|event| {
            event.phase.as_deref() == Some("remove_image") && event.kind == "action_succeeded"
        })
        .filter_map(|event| event.resource.as_deref())
        .collect::<Vec<_>>();
    assert_eq!(removed, [image('c', &[]).id.as_str()]);
}

#[tokio::test]
async fn an_image_built_for_a_deployment_in_preparation_survives_cleanup() {
    let gate = Arc::new(Gate::default());
    let harness = ControllerHarness::with_docker(FakeDocker {
        build_gate: Some(Arc::clone(&gate)),
        ..FakeDocker::default()
    })
    .await;
    let fixture = git_fixture::GitBuildFixture::new();
    let operation = harness.deploy_git_build(&fixture).await;
    let cancellation = CancellationToken::new();
    let scan = harness.controller.scan(&cancellation);
    tokio::pin!(scan);
    tokio::select! {
        () = gate.entered.notified() => {}
        result = &mut scan => panic!("scan finished before building: {result:?}"),
    }
    // Built, but no retention root records it yet: cleanup waits.
    assert_eq!(harness.image_ids().await, ['b']);
    assert!(!harness.controller.clean_images().await.unwrap());
    assert_eq!(harness.image_ids().await, ['b']);
    assert_eq!(
        harness.controller.image_status().borrow().clone(),
        ImageStatus::default()
    );

    gate.release.notify_one();
    scan.await.unwrap();
    assert_eq!(
        harness.operation(&operation.id).await.state,
        OperationState::Succeeded
    );
    assert!(harness.controller.clean_images().await.unwrap());
    assert_eq!(harness.image_ids().await, ['b']);
}

#[tokio::test]
async fn missing_images_are_pulled_again_by_digest_or_reported_unavailable() {
    let harness = ControllerHarness::new().await;
    let in_use: ImagesInUse = harness.controller.images_in_use().await;
    let web = ServiceName::parse("web").unwrap();
    let pulled = prebuilt::resolutions(&harness.application).sources[&web].clone();
    let digest = pulled.repository_digest().unwrap().to_string();

    in_use
        .ensure(&*harness.docker, [(&web, &pulled)])
        .await
        .unwrap();
    assert_eq!(harness.docker.registry.lock().await.pulls[&digest], 1);

    let api = ServiceName::parse("api").unwrap();
    let built = ResolvedSource::Git {
        requested: piqueld_core::manifest::ValidatedSource::Image {
            image: "unused".into(),
        },
        commit: "a".repeat(40),
        image_id: image('c', &[]).id,
    };
    let error = in_use
        .ensure(&*harness.docker, [(&web, &pulled), (&api, &built)])
        .await
        .unwrap_err();
    assert_eq!(error.code(), "image_unavailable");
    assert_eq!(
        error.message(),
        format!(
            "service api's image sha256:{} is no longer present, and a build can't be pulled again",
            "c".repeat(64)
        )
    );
    // `web` was present this time.
    assert_eq!(harness.docker.registry.lock().await.pulls[&digest], 1);
}
