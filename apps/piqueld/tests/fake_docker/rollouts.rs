//! Preparing and rolling out deployments: image resolution, generations, dependencies, recovery and swarm topology changes.

use super::*;

/// An [`ImageSource`] whose references always have the same repo digests.
struct FixedImageSource(Vec<String>);

#[async_trait]
impl ImageSource for FixedImageSource {
    async fn repo_digests(&self, _reference: &str) -> Result<Option<Vec<String>>, DockerError> {
        Ok(Some(self.0.clone()))
    }

    async fn pull(&self, _reference: &str) -> Result<(), DockerError> {
        Ok(())
    }
}

/// Degrades `docker`'s running web service of `id` and waits for the
/// controller to repair it.
async fn assert_active_repair(docker: &FakeDocker, id: &EnvironmentId) {
    {
        let mut observed = docker.observed.lock().await;
        let service = observed
            .services
            .iter_mut()
            .find(|service| service.labels.get(APPLICATION_LABEL) == Some(&id.to_string()))
            .unwrap();
        service.replicas = 9;
        service.convergence = Convergence::Degraded;
    }
    eventually(Duration::from_secs(3), POLL, async || {
        (docker.observe(id).await.unwrap().services[0].replicas == 1).then_some(())
    })
    .await
    .expect("active target is repaired while candidate image pull is blocked");
}

#[tokio::test]
async fn image_resolution_repairs_a_single_tag_flip_through_a_retry() {
    let docker = FakeDocker::default();
    docker.arm_tag_flips(1).await;
    // Pulling a pinned reference leaves the flip armed.
    let pinned = format!("ghcr.io/example/notes@sha256:{}", "c".repeat(64));
    assert_eq!(docker.resolve_image(&pinned).await.unwrap(), pinned);

    let resolved = docker
        .resolve_image("ghcr.io/example/notes:1.4.0")
        .await
        .expect("a single tag flip converges through the bounded retry");

    assert_eq!(
        resolved,
        format!(
            "ghcr.io/example/notes@sha256:{}",
            RegistryState::flipped_digest()
        )
    );
    let registry = docker.registry.lock().await;
    assert_eq!(
        registry.pulls.get("ghcr.io/example/notes:1.4.0"),
        Some(&2),
        "the flipped resolution must retry the whole pull exactly once"
    );
}

#[tokio::test]
async fn image_resolution_preserves_a_requested_digest() {
    let requested = format!("ghcr.io/example/notes@sha256:{}", "b".repeat(64));
    let source = FixedImageSource(vec![
        format!("ghcr.io/example/notes@sha256:{}", "a".repeat(64)),
        requested.clone(),
    ]);

    assert_eq!(
        resolve_image_digest(&source, &requested).await.unwrap(),
        requested
    );
}

#[tokio::test]
async fn image_resolution_fails_sanitized_when_the_tag_never_settles() {
    let docker = FakeDocker::default();
    docker.arm_tag_flips(usize::MAX).await;

    let error = docker
        .resolve_image("ghcr.io/example/notes:1.4.0")
        .await
        .expect_err("an always-flipping tag never converges");

    assert!(matches!(
        error,
        DockerError::ImageResolution("confirm stable image digest")
    ));
}

