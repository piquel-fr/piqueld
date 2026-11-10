//! Deployment jobs: dependencies, the services they stop, timeouts, cleanup and recovery across restarts.

use super::*;

/// A migration whose database depends on storage, plus an unrelated service.
fn manifest_with_job_dependencies() -> ApplicationManifest {
    let mut input = migration();
    input.spec.services[0].depends_on = vec!["db".into()];
    for (name, dependencies) in [
        ("db", vec!["storage".into()]),
        ("storage", vec![]),
        ("other", vec![]),
    ] {
        let mut service = input.spec.services[0].clone();
        service.name = name.into();
        service.depends_on = dependencies;
        service.mounts.clear();
        service.healthcheck = Some(piqueld_core::manifest::HealthCheck::Command {
            command: vec!["ready".into()],
            interval_seconds: 1.into(),
            timeout_seconds: 1.into(),
            start_period_seconds: 0.into(),
            retries: 3.into(),
        });
        input.spec.services.push(service);
    }
    input
}

/// The fixture with one exclusive migration of `web`, which mounts `data`.
fn manifest_with_exclusive_job() -> ApplicationManifest {
    let mut input = migration();
    input.spec.jobs[0].volume_access = piqueld_core::manifest::JobVolumeAccess::Exclusive;
    input
}

/// The fixture with one migration of `web` that stops `web` before it runs,
/// once as an exclusive job and once by stopping its own service.
fn manifests_with_web_stopping_job() -> [ApplicationManifest; 2] {
    let mut own = migration();
    own.spec.jobs[0].stop_service = true;
    [manifest_with_exclusive_job(), own]
}

#[tokio::test]
async fn job_dependencies_converge_before_migration_and_other_services() {
    let mut harness = ControllerHarness::new().await;
    harness.set_convergence_timeout(Duration::from_secs(3));
    harness
        .docker
        .slow_services
        .lock()
        .await
        // Each dependency waits about 1.75s of backed-off polls, with room
        // for journal I/O. Their combined wait still exceeds the three-second
        // budget.
        .extend([("storage".into(), 5), ("db".into(), 5)]);
    let mut input = manifest_with_job_dependencies();
    // A prerequisite's own job must succeed before that service is started.
    let mut storage_job = input.spec.jobs[0].clone();
    storage_job.name = "initialize-storage".into();
    storage_job.service = "storage".into();
    input.spec.jobs.insert(0, storage_job);
    let operation = harness.apply(input, None).await;
    harness.finish(&operation).await;
    assert_eq!(
        *harness.docker.job_starts.lock().await,
        [
            JobStart {
                job: "initialize-storage".into(),
                services: 0,
                networks: 1,
                volumes: 1
            },
            JobStart {
                job: "migrate".into(),
                services: 2,
                networks: 1,
                volumes: 1
            },
        ]
    );
    let rollout = harness.docker.rollout.lock().await;
    assert_eq!(
        &rollout[..6],
        [
            "job initialize-storage",
            "ensure storage",
            "converged storage",
            "ensure db",
            "converged db",
            "job migrate"
        ]
    );
    assert!(rollout[6..].contains(&"ensure web".to_owned()));
    assert!(rollout[6..].contains(&"ensure other".to_owned()));
}

#[tokio::test]
async fn jobs_of_a_deployment_stopped_before_them_still_reach_their_dependencies() {
    let harness = ControllerHarness::new().await;
    let operation = harness.apply(manifest_with_job_dependencies(), None).await;
    let id = &operation.environment_id;
    harness.applications().stop(id).await.unwrap();
    harness.finish(&operation).await;
    assert_eq!(
        harness.docker.job_starts.lock().await[0].services,
        2,
        "the migration's dependencies run"
    );
    let observed = harness.docker.observe(id).await.unwrap();
    assert!(
        observed
            .services
            .iter()
            .all(|service| service.replicas == 0)
    );
}

#[tokio::test]
async fn failed_or_timed_out_job_dependencies_prevent_jobs_and_other_services() {
    for failed in [false, true] {
        let mut harness = ControllerHarness::new().await;
        harness.set_convergence_timeout(Duration::from_secs(3));
        harness
            .docker
            .stall_convergence
            .store(!failed, Ordering::SeqCst);
        harness
            .docker
            .fail_convergence
            .store(failed, Ordering::SeqCst);
        let operation = harness.apply(manifest_with_job_dependencies(), None).await;
        assert_eq!(
            harness.scan_result(&operation).await,
            (
                OperationState::Failed,
                Some(
                    if failed {
                        "service_update_failed"
                    } else {
                        "convergence_timeout"
                    }
                    .into()
                )
            )
        );
        assert_eq!(harness.started_jobs().await, [] as [String; 0]);
        assert_eq!(*harness.docker.rollout.lock().await, ["ensure storage"]);
        assert!(
            harness
                .store
                .get(&operation.environment_id)
                .await
                .unwrap()
                .resolved
                .is_none()
        );
    }
}

