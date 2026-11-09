//! Environments of one application, and the networks and sockets that link applications together.

use super::*;

/// Two independently deployed environments exercising the shared Docker backend.
struct EnvironmentHarness {
    harness: ControllerHarness,
    applications: TestApplications,
    production: EnvironmentId,
    staging: EnvironmentId,
    generation: u64,
}

impl EnvironmentHarness {
    async fn new() -> Self {
        use piqueld::api::{Mutation, MutationResponse};
        use piqueld_core::EnvironmentName;
        let mut harness = ControllerHarness::new().await;
        harness.application = with_token(harness.application.to_manifest())
            .validate()
            .unwrap()
            .normalize(harness.application.id().clone());
        let store = &harness.store;
        let applications = harness.applications();
        let saved = store
            .save_application(&ApplicationTemplate::from(&harness.application), None, None)
            .await
            .unwrap();
        let production = harness.environment().await;
        // Boxed: accepting a mutation holds a whole manifest across awaits.
        let MutationResponse::Environment(staging) = Box::pin(applications.accept(
            Daemon,
            Mutation::CreateEnvironment {
                application: harness.application.id().clone(),
                name: EnvironmentName::parse("staging").unwrap(),
                source: None,
            },
            Some(saved.generation),
            false,
            None,
        ))
        .await
        .unwrap() else {
            panic!("created staging")
        };
        // Creating an environment bumps the application generation.
        let generation = saved.generation + 1;
        // One stored value; each environment mounts its own Docker copy.
        harness.store_token(&production, 0, b"shared token").await;
        for id in [&production, &staging.id] {
            applications.deploy(id, Some(generation)).await.unwrap();
        }
        harness.scan().await;
        Self {
            harness,
            applications,
            production,
            staging: staging.id,
            generation,
        }
    }

    async fn delete_staging(&self) {
        use piqueld::api::Mutation;
        self.applications
            .accept(
                Daemon,
                Mutation::Delete {
                    id: self.staging.clone(),
                },
                Some(self.generation),
                false,
                None,
            )
            .await
            .unwrap();
        self.harness.scan().await;
    }
}