#[tokio::test]
async fn save_and_deploy_are_durable_before_resolution_and_reconcile_reuses_images() {
    let harness = ControllerHarness::new().await;
    let applications = harness.applications();
    let manifest = manifest();
    let accepted = harness.apply(manifest.clone(), Some(0)).await;
    assert_eq!(harness.pulls().await, 0);
    let stored = harness.store.get(&accepted.environment_id).await.unwrap();
    assert_eq!(stored.application.generation, 1);
    assert!(stored.resolved.is_none());
    harness.finish(&accepted).await;
    let pulls = harness.pulls().await;
    let repeated = harness.save(manifest.clone(), Some(1)).await;
    assert!(repeated.operation_id.is_none());
    assert_eq!(repeated.generation, 2);
    assert_eq!(harness.pulls().await, pulls);
    let reconcile = applications
        .reconcile(&accepted.environment_id, Some(2))
        .await
        .unwrap();
    harness.finish(&reconcile).await;
    assert_eq!(harness.pulls().await, pulls);
    let refresh = applications
        .deploy(&accepted.environment_id, Some(2))
        .await
        .unwrap();
    assert_ne!(refresh.id, accepted.id);
    harness.finish(&refresh).await;
    assert!(harness.pulls().await > pulls);
    assert_eq!(
        harness
            .store
            .get(&accepted.environment_id)
            .await
            .unwrap()
            .application
            .generation,
        2
    );
    let events = harness
        .store
        .events(Some(&accepted.environment_id), None, 100)
        .await
        .unwrap()
        .items;
    assert!(
        events
            .iter()
            .any(|event| event.kind == "operation_succeeded" && event.attempt == Some(1))
    );
    assert!(
        events
            .iter()
            .any(|event| event.kind == "operation_succeeded" && event.attempt == Some(2))
    );
}

#[tokio::test]
async fn generations_protect_full_replacement_and_deletion_without_merging() {
    let harness = ControllerHarness::new().await;
    let applications = harness.applications();
    let mut manifest = manifest();
    let first = harness.apply(manifest.clone(), Some(0)).await;
    manifest.spec.services[0]
        .environment
        .insert("CHANGED".into(), "yes".into());
    let second = harness.apply(manifest.clone(), Some(1)).await;
    assert_eq!(second.generation, 2);
    assert!(matches!(
        applications
            .apply(manifest.clone().validate_template().unwrap(), Some(1))
            .await,
        Err(piqueld::api::ApplicationError::Store(
            piqueld_store::StoreError::GenerationConflict {
                expected: 1,
                actual: 2
            }
        ))
    ));
    assert!(
        applications
            .delete(&first.environment_id, Some(1))
            .await
            .is_err()
    );
    manifest.spec.services[0].environment.clear();
    let third = harness.apply(manifest.clone(), Some(2)).await;
    assert!(
        harness
            .store
            .get(&first.environment_id)
            .await
            .unwrap()
            .manifest()
            .unwrap()
            .spec()
            .services[0]
            .environment
            .is_empty()
    );
    assert_eq!(third.generation, 3);
    let deletion = applications
        .delete(&first.environment_id, Some(3))
        .await
        .unwrap();
    assert_eq!(deletion.generation, 4);
    assert_eq!(
        applications
            .delete(&first.environment_id, None)
            .await
            .unwrap()
            .id,
        deletion.id
    );
    assert!(
        applications
            .deploy(&first.environment_id, None)
            .await
            .is_err()
    );
    assert!(matches!(
        applications
            .apply(manifest.validate_template().unwrap(), Some(4))
            .await,
        Err(piqueld::api::ApplicationError::Store(
            piqueld_store::StoreError::IllegalTransition
        ))
    ));
    let deleting = harness.store.get(&first.environment_id).await.unwrap();
    assert_eq!(deleting.application.generation, 4);
    assert!(deleting.delete_intent());
    assert_eq!(
        harness.operation(&deletion.id).await.state,
        OperationState::Requested
    );
}

#[tokio::test]
async fn periodic_recovery_reuses_failed_prepared_target_and_records_health_changes() {
    let harness = ControllerHarness::new().await;
    let operation = harness.create().await;
    harness
        .store
        .transition_operation(
            &operation.id,
            OperationState::Requested,
            OperationState::Running,
            None,
        )
        .await
        .unwrap();
    harness
        .store
        .transition_operation(
            &operation.id,
            OperationState::Running,
            OperationState::Failed,
            Some(("docker_unavailable", "Docker is unavailable")),
        )
        .await
        .unwrap();
    harness.expire_backoff(&operation).await;
    harness.finish(&operation).await;
    assert_eq!(harness.pulls().await, 0);
    let completed = harness.operation(&operation.id).await;
    assert_eq!(completed.attempt, 2);
    let observed = harness
        .docker
        .observe(&operation.environment_id)
        .await
        .unwrap();
    harness
        .store
        .record_health(&operation.id, &observed)
        .await
        .unwrap();
    harness
        .store
        .record_health(&operation.id, &observed)
        .await
        .unwrap();
    let events = harness.store.events(None, None, 100).await.unwrap().items;
    assert_eq!(
        events
            .iter()
            .filter(
                |event| event.kind == "health_changed" && event.message.as_deref() == Some("ready")
            )
            .count(),
        1
    );
    assert!(
        events
            .iter()
            .any(|event| event.kind == "operation_failed" && event.attempt == Some(1))
    );
}