#[tokio::test]
async fn failed_migration_rolls_back_prerequisites_and_repairs_active_services() {
    for existing_dependencies in [false, true] {
        let harness = ControllerHarness::new().await;
        let mut initial = manifest_with_job_dependencies();
        initial.spec.jobs.clear();
        if !existing_dependencies {
            initial.spec.services[0].depends_on.clear();
            initial
                .spec
                .services
                .retain(|service| matches!(service.name.as_str(), "web" | "other"));
        }
        let first = harness.apply(initial, None).await;
        harness.finish(&first).await;
        let mut replacement = manifest_with_job_dependencies();
        for service in &mut replacement.spec.services {
            if matches!(service.name.as_str(), "web" | "db") {
                service.replicas = 2.into();
            }
        }
        harness.docker.job_exit.store(3, Ordering::SeqCst);
        let operation = harness.apply(replacement, None).await;
        assert_eq!(
            harness.scan_result(&operation).await,
            (OperationState::Failed, Some("job_failed".into()))
        );
        let replicas = |observed: &ObservedApplication, name: &str| {
            observed
                .services
                .iter()
                .find(|service| service.labels[SERVICE_LABEL] == name)
                .map(|service| service.replicas)
        };
        // The job ran against the updated prerequisites.
        assert_eq!(
            replicas(&*harness.docker.observed.lock().await, "db"),
            Some(2)
        );
        {
            let mut observed = harness.docker.observed.lock().await;
            for service in &mut observed.services {
                if matches!(service.labels[SERVICE_LABEL].as_str(), "web" | "other") {
                    service.replicas = 9;
                }
            }
        }
        // The failed deployment ended, so repair returns prerequisites to
        // the active target, alongside the other active services.
        for _ in 0..8 {
            assert_eq!(
                harness.scan_result(&operation).await,
                (OperationState::Failed, Some("job_failed".into()))
            );
        }
        let observed = harness.docker.observed.lock().await;
        let previous = existing_dependencies.then_some(1);
        assert_eq!(replicas(&observed, "db"), previous);
        assert_eq!(replicas(&observed, "storage"), previous);
        assert_eq!(replicas(&observed, "web"), Some(1));
        assert_eq!(replicas(&observed, "other"), Some(1));
        assert_eq!(harness.started_jobs().await, ["migrate"]);
        assert_eq!(
            harness
                .store
                .get(&operation.environment_id)
                .await
                .unwrap()
                .environment
                .resolved_generation,
            Some(1)
        );
    }
}

/// A deployment whose own plan a conflict blocks has ended until an
/// operator resolves it, so repair returns its job prerequisites to the
/// active target. A conflict planning can't see is retried at once instead,
/// so its prerequisites stay rather than being rolled back and forth.
#[tokio::test]
async fn a_deployment_blocked_by_a_conflict_rolls_back_its_job_prerequisites() {
    let harness = ControllerHarness::new().await;
    let mut initial = manifest_with_job_dependencies();
    initial.spec.jobs.clear();
    initial.spec.services[0].depends_on.clear();
    initial
        .spec
        .services
        .retain(|service| matches!(service.name.as_str(), "web" | "other"));
    let first = harness.apply(initial, None).await;
    harness.finish(&first).await;
    let mut replacement = manifest_with_job_dependencies();
    let mut extra = replacement.spec.services[3].clone();
    extra.name = "extra".into();
    replacement.spec.services.push(extra);
    *harness.docker.conflicting_job.lock().await = Some("migrate".into());
    let operation = harness.apply(replacement, None).await;
    for _ in 0..3 {
        assert_eq!(
            harness.scan_result(&operation).await,
            (OperationState::Failed, Some("ownership_conflict".into()))
        );
        assert_eq!(
            harness.owned_services().await,
            ["db", "other", "storage", "web"]
        );
    }

    // A foreign service takes the name of the one the deployment adds.
    let foreign = harness.add_foreign_service(&operation, "extra").await;
    for _ in 0..8 {
        harness.scan().await;
    }
    assert_eq!(harness.owned_services().await, ["other", "web"]);
    assert_eq!(harness.started_jobs().await, [] as [String; 0]);

    // Once the conflict is resolved, the deployment rolls out after all.
    harness.remove_foreign_service(&foreign).await;
    *harness.docker.conflicting_job.lock().await = None;
    harness.finish(&operation).await;
    assert_eq!(
        harness.owned_services().await,
        ["db", "extra", "other", "storage", "web"]
    );
    assert_eq!(harness.started_jobs().await, ["migrate"]);
}