#[tokio::test]
async fn deleting_staging_preserves_production_runtime_secrets_and_history() {
    let fixture = Box::pin(EnvironmentHarness::new()).await;
    let harness = &fixture.harness;
    let store = &harness.store;
    let production = &fixture.production;
    let staging = &fixture.staging;
    let production_runtime = harness.docker.observe(production).await.unwrap();
    let staging_runtime = harness.docker.observe(staging).await.unwrap();
    assert_eq!(production_runtime.services.len(), 1);
    assert_eq!(staging_runtime.services.len(), 1);
    assert_ne!(
        production_runtime.services[0].name,
        staging_runtime.services[0].name
    );
    assert_ne!(
        production_runtime.volumes[0].name,
        staging_runtime.volumes[0].name
    );
    let production_history = store.deployments(production, None, 10).await.unwrap();
    let production_events = store.events(Some(production), None, 100).await.unwrap();
    assert!(!production_history.items.is_empty());
    assert_eq!(
        store
            .deployments(staging, None, 10)
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    assert_eq!(harness.docker.secrets.lock().await.len(), 2);

    fixture.delete_staging().await;

    assert!(matches!(
        store.get(staging).await,
        Err(piqueld::store::StoreError::NotFound)
    ));
    let remaining = harness.docker.observe(staging).await.unwrap();
    assert_eq!(remaining.services, [] as [ObservedService; 0]);
    assert_eq!(remaining.networks, [] as [ObservedNetwork; 0]);
    assert_eq!(
        remaining.volumes, staging_runtime.volumes,
        "staging volume data is retained"
    );
    assert_eq!(
        harness.docker.observe(production).await.unwrap(),
        production_runtime
    );
    assert_eq!(
        store.status(production).await.unwrap().state,
        piqueld::store::ApplicationState::Ready
    );
    let history = store.deployments(production, None, 10).await.unwrap();
    assert_eq!(
        history
            .items
            .iter()
            .map(|item| (&item.operation.id, &item.application))
            .collect::<Vec<_>>(),
        production_history
            .items
            .iter()
            .map(|item| (&item.operation.id, &item.application))
            .collect::<Vec<_>>()
    );
    let events = store.events(Some(production), None, 100).await.unwrap();
    assert!(
        production_events
            .items
            .iter()
            .all(|old| events.items.iter().any(|event| event.id == old.id))
    );
    // Staging's events stay in the application's history until it is deleted.
    let staging_events = store.events(Some(staging), None, 100).await.unwrap();
    assert!(
        staging_events
            .items
            .iter()
            .any(|event| event.environment_id.as_ref() == Some(staging))
    );
    let stored = store
        .stored_secrets(harness.application.id())
        .await
        .unwrap();
    assert_eq!(stored[0].metadata.generation, 1);
    let values = harness.docker.secrets.lock().await;
    assert_eq!(values.len(), 1, "only staging's copy is removed");
    assert_eq!(values.values().next().unwrap().value, b"shared token");
    let environments = store.environments(harness.application.id()).await;
    assert_eq!(environments.unwrap().len(), 1);
    assert_eq!(
        store
            .application(harness.application.id())
            .await
            .unwrap()
            .generation,
        fixture.generation + 1,
        "deleting staging bumps the generation once"
    );
}

/// Deploys `auth`, whose `web` service is exported to `notes` beside an
/// unexported `worker`, and `notes`, which imports it, then scans until they
/// are linked. Returns the harness and both environments.
async fn linked_harness() -> (ControllerHarness, EnvironmentId, EnvironmentId) {
    let harness = ControllerHarness::new().await;
    let auth = harness
        .deploy_application("auth", |manifest| {
            manifest.spec.exports.push(piqueld_core::manifest::Export {
                service: "web".into(),
                applications: vec!["notes".into()],
                previews: false,
            });
            let mut worker = manifest.spec.services[0].clone();
            worker.name = "worker".into();
            manifest.spec.services.push(worker);
        })
        .await;
    let notes = harness
        .deploy_application("notes", |manifest| {
            manifest.spec.imports.push(piqueld_core::manifest::Import {
                application: "auth".into(),
                service: "web".into(),
                environment: None,
            });
        })
        .await;
    for _ in 0..3 {
        harness.scan().await;
    }

    (harness, auth, notes)
}

/// Two applications linked by an export and an import connect once both
/// deploy: the importer creates the link network, and the exporter's service
/// joins it on its next scan under `<service>.<application>`.
#[tokio::test]
async fn linked_applications_join_the_importers_network() {
    use piqueld_core::links::ImportState;
    let (harness, auth, notes) = linked_harness().await;
    let network = piqueld_core::DockerNetworkName::for_link(&notes, &auth);
    let importer = harness.docker.observe(&notes).await.unwrap();
    assert!(
        importer
            .networks
            .iter()
            .any(|observed| observed.name == network.as_str())
    );
    assert!(importer.services[0].networks.iter().any(|attachment| {
        attachment.network == network.as_str() && attachment.aliases.is_empty()
    }));
    let exporter = harness.docker.observe(&auth).await.unwrap();
    assert!(
        web(&exporter)
            .networks
            .contains(&piqueld_core::resource::NetworkAttachment {
                network: network.to_string(),
                aliases: vec!["web.auth".into()],
            })
    );
    assert!(
        exporter
            .services
            .iter()
            .filter(|service| service.name != web(&exporter).name)
            .all(|service| service
                .networks
                .iter()
                .all(|attachment| attachment.network != network.as_str())),
        "only the exported service joins"
    );
    let report = harness
        .store
        .link_report(&notes, None, &piqueld_core::access::Scope::All)
        .await
        .unwrap();
    assert_eq!(report.imports[0].state, ImportState::Connected);
    let report = harness
        .store
        .link_report(&auth, None, &piqueld_core::access::Scope::All)
        .await
        .unwrap();
    assert_eq!(report.importers[0].application.as_str(), "notes");
}

/// auth's worker fails, which blocks its plan. Deleting notes neither waits
/// for auth nor leaves web attached: web leaves the link network, which is
/// removed once nothing uses it.
#[tokio::test]
async fn revoked_links_close_while_the_exporters_plan_is_blocked() {
    let (harness, auth, notes) = linked_harness().await;
    let network = piqueld_core::DockerNetworkName::for_link(&notes, &auth);
    for service in &mut harness.docker.observed.lock().await.services {
        if service.labels.get(SERVICE_LABEL).map(String::as_str) == Some("worker") {
            service.convergence = Convergence::Failed;
        }
    }
    harness.store.request_delete(&notes).await.unwrap();
    for _ in 0..3 {
        harness.scan().await;
    }
    assert!(matches!(
        harness.store.get(&notes).await,
        Err(piqueld::store::StoreError::NotFound)
    ));
    let exporter = harness.docker.observe(&auth).await.unwrap();
    assert!(
        web(&exporter)
            .networks
            .iter()
            .all(|attachment| attachment.network != network.as_str())
    );
    assert_eq!(exporter.linked_networks, []);
    assert!(
        exporter
            .services
            .iter()
            .any(|service| service.convergence == Convergence::Failed),
        "auth's plan stayed blocked"
    );
}

#[tokio::test]
async fn deleting_an_exporter_removes_networks_its_deleted_importers_left() {
    let (harness, auth, notes) = linked_harness().await;
    let network = piqueld_core::DockerNetworkName::for_link(&notes, &auth);
    let scan = || async {
        for _ in 0..3 {
            harness.scan().await;
        }
    };
    let leftover = harness
        .docker
        .observed
        .lock()
        .await
        .networks
        .iter()
        .find(|observed| observed.name == network.as_str())
        .cloned()
        .unwrap();
    harness.store.request_delete(&notes).await.unwrap();
    scan().await;
    // As if auth's deletion came before the full scan that removes it.
    harness.docker.observed.lock().await.networks.push(leftover);
    harness.store.request_delete(&auth).await.unwrap();
    scan().await;
    assert!(matches!(
        harness.store.get(&auth).await,
        Err(piqueld::store::StoreError::NotFound)
    ));
    assert!(
        harness
            .docker
            .observed
            .lock()
            .await
            .networks
            .iter()
            .all(|observed| observed.name != network.as_str())
    );
}

/// A granted environment's receiver gets its socket directory mounted, as
/// the daemon's user, on the next scan without a deployment. A renamed
/// receiver gets a directory of its own, and the old one's is removed with
/// its service. Revoking the grant unmounts it and removes the directories.
#[tokio::test]
async fn telemetry_sockets_are_mounted_while_the_environment_is_granted() {
    let mut harness = ControllerHarness::new().await;
    let data = tempfile::tempdir().unwrap();
    harness.controller = Controller::new(Arc::clone(&harness.docker), Arc::clone(&harness.store))
        .with_telemetry(Some(piqueld::telemetry::Sockets::new(data.path())));
    let mut manifest = harness.application.to_manifest();
    manifest.spec.services[0].receives_telemetry = true;
    let id = harness.application.id().clone();
    harness.set_application(manifest.clone().validate().unwrap().normalize(id.clone()));
    let created = harness.create().await;
    harness.finish(&created).await;
    let environment = harness.environment().await;
    let mounted = async |harness: &ControllerHarness| {
        harness.docker.observe(&environment).await.unwrap().services[0]
            .telemetry
            .clone()
    };
    assert_eq!(mounted(&harness).await, None, "not granted yet");

    let grant = |harness: &ControllerHarness, granted| {
        let mutation = piqueld::api::Mutation::SetTelemetry {
            id: environment.clone(),
            granted,
        };
        let applications = harness.applications();
        async move {
            Box::pin(applications.accept(Daemon, mutation, None, false, None))
                .await
                .unwrap();
        }
    };
    grant(&harness, true).await;
    harness.scan().await;
    let sockets = data.path().join("telemetry").join(environment.as_str());
    let directory = sockets.join(manifest.spec.services[0].name.as_str());
    let mount = mounted(&harness)
        .await
        .expect("granted receivers mount their socket");
    assert_eq!(mount.source, directory.display().to_string());
    assert_ne!(mount.user, "");
    assert!(directory.is_dir());

    manifest.spec.services[0].name = "otel".into();
    harness.set_application(manifest.validate().unwrap().normalize(id));
    let renamed = harness.create().await;
    harness.finish(&renamed).await;
    harness.scan().await;
    let mount = mounted(&harness)
        .await
        .expect("the renamed receiver mounts its own");
    assert_eq!(mount.source, sockets.join("otel").display().to_string());
    assert!(
        !directory.exists(),
        "the old receiver's directory is removed"
    );

    harness.scan().await;
    grant(&harness, false).await;
    harness.scan().await;
    assert_eq!(mounted(&harness).await, None, "revoking unmounts it");
    harness.scan().await;
    assert!(!sockets.exists(), "and removes the directories");
}

/// Stopping scales every service to zero and reports the runtime stopped:
/// the deployment that rolled out runs again with no replicas, and after a
/// later one failed before rolling out, repair stops what runs. Starting
/// runs them again, and so does deploying.
#[tokio::test]
async fn stopped_environments_run_no_replicas_until_started_or_deployed() {
    let harness = ControllerHarness::new().await;
    let applications = harness.applications();
    let first = harness.apply(manifest(), None).await;
    harness.finish(&first).await;
    let id = first.environment_id.clone();
    let health = async || harness.store.status(&id).await.unwrap().runtime_health;
    let replicas = async || harness.replicas_of(&id).await;

    let stopped = applications.stop(&id).await.unwrap();
    assert_eq!(stopped.id, first.id, "the rolled out deployment runs again");
    harness.finish(&stopped).await;
    assert_eq!(replicas().await, 0);
    assert_eq!(health().await.as_deref(), Some("stopped"));
    harness.scan().await;
    assert_eq!(replicas().await, 0, "repair keeps it stopped");

    let started = applications.reconcile(&id, None).await.unwrap();
    harness.finish(&started).await;
    assert_eq!(replicas().await, 1);
    assert_eq!(health().await.as_deref(), Some("ready"));

    let mut unstable = manifest();
    unstable.spec.services[0].source = piqueld_core::Source::Image {
        image: "ghcr.io/example/unstable:1".into(),
    };
    harness.docker.arm_tag_flips(100).await;
    let failed = harness.apply(unstable, None).await;
    harness.scan().await;
    assert_eq!(
        harness.operation(&failed.id).await.state,
        OperationState::Failed
    );
    let stopped = applications.stop(&id).await.unwrap();
    assert_eq!(
        (stopped.id.as_str(), stopped.state),
        (failed.id.as_str(), OperationState::Failed),
        "a deployment that never rolled out is not retried"
    );
    harness.scan().await;
    assert_eq!(replicas().await, 0, "repair stops the active target");

    harness.docker.arm_tag_flips(0).await;
    let deployed = harness.apply(manifest(), None).await;
    harness.finish(&deployed).await;
    assert_eq!(replicas().await, 1);
    assert!(!harness.store.get(&id).await.unwrap().environment.stopped());
}