#[tokio::test]
async fn health_is_degraded_while_an_expected_service_is_missing() {
    let harness = ControllerHarness::new().await;
    let operation = harness.create().await;
    harness.finish(&operation).await;
    let mut observed = harness
        .docker
        .observe(&operation.environment_id)
        .await
        .unwrap();
    // Only an unexpected, converged service runs; the target's is missing.
    observed.services[0].name = "unexpected".into();

    harness
        .store
        .record_health(&operation.id, &observed)
        .await
        .unwrap();

    let status = harness
        .store
        .status(&operation.environment_id)
        .await
        .unwrap();
    assert_eq!(status.runtime_health.as_deref(), Some("degraded"));
    assert!(status.health_observed_at_ms.is_some());
}

#[tokio::test]
async fn pending_pulls_do_not_block_other_apps_and_superseded_preparation_is_discarded() {
    let gate = Arc::new(Gate::default());
    let harness = ControllerHarness::with_docker(FakeDocker {
        resolution_gate: Some(Arc::clone(&gate)),
        ..FakeDocker::default()
    })
    .await;
    let (store, docker, controller) = (harness.store, harness.docker, harness.controller);
    let wake = Arc::new(tokio::sync::Notify::new());
    let applications = TestApplications::new(
        Arc::clone(&store),
        controller.runtime(Arc::clone(&wake)),
        harness.sources,
    );
    let mut slow = manifest();
    slow.metadata.name = "slow".into();
    let original = applications
        .apply(slow.clone().validate_template().unwrap(), None)
        .await
        .unwrap();
    controller.scan(&CancellationToken::new()).await.unwrap();
    let original_target = store
        .get(&original.environment_id)
        .await
        .unwrap()
        .resolved
        .unwrap();
    slow.spec.services[0].source = piqueld_core::Source::Image {
        image: "ghcr.io/example/slow:1".into(),
    };
    let accepted = applications
        .apply(slow.validate_template().unwrap(), None)
        .await
        .unwrap();
    let cancellation = CancellationToken::new();
    let token = cancellation.clone();
    let controller_task = tokio::spawn(async move {
        controller
            .run(wake, Duration::from_millis(50), 10, 30, token)
            .await
    });
    tokio::time::timeout(Duration::from_secs(3), gate.entered.notified())
        .await
        .expect("pull started");
    assert_active_repair(&docker, &accepted.environment_id).await;
    let mut fast = manifest();
    fast.metadata.name = "fast".into();
    let fast = applications
        .apply(fast.validate_template().unwrap(), None)
        .await
        .unwrap();
    eventually(Duration::from_secs(3), POLL, async || {
        (store.operation(&fast.id).await.unwrap().state == OperationState::Succeeded).then_some(())
    })
    .await
    .expect("unrelated application completes while slow pull is pending");
    let pending = store.get(&accepted.environment_id).await.unwrap();
    assert_eq!(pending.application.generation, 2);
    assert_eq!(pending.environment.resolved_generation, Some(1));
    assert_eq!(pending.resolved, Some(original_target));
    assert_eq!(
        docker
            .observe(&accepted.environment_id)
            .await
            .unwrap()
            .services[0]
            .convergence,
        Convergence::Converged
    );
    let deleted = applications
        .delete(&accepted.environment_id, None)
        .await
        .unwrap();
    eventually(Duration::from_secs(3), POLL, async || {
        matches!(
            store.operation(&deleted.id).await,
            Err(piqueld_store::StoreError::NotFound)
        )
        .then_some(())
    })
    .await
    .expect("delete supersedes pending preparation without waiting for its pull");
    assert_eq!(docker.images.maximum.load(Ordering::SeqCst), 2);
    cancellation.cancel();
    controller_task.await.unwrap().unwrap();
}