/// A deployment adding an import keeps the link network its job
/// prerequisites created while repair maintains the active target, which
/// links nothing: creating it asks for a scan, and removing it would fail
/// the next service joining it.
#[tokio::test]
async fn job_prerequisites_keep_the_link_network_of_a_new_import() {
    let harness = ControllerHarness::new().await;
    let first = harness.apply(manifest(), None).await;
    harness.finish(&first).await;
    let auth = harness
        .deploy_application("auth", |manifest| {
            manifest.spec.exports.push(piqueld_core::manifest::Export {
                service: "web".into(),
                applications: vec!["notes".into()],
                previews: false,
            });
        })
        .await;
    harness.scan().await;
    let mut replacement = migration();
    replacement
        .spec
        .imports
        .push(piqueld_core::manifest::Import {
            application: "auth".into(),
            service: "web".into(),
            environment: None,
        });
    // The job fails as a conflict planning can't see, so the deployment
    // may still roll out while scans repair the active target.
    *harness.docker.conflicting_job.lock().await = Some("migrate".into());
    let operation = harness.apply(replacement, None).await;
    let network = piqueld_core::DockerNetworkName::for_link(&operation.environment_id, &auth);
    for _ in 0..3 {
        assert_eq!(
            harness.scan_result(&operation).await,
            (OperationState::Failed, Some("ownership_conflict".into()))
        );
    }
    assert!(
        !harness
            .operation_events(&operation.id)
            .await
            .iter()
            .any(|event| event.phase.as_deref() == Some("remove_network")
                && event.resource.as_deref() == Some(network.as_str())),
        "repair removed the link network"
    );
    *harness.docker.conflicting_job.lock().await = None;
    harness.finish(&operation).await;
}

/// A conflict blocking a deployment after a job that stops services
/// succeeded keeps them stopped, as they would run against data in the new
/// format. The deployment continues once the conflict is resolved.
#[tokio::test]
async fn a_conflict_after_a_job_stopping_services_succeeded_keeps_them_stopped() {
    for replacement in manifests_with_web_stopping_job() {
        a_conflict_after_the_job_succeeded_keeps_web_stopped(replacement).await;
    }
}

/// Deploys `replacement`, whose job stops `web`, and blocks it by a conflict
/// once the job runs.
async fn a_conflict_after_the_job_succeeded_keeps_web_stopped(
    mut replacement: ApplicationManifest,
) {
    let harness = ControllerHarness::new().await;
    let first = harness.apply(manifest(), None).await;
    harness.finish(&first).await;
    let mut extra = replacement.spec.services[0].clone();
    extra.name = "extra".into();
    extra.mounts.clear();
    replacement.spec.services.push(extra);
    harness.docker.hold_jobs.store(true, Ordering::SeqCst);
    let operation = harness.apply(replacement, None).await;
    // The conflict appears while the job runs.
    let conflict = async {
        harness.wait_for_job_starts(1).await;
        let foreign = harness.add_foreign_service(&operation, "extra").await;
        harness.docker.hold_jobs.store(false, Ordering::SeqCst);
        foreign
    };
    let ((), foreign) = tokio::join!(harness.scan(), conflict);
    for _ in 0..3 {
        assert_eq!(
            harness.scan_result(&operation).await,
            (OperationState::Failed, Some("ownership_conflict".into()))
        );
        assert_eq!(harness.owned_services().await, [] as [String; 0]);
    }

    harness.remove_foreign_service(&foreign).await;
    harness.finish(&operation).await;
    assert_eq!(harness.owned_services().await, ["extra", "web"]);
    assert_eq!(harness.started_jobs().await, ["migrate"]);
}

#[tokio::test]
async fn jobs_run_while_the_services_they_stop_are_stopped() {
    for replacement in manifests_with_web_stopping_job() {
        let harness = ControllerHarness::new().await;
        let first = harness.apply(manifest(), None).await;
        harness.finish(&first).await;
        let operation = harness.apply(replacement, None).await;
        harness.finish(&operation).await;
        // Web stopped before the job and restarted with the rollout.
        assert_eq!(
            *harness.docker.job_starts.lock().await,
            [JobStart {
                job: "migrate".into(),
                services: 0,
                networks: 1,
                volumes: 1
            }]
        );
        assert_eq!(harness.docker.observed.lock().await.services.len(), 1);
    }
}

#[tokio::test]
async fn failed_jobs_restart_the_services_they_stopped_once_their_run_stopped() {
    for replacement in manifests_with_web_stopping_job() {
        a_failed_job_restarts_web_once_its_run_stopped(replacement).await;
    }
}

/// Deploys `replacement`, whose job stops `web`, with a failing job whose
/// cleanup fails at first.
async fn a_failed_job_restarts_web_once_its_run_stopped(replacement: ApplicationManifest) {
    let harness = ControllerHarness::new().await;
    let first = harness.apply(manifest(), None).await;
    harness.finish(&first).await;
    harness.docker.job_exit.store(3, Ordering::SeqCst);
    harness
        .docker
        .fail_job_cleanup
        .store(true, Ordering::SeqCst);
    let operation = harness.apply(replacement, None).await;
    // While the run may still be running, web stays stopped.
    for _ in 0..3 {
        assert_eq!(
            harness.scan_result(&operation).await,
            (OperationState::Failed, Some("job_failed".into()))
        );
        assert_eq!(
            harness.docker.observed.lock().await.services,
            [] as [ObservedService; 0]
        );
    }
    harness
        .docker
        .fail_job_cleanup
        .store(false, Ordering::SeqCst);
    harness.scan_result(&operation).await;
    assert!(harness.docker.jobs.lock().await.is_empty());
    let observed = harness.docker.observed.lock().await;
    assert_eq!(observed.services.len(), 1);
    assert_eq!(observed.services[0].labels[SERVICE_LABEL], "web");
}

