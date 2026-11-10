use super::*;
use piqueld::api::{Mutation, MutationResponse};
use piqueld_core::api::{BuildRecord, BuildState};

impl ControllerHarness {
    /// Deploys `environment` again, building every Git source when
    /// `rebuild` is set, and waits for it to succeed.
    async fn redeploy(&self, environment: &EnvironmentId, rebuild: bool) {
        let MutationResponse::Operation(accepted) = self
            .applications()
            .accept(
                Daemon,
                Mutation::Deploy {
                    id: environment.clone(),
                    revision: None,
                    rebuild,
                },
                None,
                true,
                None,
            )
            .await
            .unwrap()
        else {
            panic!("operation expected")
        };
        self.finish(&self.operation(&accepted.operation_id).await)
            .await;
    }

    /// How many images the engine built.
    async fn image_builds(&self) -> u64 {
        self.docker.registry.lock().await.pulls["git-build"]
    }

    /// `environment`'s builds, newest first.
    async fn builds(&self, environment: &EnvironmentId) -> Vec<BuildRecord> {
        self.store
            .builds(
                None,
                Some(environment),
                &piqueld_core::access::Scope::All,
                None,
                50,
            )
            .await
            .unwrap()
            .items
    }
}

#[tokio::test]
async fn a_deployment_reuses_the_image_of_an_unchanged_build_unless_it_rebuilds_or_the_image_is_gone()
 {
    let harness = ControllerHarness::new().await;
    let fixture = git_fixture::GitBuildFixture::new();
    let mut input = manifest();
    input.spec.services[0].source = fixture.source.clone();
    let first = harness.apply(input, Some(0)).await;
    harness.finish(&first).await;
    let environment = &first.environment_id;
    assert_eq!(harness.image_builds().await, 1);

    // The same commit and build settings: nothing is built.
    harness.redeploy(environment, false).await;
    assert_eq!(harness.image_builds().await, 1);
    let builds = harness.builds(environment).await;
    let (reused, built) = (&builds[0], &builds[1]);
    assert_eq!(reused.state, BuildState::Succeeded);
    assert_eq!(reused.reused_build, Some(built.id));
    assert_eq!(
        (built.reused_build, &reused.image_id),
        (None, &built.image_id)
    );
    let output = harness
        .store
        .build_logs(reused.id, None, None)
        .await
        .unwrap();
    assert!(
        output
            .items
            .iter()
            .any(|chunk| chunk.text.contains(&format!("build #{}", built.id)))
    );

    harness.redeploy(environment, true).await;
    assert_eq!(harness.image_builds().await, 2);
    assert_eq!(harness.builds(environment).await[0].reused_build, None);

    // An image cleanup removed, or someone pruned, has to be built again.
    harness.docker.local_images.lock().await.clear();
    harness.redeploy(environment, false).await;
    assert_eq!(harness.image_builds().await, 3);
    assert_eq!(harness.builds(environment).await[0].reused_build, None);
}