#[tokio::test]
async fn saving_preserves_active_images_until_deploy_and_rename_preserves_resources() {
    let harness = ControllerHarness::new().await;
    let applications = harness.applications();
    let mut input = manifest();
    let first = harness.apply(input.clone(), Some(0)).await;
    harness.finish(&first).await;
    let pulls = harness.pulls().await;
    let piqueld_core::Source::Image { image } = &input.spec.services[0].source else {
        panic!("expected image fixture")
    };
    harness
        .docker
        .registry
        .lock()
        .await
        .digests
        .insert(image.to_string(), "b".repeat(64));
    input.spec.services[0].replicas = 2.into();
    input.spec.services[0]
        .environment
        .insert("TOKEN".into(), "private-value".into());
    let changed = harness.save(input.clone(), Some(1)).await;
    assert!(changed.operation_id.is_none());
    assert_eq!(harness.pulls().await, pulls);
    let app = harness.store.get(&first.environment_id).await.unwrap();
    assert!(
        app.resolved.unwrap().services[0]
            .image
            .as_str()
            .ends_with(&"a".repeat(64))
    );
    let refreshed = applications
        .deploy(&first.environment_id, Some(2))
        .await
        .unwrap();
    harness.finish(&refreshed).await;
    let before = harness
        .store
        .get(&first.environment_id)
        .await
        .unwrap()
        .resolved
        .unwrap();
    assert!(before.services[0].image.as_str().ends_with(&"b".repeat(64)));
    let renamed = applications
        .accept(
            Daemon,
            piqueld::api::Mutation::Rename {
                id: harness.application_of(&first.environment_id).await,
                name: "renamed".into(),
            },
            Some(2),
            false,
            Some("rename-resources"),
        )
        .await
        .unwrap();
    let piqueld::api::MutationResponse::Rename(renamed) = renamed else {
        panic!("rename response")
    };
    assert_eq!(renamed.generation, 3);
    let after = harness
        .store
        .get(&first.environment_id)
        .await
        .unwrap()
        .resolved
        .unwrap();
    assert_eq!(after.services, before.services);
    assert_eq!(after.networks, before.networks);
    assert_eq!(after.volumes, before.volumes);
    assert_eq!(
        harness
            .store
            .latest_operation_for_environment(&first.environment_id)
            .await
            .unwrap()
            .unwrap()
            .id,
        refreshed.id
    );
    let events = harness.store.events(None, None, 100).await.unwrap().items;
    assert!(events.iter().any(|event| event.kind == "resource_mutated"
        && event.phase.as_deref() == Some("ensure_service")
        && event.resource.as_deref() == Some(after.services[0].name.as_str())));
    assert!(
        !serde_json::to_string(&events)
            .unwrap()
            .contains("private-value")
    );
}