#[tokio::test]
async fn resumed_job_keeps_the_deadline_of_its_start() {
    let mut harness = ControllerHarness::new().await;
    harness.docker.hold_jobs.store(true, Ordering::SeqCst);
    let operation = harness.apply(migration(), None).await;
    let shutdown = CancellationToken::new();
    let (scanned, ()) = tokio::join!(harness.controller.scan(&shutdown), async {
        harness.wait_for_job_starts(1).await;
        shutdown.cancel();
    });
    scanned.unwrap();
    // The daemon stays down past the run's deadline.
    for run in harness.docker.jobs.lock().await.values_mut() {
        run.deadline = std::time::SystemTime::now() - Duration::from_secs(1);
    }
    harness.reopen().await;
    let result = tokio::time::timeout(Duration::from_secs(30), harness.scan_result(&operation))
        .await
        .expect("the resumed run times out at its original deadline");
    assert_eq!(result, (OperationState::Failed, Some("job_timeout".into())));
    assert_eq!(harness.started_jobs().await, ["migrate"]);
}

#[tokio::test]
async fn before_rollout_jobs_gate_service_changes_and_record_output() {
    let harness = ControllerHarness::new().await;
    let mut input = migration();
    let first = harness.apply(input.clone(), None).await;
    // A run of a job an earlier manifest declared is removed before jobs start.
    let stored = harness.store.get(&first.environment_id).await.unwrap();
    let earlier = manifest_with_jobs(&["seed"], 300)
        .validate()
        .unwrap()
        .normalize(stored.application.application.id().clone());
    let leftover = prebuilt::resolve(&earlier, &first.environment_id, harness.store.instance_id())
        .jobs[0]
        .for_operation("earlier-operation");
    harness.docker.jobs.lock().await.insert(
        leftover.container.name.to_string(),
        JobRunState::of(&leftover, 0),
    );
    harness.finish(&first).await;
    // The first job sees its network and volume but no service yet.
    assert_eq!(
        *harness.docker.job_starts.lock().await,
        [JobStart {
            job: "migrate".into(),
            services: 0,
            networks: 1,
            volumes: 1,
        }]
    );
    assert!(harness.docker.jobs.lock().await.is_empty());
    let (run, output) = harness.latest_run(&first.environment_id).await;
    assert_eq!(
        (run.job.as_deref(), run.exit_code, run.state),
        (
            Some("migrate"),
            Some(0),
            piqueld_core::api::BuildState::Succeeded
        )
    );
    assert_eq!(output, "migrated\n");

    assert!(!run.log_truncated);

    // A failing job fails the deployment before the running service changes.
    harness.docker.job_exit.store(3, Ordering::SeqCst);
    harness
        .docker
        .truncate_job_output
        .store(true, Ordering::SeqCst);
    input.spec.services[0].replicas = 2.into();
    let failed = harness.apply(input, None).await;
    assert_eq!(
        harness.scan_result(&failed).await,
        (OperationState::Failed, Some("job_failed".into()))
    );
    assert_eq!(harness.docker.observed.lock().await.services[0].replicas, 1);
    assert_eq!(
        harness
            .store
            .get(&first.environment_id)
            .await
            .unwrap()
            .environment
            .resolved_generation,
        Some(1)
    );
    let (run, output) = harness.latest_run(&first.environment_id).await;
    assert_eq!(
        (run.exit_code, run.state),
        (Some(3), piqueld_core::api::BuildState::Failed)
    );
    // Output dropped by Docker is flagged, after Docker's explanation is kept.
    assert!(run.log_truncated);
    assert_eq!(output, "migrated\npiqueld: task: non-zero exit (3)\n");
    assert!(harness.docker.jobs.lock().await.is_empty());
}

#[tokio::test]
async fn a_secret_only_a_job_mounts_is_generated_for_the_job_alone() {
    use piqueld_core::manifest::{SecretDeclaration, SecretEncoding, SecretGenerator, SecretMount};
    let harness = ControllerHarness::new().await;
    let mut input = migration();
    input.spec.secrets.push(SecretDeclaration {
        name: "owner-password".into(),
        generate: SecretGenerator::Random {
            bytes: 16,
            encoding: SecretEncoding::Hex,
        },
    });
    input.spec.jobs[0].secrets.push(SecretMount {
        name: "owner-password".into(),
        target: "/run/secrets/owner-password".into(),
    });
    let deployment = harness.apply(input, None).await;
    harness.finish(&deployment).await;
    let target = harness.target(&deployment.environment_id).await;
    let pinned = &target.secret_names["owner-password"];
    assert!(harness.docker.secrets.lock().await.contains_key(pinned));
    assert_eq!(target.services[0].secrets, []);
    assert_eq!(
        target.jobs[0].container.secrets,
        [piqueld_core::resource::SecretFile {
            secret_name: pinned.clone(),
            target: "/run/secrets/owner-password".into(),
        }]
    );
}