#[tokio::test]
async fn failed_preparation_preserves_active_repair_and_save_does_not_retry() {
    let harness = ControllerHarness::new().await;
    let applications = harness.applications();
    let mut input = manifest();
    let first = harness.apply(input.clone(), None).await;
    harness.finish(&first).await;
    input.spec.services[0].source = piqueld_core::Source::Image {
        image: "ghcr.io/example/unstable:1".into(),
    };
    harness.docker.arm_tag_flips(100).await;
    let failed = harness.apply(input.clone(), None).await;
    harness.scan().await;
    let failure = harness.operation(&failed.id).await;
    assert_eq!(failure.state, OperationState::Failed);
    assert_eq!(failure.phase.as_deref(), Some("resolving_image"));
    assert_eq!(failure.resource.as_deref(), Some("web"));
    let pulls = harness.pulls().await;
    let response = applications
        .accept(
            Daemon,
            piqueld::api::Mutation::save(
                input.validate_template().unwrap(),
                Some(
                    harness
                        .application_of(&first.environment_id)
                        .await
                        .to_string(),
                ),
                false,
            ),
            Some(failed.generation),
            false,
            None,
        )
        .await
        .unwrap();
    let piqueld::api::MutationResponse::Saved(saved) = response else {
        panic!("saved configuration")
    };
    assert!(saved.operation_id.is_none());
    assert_eq!(harness.operation(&failed.id).await, failure);
    assert_eq!(harness.pulls().await, pulls);
    {
        let mut observed = harness.docker.observed.lock().await;
        observed.services[0].replicas = 9;
        observed.services[0].convergence = Convergence::Degraded;
    }
    harness.scan().await;
    assert_eq!(harness.docker.observed.lock().await.services[0].replicas, 1);
    let stored = harness.store.get(&first.environment_id).await.unwrap();
    assert_eq!(stored.environment.resolved_generation, Some(1));
    let events = harness.store.events(None, None, 100).await.unwrap().items;
    assert!(events.iter().any(|event| event.kind == "operation_failed"
        && event.error_code.as_deref() == Some("image_resolution_failed")
        && event.resource.as_deref() == Some("web")));
}

#[tokio::test]
async fn convergence_timeout_closes_the_waiting_action_as_failed() {
    let mut harness = ControllerHarness::new().await;
    harness
        .docker
        .stall_convergence
        .store(true, Ordering::SeqCst);
    // The deadline covers network, volume and service creation before the wait
    // starts. Leave headroom so a loaded runner still reaches the waiting action.
    harness.set_convergence_timeout(Duration::from_secs(3));
    let created = harness.create().await;
    harness.scan().await;
    let events = harness.operation_events(&created.id).await;
    assert!(
        events.iter().any(|event| event.kind == "action_failed"
            && event.error_code.as_deref() == Some("convergence_timeout")
            && event.duration_ms.is_some()),
        "{events:#?}"
    );
    assert!(
        !events
            .iter()
            .any(|event| event.kind == "action_outcome_unknown")
    );
}

/// Deploys image services given as `(name, depends_on)`, where each `slow`
/// service stays updating for that many observations after its ensure.
/// Returns the operation's final state and Docker's rollout log.
async fn deploy_dependents(
    services: &[(&str, &[&str])],
    slow: &[(&str, usize)],
    convergence_timeout: Duration,
) -> (OperationState, Vec<String>) {
    use std::fmt::Write;
    let mut manifest = String::from(
        "api_version = \"piqueld.dev/v1alpha1\"\nkind = \"Application\"\n[metadata]\nname = \"notes\"\n",
    );
    for (name, depends_on) in services {
        write!(
            manifest,
            "[[spec.services]]\nname = \"{name}\"\ndepends_on = {depends_on:?}\n[spec.services.source]\ntype = \"image\"\nimage = \"{}\"\n",
            prebuilt::IMAGE
        )
        .unwrap();
    }
    let mut harness = ControllerHarness::new().await;
    harness.set_application(
        piqueld_core::parse_toml(&manifest)
            .unwrap()
            .normalize(ApplicationId::parse("app-dependents-01").unwrap()),
    );
    harness.docker.slow_services.lock().await.extend(
        slow.iter()
            .map(|(name, observations)| ((*name).to_owned(), *observations)),
    );
    harness.set_convergence_timeout(convergence_timeout);
    let created = harness.create().await;
    harness.scan().await;
    let state = harness.operation(&created.id).await.state;
    (state, harness.docker.rollout.lock().await.clone())
}

/// A service that disappears while the deployment waits for it fails the
/// attempt, which is retried, instead of waiting out the convergence timeout.
#[tokio::test]
async fn a_service_removed_while_waiting_for_it_fails_the_attempt() {
    let harness = ControllerHarness::new().await;
    harness
        .docker
        .stall_convergence
        .store(true, Ordering::SeqCst);
    let operation = harness.create().await;
    let remove = async {
        eventually(Duration::from_secs(10), POLL, async || {
            (harness.operation(&operation.id).await.phase.as_deref() == Some("wait_for_service"))
                .then_some(())
        })
        .await
        .expect("the deployment waits for its service");
        harness.docker.observed.lock().await.services.clear();
    };
    tokio::time::timeout(Duration::from_secs(10), async {
        tokio::join!(harness.scan(), remove)
    })
    .await
    .expect("the wait ends once its service is gone");
    let failed = harness.operation(&operation.id).await;
    assert_eq!(failed.state, OperationState::Failed);
    assert_eq!(failed.error_code.as_deref(), Some("docker_request_failed"));
}