#[tokio::test]
async fn failed_job_keeps_an_unchanged_application_degraded() {
    let harness = ControllerHarness::new().await;
    let applications = harness.applications();
    let first = harness.apply(migration(), None).await;
    harness.finish(&first).await;
    harness.docker.job_exit.store(3, Ordering::SeqCst);
    let redeploy = applications
        .deploy(&first.environment_id, None)
        .await
        .unwrap();
    assert_eq!(
        harness.scan_result(&redeploy).await,
        (OperationState::Failed, Some("job_failed".into()))
    );
    // Services already match the target, but the deployment still failed.
    harness.scan_result(&redeploy).await;
    let status = harness.store.status(&first.environment_id).await.unwrap();
    assert_eq!(status.state, piqueld::store::ApplicationState::Degraded);
    assert_eq!(harness.started_jobs().await, ["migrate", "migrate"]);
}

#[tokio::test]
async fn retried_operation_skips_jobs_that_already_succeeded() {
    let harness = ControllerHarness::new().await;
    *harness.docker.conflicting_job.lock().await = Some("seed".into());
    let operation = harness
        .apply(manifest_with_jobs(&["migrate", "seed"], 300), None)
        .await;
    assert_eq!(
        harness.scan_result(&operation).await,
        (OperationState::Failed, Some("ownership_conflict".into()))
    );
    // Planning cannot see the conflict, so the next scan retries the same
    // operation, which runs only the job that has not succeeded yet.
    *harness.docker.conflicting_job.lock().await = None;
    harness.finish(&operation).await;
    assert_eq!(harness.started_jobs().await, ["migrate", "seed"]);
}

/// Waiting for removed job runs to stop leaves the Docker mutation lock
/// free, so other environments' changes and secret deletion carry on.
#[tokio::test]
async fn waiting_for_job_runs_to_stop_leaves_docker_mutations_free() {
    let harness = ControllerHarness::new().await;
    let operation = harness.apply(migration(), None).await;
    let held = harness.docker.hold_job_stop.lock().await;
    let shutdown = CancellationToken::new();
    let scan = harness.controller.scan(&shutdown);
    tokio::pin!(scan);
    tokio::select! {
        result = &mut scan => panic!("scan finished while job runs were stopping: {result:?}"),
        waiting = eventually(Duration::from_secs(10), POLL, async || {
            (harness.docker.job_stop_waits.load(Ordering::SeqCst) > 0).then_some(())
        }) => waiting.expect("the deployment waits for earlier job runs to stop"),
    }
    let runtime = harness.controller.runtime(Arc::default());
    drop(
        tokio::time::timeout(Duration::from_secs(5), runtime.lock_mutations())
            .await
            .expect("the mutation lock is free while job runs stop"),
    );
    drop(held);
    scan.await.unwrap();
    assert_eq!(
        harness.operation(&operation.id).await.state,
        OperationState::Succeeded
    );
}

/// Shutdown lets a deployment waiting for its job record that the run was
/// interrupted, instead of dropping it mid-way.
#[tokio::test]
async fn shutdown_lets_a_running_job_record_its_interruption() {
    let harness = ControllerHarness::new().await;
    harness.docker.hold_jobs.store(true, Ordering::SeqCst);
    let operation = harness.apply(migration(), None).await;
    let shutdown = CancellationToken::new();
    let run = harness.controller.run(
        Arc::default(),
        Duration::from_mins(1),
        0,
        0,
        shutdown.clone(),
    );
    let stop = async {
        harness.wait_for_job_starts(1).await;
        shutdown.cancel();
    };
    let (result, ()) =
        tokio::time::timeout(Duration::from_secs(20), async { tokio::join!(run, stop) })
            .await
            .expect("shutdown finishes within its grace period");
    result.unwrap();
    let (run, _) = harness.latest_run(&operation.environment_id).await;
    assert_eq!(run.state, piqueld_core::api::BuildState::Interrupted);
    // The run is kept for the retried deployment to resume.
    assert_eq!(harness.docker.jobs.lock().await.len(), 1);
}