#[tokio::test]
async fn dependents_roll_out_once_their_dependencies_converge() {
    // `worker` is independent and sorts after the dependent `server`.
    let rollout = deploy_dependents(
        &[("db", &[]), ("server", &["db"]), ("worker", &[])],
        &[("db", 3)],
        Duration::from_mins(2),
    )
    .await;
    assert_eq!(
        rollout,
        (
            OperationState::Succeeded,
            vec![
                "ensure db".to_owned(),
                "ensure worker".to_owned(),
                "converged db".to_owned(),
                "ensure server".to_owned(),
            ]
        )
    );
}

#[tokio::test]
async fn each_dependency_gets_the_full_convergence_timeout() {
    // Each link converges in about 1.75s of backed-off polls: well within
    // the 4s timeout alone, but the chain as a whole takes longer.
    let rollout = deploy_dependents(
        &[("db", &[]), ("api", &["db"]), ("web", &["api"])],
        &[("db", 5), ("api", 5), ("web", 5)],
        Duration::from_secs(4),
    )
    .await;
    assert_eq!(
        rollout,
        (
            OperationState::Succeeded,
            ["db", "api", "web"]
                .iter()
                .flat_map(|name| [format!("ensure {name}"), format!("converged {name}")])
                .collect()
        )
    );
}

#[tokio::test]
async fn git_deploy_prepares_before_rollout_and_supersedes_pending_requests() {
    use piqueld::api::{Mutation, MutationResponse};
    let harness = ControllerHarness::new().await;
    let applications = harness.applications();
    let mut input = manifest();
    let fixture = git_fixture::GitBuildFixture::new();
    input.spec.services[0].source = fixture.source.clone();
    let first = harness.apply(input.clone(), Some(0)).await;
    let deploy = || Mutation::deploy(first.environment_id.clone());
    let MutationResponse::Operation(replacement) = applications
        .accept(Daemon, deploy(), None, true, None)
        .await
        .unwrap()
    else {
        panic!("deployment")
    };
    assert_eq!(
        harness.operation(&first.id).await.state,
        OperationState::Superseded
    );
    let replacement = harness.operation(&replacement.operation_id).await;
    harness.finish(&replacement).await;
    let active = harness
        .store
        .get(&first.environment_id)
        .await
        .unwrap()
        .resolved
        .unwrap();
    assert!(
        matches!(&active.services[0].source, ResolvedSource::Git { commit, .. } if piqueld_core::manifest::valid_git_commit(commit))
    );
    let pulls = harness.pulls().await;
    let keyed = || {
        let actor = Daemon;
        applications.accept(actor, deploy(), None, true, Some("git-deploy"))
    };
    let MutationResponse::Operation(accepted) = keyed().await.unwrap() else {
        panic!("expected operation")
    };
    let MutationResponse::Operation(replay) = keyed().await.unwrap() else {
        panic!("expected operation")
    };
    assert_eq!(accepted.operation_id, replay.operation_id);
    let operation = harness.operation(&accepted.operation_id).await;
    harness.finish(&operation).await;
    // It prepared again, and reused the image of the unchanged commit.
    assert_eq!(harness.pulls().await, pulls);
    input.spec.services[0].source = fixture.failing_source();
    let failed = harness.apply(input, Some(1)).await;
    harness.scan().await;
    let failure = harness.operation(&failed.id).await;
    assert_eq!(failure.state, OperationState::Failed);
    assert_eq!(failure.phase.as_deref(), Some("building_git"));
    assert_eq!(failure.resource.as_deref(), Some("web"));
    assert_eq!(failure.error_code.as_deref(), Some("git_build_failed"));
    assert_eq!(
        harness
            .store
            .get(&first.environment_id)
            .await
            .unwrap()
            .resolved
            .unwrap(),
        active
    );
    harness
        .assert_git_build_history(&first.environment_id)
        .await;
}