/// A failed migration fails its deployment as soon as its run finished,
/// before its output is read and its run cleaned up. Dropping the deployment
/// there, as a shutdown past its grace or a crash does, keeps the failure,
/// and a restart doesn't run the migration again.
#[tokio::test]
async fn a_failed_job_keeps_its_failure_when_its_deployment_is_dropped() {
    let mut harness = ControllerHarness::new().await;
    harness.docker.job_exit.store(3, Ordering::SeqCst);
    let operation = harness.apply(migration(), None).await;
    let held = harness.docker.hold_job_output.lock().await;
    let shutdown = CancellationToken::new();
    tokio::select! {
        result = harness.controller.scan(&shutdown) => {
            panic!("scan finished while the output read was held: {result:?}");
        }
        reading = eventually(Duration::from_secs(10), POLL, async || {
            (harness.docker.job_output_reads.load(Ordering::SeqCst) > 0).then_some(())
        }) => reading.expect("the failed run's output is read"),
    }
    drop(held);
    harness.reopen().await;
    harness.scan().await;
    let failed = harness.operation(&operation.id).await;
    assert_eq!(failed.state, OperationState::Failed);
    assert_eq!(failed.error_code.as_deref(), Some("job_failed"));
    assert_eq!(harness.started_jobs().await, ["migrate"]);
}

#[tokio::test]
async fn job_run_accepted_before_a_lost_response_is_not_started_again() {
    let harness = ControllerHarness::new().await;
    harness.docker.lose_job_start.store(true, Ordering::SeqCst);
    let operation = harness.apply(migration(), None).await;
    harness.finish(&operation).await;
    assert_eq!(harness.started_jobs().await, ["migrate"]);
}

#[tokio::test]
async fn restarted_daemon_resumes_a_running_job() {
    let mut harness = ControllerHarness::new().await;
    harness.docker.hold_jobs.store(true, Ordering::SeqCst);
    let operation = harness.apply(migration(), None).await;
    let shutdown = CancellationToken::new();
    let (scanned, ()) = tokio::join!(harness.controller.scan(&shutdown), async {
        harness.wait_for_job_starts(1).await;
        shutdown.cancel();
    });
    scanned.unwrap();
    // Shutdown leaves the run in Docker for the restarted daemon.
    assert_eq!(harness.docker.jobs.lock().await.len(), 1);

    harness.reopen().await;
    harness.docker.hold_jobs.store(false, Ordering::SeqCst);
    harness.finish(&operation).await;
    assert_eq!(harness.started_jobs().await, ["migrate"]);
    assert!(harness.docker.jobs.lock().await.is_empty());
    let (run, output) = harness.latest_run(&operation.environment_id).await;
    assert_eq!(run.state, piqueld_core::api::BuildState::Succeeded);
    assert_eq!(output, "migrated\n");
}

#[tokio::test]
async fn superseded_job_run_is_stopped() {
    let harness = ControllerHarness::new().await;
    harness.docker.hold_jobs.store(true, Ordering::SeqCst);
    let first = harness.apply(migration(), None).await;
    let cancellation = CancellationToken::new();
    let (scanned, second) = tokio::join!(harness.controller.scan(&cancellation), async {
        harness.wait_for_job_starts(1).await;
        harness.apply(manifest(), None).await
    });
    scanned.unwrap();
    // The replacement deployment declares no jobs, so the superseded
    // operation must stop its own run.
    assert!(harness.docker.jobs.lock().await.is_empty());
    let (run, _) = harness.latest_run(&first.environment_id).await;
    assert_eq!(run.state, piqueld_core::api::BuildState::Interrupted);
    harness.finish(&second).await;
    assert_eq!(harness.started_jobs().await, ["migrate"]);
}

/// Image cleanup waits while an operation runs: once superseded, it is no
/// retention root, but its job may still be starting on its image.
#[tokio::test]
async fn image_cleanup_waits_for_a_running_operation() {
    let harness = ControllerHarness::new().await;
    harness.docker.hold_jobs.store(true, Ordering::SeqCst);
    harness.apply(migration(), None).await;
    let cancellation = CancellationToken::new();
    let (scanned, ()) = tokio::join!(harness.controller.scan(&cancellation), async {
        harness.wait_for_job_starts(1).await;
        assert!(!harness.controller.clean_images().await.unwrap());
        // Superseded, the operation stops its job and finishes.
        harness.apply(manifest(), None).await;
    });
    scanned.unwrap();
    assert!(harness.controller.clean_images().await.unwrap());
}

#[tokio::test]
async fn job_timeout_keeps_output_and_stops_the_run() {
    let harness = ControllerHarness::new().await;
    harness.docker.hold_jobs.store(true, Ordering::SeqCst);
    let operation = harness
        .apply(manifest_with_jobs(&["migrate"], 1), None)
        .await;
    assert_eq!(
        harness.scan_result(&operation).await,
        (OperationState::Failed, Some("job_timeout".into()))
    );
    let (run, output) = harness.latest_run(&operation.environment_id).await;
    assert_eq!(run.state, piqueld_core::api::BuildState::Failed);
    assert_eq!(output, "migrated\n");
    assert!(harness.docker.jobs.lock().await.is_empty());
    assert_eq!(
        harness.docker.observed.lock().await.services,
        [] as [ObservedService; 0]
    );
}

#[tokio::test]
async fn timed_out_job_cleanup_retries_after_restart_without_rerunning() {
    let mut harness = ControllerHarness::new().await;
    harness.docker.hold_jobs.store(true, Ordering::SeqCst);
    harness
        .docker
        .fail_job_cleanup
        .store(true, Ordering::SeqCst);
    let operation = harness
        .apply(manifest_with_jobs(&["migrate"], 1), None)
        .await;
    assert_eq!(
        harness.scan_result(&operation).await,
        (OperationState::Failed, Some("job_timeout".into()))
    );
    let failed = harness.operation(&operation.id).await;
    // Repeated scans retry stopping, without changing the job's failure or
    // starting another run while Docker is unavailable.
    harness.scan().await;
    assert_eq!(harness.docker.jobs.lock().await.len(), 1);
    assert_eq!(harness.operation(&operation.id).await, failed);

    harness.reopen().await;
    harness
        .docker
        .fail_job_cleanup
        .store(false, Ordering::SeqCst);
    harness.scan().await;
    assert!(harness.docker.jobs.lock().await.is_empty());
    assert_eq!(harness.started_jobs().await, ["migrate"]);
    assert_eq!(harness.operation(&operation.id).await, failed);
    assert!(
        harness
            .store
            .get(&operation.environment_id)
            .await
            .unwrap()
            .resolved
            .is_none()
    );
    let (run, output) = harness.latest_run(&operation.environment_id).await;
    assert_eq!(run.state, piqueld_core::api::BuildState::Failed);
    assert_eq!(output, "migrated\n");
}

#[tokio::test]
async fn job_cleanup_still_runs_when_outcome_recording_fails() {
    for trigger in [
        "CREATE TRIGGER reject_job_outcome BEFORE UPDATE OF state ON builds WHEN NEW.job IS NOT NULL AND NEW.state='failed' BEGIN SELECT RAISE(FAIL,'build history unavailable'); END",
        "CREATE TRIGGER reject_job_outcome BEFORE INSERT ON events WHEN NEW.phase='run_job' AND NEW.kind='action_failed' BEGIN SELECT RAISE(FAIL,'journal unavailable'); END",
    ] {
        let harness = ControllerHarness::new().await;
        let mut connection = harness.database().await;
        sqlx::query(trigger).execute(&mut connection).await.unwrap();
        harness.docker.hold_jobs.store(true, Ordering::SeqCst);
        let operation = harness
            .apply(manifest_with_jobs(&["migrate"], 1), None)
            .await;
        assert_eq!(
            harness.scan_result(&operation).await,
            (OperationState::Failed, Some("job_timeout".into()))
        );
        assert!(harness.docker.jobs.lock().await.is_empty());
        sqlx::query("DROP TRIGGER reject_job_outcome")
            .execute(&mut connection)
            .await
            .unwrap();
        harness.scan().await;
        assert_eq!(harness.started_jobs().await, ["migrate"]);
    }
}

#[tokio::test]
async fn pending_cleanup_blocks_the_next_job() {
    let harness = ControllerHarness::new().await;
    harness
        .docker
        .fail_job_cleanup
        .store(true, Ordering::SeqCst);
    let operation = harness
        .apply(manifest_with_jobs(&["migrate", "seed"], 300), None)
        .await;
    assert_eq!(
        harness.scan_result(&operation).await,
        (OperationState::Failed, Some("docker_request_failed".into()))
    );
    assert_eq!(harness.started_jobs().await, ["migrate"]);
    assert_eq!(harness.docker.jobs.lock().await.len(), 1);
    // A still-failing cleanup on a due deployment retry must refresh the
    // deployment's backoff, while keeping the successful job skipped.
    harness.expire_backoff(&operation).await;
    assert_eq!(
        harness.scan_result(&operation).await,
        (OperationState::Failed, Some("docker_request_failed".into()))
    );
    assert_eq!(
        harness.operation(&operation.id).await.consecutive_failures,
        2
    );
    assert_eq!(harness.started_jobs().await, ["migrate"]);
    harness
        .docker
        .fail_job_cleanup
        .store(false, Ordering::SeqCst);
    harness.expire_backoff(&operation).await;
    harness.finish(&operation).await;
    assert_eq!(harness.started_jobs().await, ["migrate", "seed"]);
    assert!(harness.docker.jobs.lock().await.is_empty());
}

#[tokio::test]
async fn unknown_job_status_keeps_the_run_for_a_retry() {
    let harness = ControllerHarness::new().await;
    harness.docker.hold_jobs.store(true, Ordering::SeqCst);
    harness.docker.fail_job_status.store(true, Ordering::SeqCst);
    let operation = harness
        .apply(manifest_with_jobs(&["migrate"], 1), None)
        .await;
    // Docker never reported the status, so the outcome is unknown: the
    // failure is transient and the run keeps going.
    assert_eq!(
        harness.scan_result(&operation).await,
        (OperationState::Failed, Some("docker_request_failed".into()))
    );
    let (run, _) = harness.latest_run(&operation.environment_id).await;
    assert_eq!(run.state, piqueld_core::api::BuildState::Interrupted);
    assert_eq!(harness.docker.jobs.lock().await.len(), 1);

    // The retry resumes the run instead of starting it again.
    harness
        .docker
        .fail_job_status
        .store(false, Ordering::SeqCst);
    harness.docker.hold_jobs.store(false, Ordering::SeqCst);
    harness.expire_backoff(&operation).await;
    harness.finish(&operation).await;
    assert_eq!(harness.started_jobs().await, ["migrate"]);
    let (run, _) = harness.latest_run(&operation.environment_id).await;
    assert_eq!(run.state, piqueld_core::api::BuildState::Succeeded);
    assert!(harness.docker.jobs.lock().await.is_empty());
}