#[tokio::test]
async fn mixed_sources_report_the_failing_source() {
    for git_fails in [false, true] {
        let harness = ControllerHarness::new().await;
        let fixture = git_fixture::GitBuildFixture::new();
        let mut input = manifest();
        let mut git_service = input.spec.services[0].clone();
        git_service.name = "git-service".into();
        if git_fails {
            git_service.source = fixture.failing_source();
        } else {
            git_service.source = fixture.source.clone();
            input.spec.services[0].source = piqueld_core::Source::Image {
                image: "ghcr.io/example/unstable:1".into(),
            };
            harness.docker.arm_tag_flips(100).await;
        }
        input.spec.services.push(git_service);
        let operation = harness.apply(input, Some(0)).await;
        harness.scan().await;
        let failure = harness.operation(&operation.id).await;
        assert_eq!(failure.state, OperationState::Failed);
        let (phase, service, code) = if git_fails {
            ("building_git", "git-service", "git_build_failed")
        } else {
            ("resolving_image", "web", "image_resolution_failed")
        };
        assert_eq!(failure.phase.as_deref(), Some(phase));
        assert_eq!(failure.resource.as_deref(), Some(service));
        assert_eq!(failure.error_code.as_deref(), Some(code));
    }
}

#[tokio::test]
async fn preparation_timeout_is_retried_after_backoff() {
    let gate = Arc::new(Gate::default());
    let mut harness = ControllerHarness::with_docker(FakeDocker {
        resolution_gate: Some(Arc::clone(&gate)),
        ..FakeDocker::default()
    })
    .await;
    harness.controller = harness
        .new_controller()
        .with_prepare_timeout(Duration::from_millis(100));
    let mut manifest = manifest();
    manifest.spec.services[0].source = piqueld_core::Source::Image {
        image: "ghcr.io/example/slow:1".into(),
    };
    let operation = harness.apply(manifest, None).await;
    harness.scan().await;
    let failed = harness.operation(&operation.id).await;
    assert_eq!(failed.state, OperationState::Failed);
    assert_eq!(failed.error_code.as_deref(), Some("preparation_timeout"));
    assert!(
        harness
            .store
            .prepared_target(&operation.id)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        harness.docker.observed.lock().await.services,
        [] as [ObservedService; 0]
    );
    harness.expire_backoff(&operation).await;
    harness.controller = harness.new_controller();
    gate.release.notify_one();
    harness.scan().await;
    let latest = harness
        .store
        .latest_operation_for_environment(&operation.environment_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(latest.state, OperationState::Succeeded);
    assert_eq!(harness.docker.observed.lock().await.services.len(), 1);
}

#[tokio::test]
async fn changed_swarm_topology_blocks_preparation_and_recovers_after_backoff() {
    let harness = ControllerHarness::new().await;
    harness
        .docker
        .incompatible_swarm
        .store(true, Ordering::SeqCst);
    let operation = harness.apply(manifest(), None).await;
    harness.scan().await;
    assert_eq!(
        harness.operation(&operation.id).await.error_code.as_deref(),
        Some("swarm_topology_unsupported")
    );
    assert!(harness.docker.registry.lock().await.pulls.is_empty());
    assert_eq!(
        harness.docker.observed.lock().await.networks,
        [] as [ObservedNetwork; 0]
    );
    harness
        .docker
        .incompatible_swarm
        .store(false, Ordering::SeqCst);
    harness.expire_backoff(&operation).await;
    harness.scan().await;
    let latest = harness
        .store
        .latest_operation_for_environment(&operation.environment_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(latest.state, OperationState::Succeeded);
}

#[tokio::test]
async fn topology_change_during_preparation_blocks_promotion_and_mutation() {
    let gate = Arc::new(Gate::default());
    let harness = ControllerHarness::with_docker(FakeDocker {
        resolution_gate: Some(Arc::clone(&gate)),
        ..FakeDocker::default()
    })
    .await;
    let mut manifest = manifest();
    manifest.spec.services[0].source = piqueld_core::Source::Image {
        image: "ghcr.io/example/slow:1".into(),
    };
    let operation = harness.apply(manifest, None).await;
    let cancellation = CancellationToken::new();
    let scan = harness.controller.scan(&cancellation);
    let change_topology = async {
        gate.entered.notified().await;
        harness
            .docker
            .incompatible_swarm
            .store(true, Ordering::SeqCst);
        gate.release.notify_one();
    };
    let (result, ()) = tokio::join!(scan, change_topology);
    result.unwrap();
    assert_eq!(
        harness.operation(&operation.id).await.error_code.as_deref(),
        Some("swarm_topology_unsupported")
    );
    assert!(
        harness
            .store
            .get(&operation.environment_id)
            .await
            .unwrap()
            .resolved
            .is_none()
    );
    assert_eq!(
        harness.docker.observed.lock().await.networks,
        [] as [ObservedNetwork; 0]
    );
}

#[tokio::test]
async fn deletion_proceeds_after_another_node_joins() {
    let harness = ControllerHarness::new().await;
    harness.create().await;
    harness.scan().await;
    harness
        .docker
        .incompatible_swarm
        .store(true, Ordering::SeqCst);
    let deletion = harness.delete().await;
    harness.scan().await;
    harness.assert_deleted(&deletion).await;
}

/// Deletion waits for Swarm to release the network a container just left,
/// even beyond the attempts other failed requests get.
#[tokio::test]
async fn deletion_waits_for_swarm_to_release_the_network() {
    let mut harness = ControllerHarness::new().await;
    harness.set_retry_policy(piqueld::reconcile::RetryPolicy {
        attempts: 2,
        initial_delay: Duration::from_millis(10),
        max_delay: Duration::from_millis(10),
        ..piqueld::reconcile::RetryPolicy::default()
    });
    harness.create().await;
    harness.scan().await;
    harness.docker.releasing_networks.store(5, Ordering::SeqCst);
    let deletion = harness.delete().await;
    harness.scan().await;
    harness.assert_deleted(&deletion).await;
    assert_eq!(harness.docker.releasing_networks.load(Ordering::SeqCst), 0);
}

/// Restoring an earlier deployment rolls out its image again, pulled again
/// by digest once removed, without running its jobs again.
#[tokio::test]
async fn restoring_a_deployment_runs_its_image_again_without_jobs() {
    let harness = ControllerHarness::new().await;
    let first = harness.apply(migration(), None).await;
    harness.finish(&first).await;
    let environment = first.environment_id.clone();
    let image = async || {
        harness.target(&environment).await.services[0]
            .image
            .to_string()
    };
    let original = image().await;
    // The tag now resolves to another image.
    harness
        .docker
        .registry
        .lock()
        .await
        .digests
        .insert("ghcr.io/example/notes:1.4.0".into(), "b".repeat(64));
    let second = harness
        .applications()
        .deploy(&environment, None)
        .await
        .unwrap();
    harness.finish(&second).await;
    assert_ne!(image().await, original);
    assert_eq!(harness.started_jobs().await, ["migrate", "migrate"]);

    // Cleanup or an operator removed the image meanwhile.
    harness.docker.local_images.lock().await.clear();
    let pulls = harness.pulls().await;
    let accepted = harness
        .applications()
        .restore(
            Daemon,
            piqueld::api::Restore::Environment {
                id: environment.clone(),
                deployment: first.id.clone(),
            },
            None,
        )
        .await
        .unwrap();
    assert_eq!(harness.pulls().await, pulls + 1);
    harness
        .finish(&harness.operation(&accepted.operation_id).await)
        .await;
    assert_eq!(image().await, original);
    assert_eq!(harness.started_jobs().await, ["migrate", "migrate"]);
}