#[tokio::test]
async fn recorded_success_survives_a_crash_before_cleanup() {
    let mut harness = ControllerHarness::new().await;
    let operation = harness.apply(migration(), None).await;
    let held = harness.docker.hold_job_cleanup.lock().await;
    // The success is stored before the run is removed; dropping the scan
    // there is a crash.
    let succeeded = eventually(Duration::from_secs(10), POLL, async || {
        let builds = harness
            .store
            .builds(
                None,
                Some(&operation.environment_id),
                &piqueld_core::access::Scope::All,
                None,
                10,
            )
            .await
            .unwrap();
        builds
            .items
            .first()
            .is_some_and(|run| run.state == piqueld_core::api::BuildState::Succeeded)
            .then_some(())
    });
    let shutdown = CancellationToken::new();
    tokio::select! {
        result = harness.controller.scan(&shutdown) => {
            panic!("scan finished while cleanup was held: {result:?}");
        }
        recorded = succeeded => {
            recorded.expect("success recorded while cleanup was held");
        }
    }
    drop(held);
    assert_eq!(harness.docker.jobs.lock().await.len(), 1);

    harness.reopen().await;
    harness.finish(&operation).await;
    assert_eq!(harness.started_jobs().await, ["migrate"]);
    assert!(harness.docker.jobs.lock().await.is_empty());
}

#[tokio::test]
async fn retry_reruns_a_failed_run_left_in_place() {
    let harness = ControllerHarness::new().await;
    harness.docker.job_exit.store(3, Ordering::SeqCst);
    harness
        .docker
        .fail_job_cleanup
        .store(true, Ordering::SeqCst);
    let operation = harness.apply(migration(), None).await;
    assert_eq!(
        harness.scan_result(&operation).await,
        (OperationState::Failed, Some("job_failed".into()))
    );
    assert_eq!(harness.docker.jobs.lock().await.len(), 1);
    // The failed run's outcome is recorded, so retrying the operation starts
    // a new run.
    harness.docker.job_exit.store(0, Ordering::SeqCst);
    harness
        .docker
        .fail_job_cleanup
        .store(false, Ordering::SeqCst);
    let retry = harness
        .applications()
        .reconcile(&operation.environment_id, None)
        .await
        .unwrap();
    assert_eq!(retry.id, operation.id);
    harness.finish(&operation).await;
    assert_eq!(harness.started_jobs().await, ["migrate", "migrate"]);
}

#[tokio::test]
async fn job_outcome_survives_output_and_cleanup_failures() {
    let harness = ControllerHarness::new().await;
    harness.docker.fail_job_output.store(true, Ordering::SeqCst);
    harness
        .docker
        .fail_job_cleanup
        .store(true, Ordering::SeqCst);
    let operation = harness.apply(migration(), None).await;
    harness.finish(&operation).await;
    let (run, output) = harness.latest_run(&operation.environment_id).await;
    assert_eq!(
        (run.exit_code, run.state),
        (Some(0), piqueld_core::api::BuildState::Succeeded)
    );
    assert!(
        output.starts_with("piqueld: job output unavailable"),
        "{output}"
    );
    assert_eq!(harness.docker.jobs.lock().await.len(), 1);
    assert_eq!(harness.started_jobs().await, ["migrate"]);
    // A periodic scan removes the leftover without another deployment.
    harness
        .docker
        .fail_job_cleanup
        .store(false, Ordering::SeqCst);
    harness.finish(&operation).await;
    assert_eq!(harness.started_jobs().await, ["migrate"]);
    assert!(harness.docker.jobs.lock().await.is_empty());
}

#[tokio::test]
async fn blocked_deployment_runs_no_jobs() {
    let harness = ControllerHarness::new().await;
    let operation = harness.apply(migration(), None).await;
    harness
        .docker
        .observed
        .lock()
        .await
        .networks
        .push(ObservedNetwork {
            name: piqueld_core::DockerNetworkName::for_application(&operation.environment_id)
                .to_string(),
            runtime_configuration_matches: true,
            labels: foreign_labels(&operation.environment_id),
            in_use: false,
        });
    assert_eq!(
        harness.scan_result(&operation).await,
        (OperationState::Failed, Some("ownership_conflict".into()))
    );
    assert_eq!(harness.started_jobs().await, [] as [String; 0]);
}
