//! Saving, previewing, deploying and inspecting applications, through HTTP and the service layer.

use super::*;

#[tokio::test]
async fn fake_runtime_accepts_digest_pinned_requested_images() {
    let mut input = manifest();
    let Source::Image { image } = &mut input.spec.services[0].source else {
        panic!("expected image fixture")
    };
    *image = format!("ghcr.io/example/notes@sha256:{}", "b".repeat(64)).into();
    let application = input
        .validate()
        .unwrap()
        .normalize(piqueld_core::ApplicationId::parse("app-digest-fixture").unwrap());
    let runtime = FakeRuntime {
        cleanup_gate: None,
        instance: InstanceId::parse("test").unwrap(),
        unavailable: AtomicBool::new(false),
    };

    runtime
        .prepare(
            &piqueld_core::EnvironmentId::default_for(application.id()),
            &application,
            &ResolutionSet::default(),
        )
        .await
        .expect("digest-pinned image resolves");
}

#[tokio::test]
async fn toml_save_exposes_summary_and_full_configuration() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let server = Server::tcp(router(state(&temp).await, FakeAuth)).await;

    let valid_toml = r#"
api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "tomlnotes"
[[spec.services]]
name = "web"
replicas = 2
[spec.services.source]
type = "image"
image = "ghcr.io/example/notes:1"
"#;
    let created = server
        .send(
            Method::POST,
            "/api/v1/applications/apply",
            &[
                ("content-type", "application/toml"),
                ("x-expected-generation", "0"),
            ],
            valid_toml.as_bytes().to_vec(),
        )
        .await;
    assert_eq!(created.status, StatusCode::OK);
    let created = created.json();
    assert!(created["data"]["operation_id"].is_null());
    let application_id = created["data"]["application_id"]
        .as_str()
        .expect("accepted application ID");

    let listed = server
        .send(Method::GET, "/api/v1/applications", &[], Vec::new())
        .await
        .json();
    let summary = &listed["data"]["items"][0];
    assert_eq!(summary["id"], application_id);
    assert_eq!(summary["name"], "tomlnotes");
    assert!(summary.get("application").is_none());

    let full = server
        .send(
            Method::GET,
            &format!("/api/v1/applications/{application_id}"),
            &[],
            Vec::new(),
        )
        .await;
    assert_eq!(full.status, StatusCode::OK);
    let full = full.json();
    assert_eq!(full["data"]["application"]["id"], application_id);
    assert_eq!(
        full["data"]["application"]["spec"]["services"][0]["name"],
        "web"
    );
}

#[tokio::test]
async fn saved_configuration_preview_and_deployment_are_separate_even_offline() {
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    api.runtime.set_unavailable(true);
    let mut request = new_application();
    request.manifest.spec.services.clear();
    let saved = api.client.apply_application(&request).await.unwrap();
    assert!(saved.operation_id.is_none());
    let detail = api
        .client
        .environment_detail(&saved.application_id)
        .await
        .unwrap();
    assert_eq!(
        detail.status.state,
        piqueld_core::ApplicationState::NotDeployed
    );
    assert!(detail.latest_operation.is_none());
    let first = api
        .client
        .deploy_environment(&saved.application_id, saved.generation, None)
        .await
        .unwrap();
    let second = api
        .client
        .deploy_environment(&saved.application_id, saved.generation, None)
        .await
        .unwrap();
    assert_ne!(first.operation_id, second.operation_id);
    let history = api
        .client
        .deployments(&saved.application_id, None, None)
        .await
        .unwrap();
    assert_eq!(history.items.len(), 2);
    assert_eq!(
        history.items[1].operation.state,
        piqueld_core::OperationState::Superseded
    );
    // `limit` sizes the page: one per page here, and 0 is refused.
    let page = api
        .client
        .deployments(&saved.application_id, None, Some(1))
        .await
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].operation.id, second.operation_id);
    assert!(page.next_cursor.is_some());
    let refused = api
        .client
        .deployments(&saved.application_id, None, Some(0))
        .await
        .unwrap_err();
    assert_eq!(api_error(refused).code, "pagination_invalid");
    request.expected_application_id = Some(saved.application_id.clone());
    request.expected_generation = Some(saved.generation);
    request
        .manifest
        .spec
        .volumes
        .push(piqueld_core::manifest::Volume {
            name: "later".into(),
        });
    let saved = api.client.apply_application(&request).await.unwrap();
    assert_eq!(
        api.deployed(&saved.application_id, &second.operation_id)
            .await
            .spec()
            .volumes
            .len(),
        0
    );
    assert!(
        api.client
            .deploy_environment(&saved.application_id, 1, None)
            .await
            .is_err()
    );
    api.runtime.set_unavailable(false);
    request.expected_generation = Some(saved.generation);
    let preview = api.client.plan_application(&request, None).await.unwrap();
    assert!(!preview.identical);
    assert!(!preview.changes.is_empty());
}

#[tokio::test]
async fn deploy_after_rename_captures_saved_name_and_spec() {
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let accepted = api.deploy().await;
    api.finish_operation(&accepted.operation_id, None).await;
    api.client
        .rename_application(
            &accepted.environment_id,
            &piqueld_client::RenameApplicationRequest {
                name: "renamed".into(),
                expected_generation: Some(1),
            },
        )
        .await
        .unwrap();
    let mut edited = new_application();
    edited.manifest.metadata.name = "renamed".into();
    edited.manifest.spec.services[0].replicas = 2.into();
    edited.expected_generation = Some(2);
    edited.expected_application_id = Some(accepted.environment_id.clone());
    api.client.apply_application(&edited).await.unwrap();
    let refreshed = api
        .client
        .deploy_environment(&accepted.environment_id, 3, None)
        .await
        .unwrap();
    let snapshot = api
        .deployed(&accepted.environment_id, &refreshed.operation_id)
        .await;
    assert_eq!(snapshot.metadata().name.as_str(), "renamed");
    assert_eq!(snapshot.spec().services[0].replicas, 2);
    assert_eq!(
        api.deployed(&accepted.environment_id, &accepted.operation_id)
            .await
            .metadata()
            .name
            .as_str(),
        "notes"
    );
}

#[tokio::test]
async fn unavailable_observation_does_not_claim_services_are_missing() {
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let accepted = api.deploy().await;
    let id = piqueld_core::EnvironmentId::parse(&accepted.environment_id).unwrap();
    let app = api.store.get(&id).await.unwrap();
    let target = api
        .runtime
        .prepare(
            &id,
            &app.render(app.manifest().unwrap(), &accepted.operation_id)
                .unwrap()
                .application,
            &ResolutionSet::default(),
        )
        .await
        .unwrap();
    api.store
        .transition_operation(
            &accepted.operation_id,
            piqueld_core::OperationState::Requested,
            piqueld_core::OperationState::Running,
            None,
        )
        .await
        .unwrap();
    let op = api.store.operation(&accepted.operation_id).await.unwrap();
    api.store.save_prepared(&op, &target).await.unwrap();
    api.store.publish_prepared(&op).await.unwrap();
    api.store
        .set_status_for_operation(&op.id, piqueld_core::ApplicationState::Ready, None)
        .await
        .unwrap();
    api.runtime.set_unavailable(true);
    let detail = api
        .client
        .environment_detail(&accepted.environment_id)
        .await
        .unwrap();
    assert!(
        detail
            .diagnostics
            .iter()
            .any(|d| d.code == "runtime_unavailable")
    );
    assert!(detail.observed.services.iter().all(|s| s.diagnostics.is_empty() && s.convergence != piqueld_core::Convergence::Failed));
}

#[tokio::test]
async fn downloaded_manifest_round_trips_saved_configuration_without_docker() {
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    api.runtime.set_unavailable(true);
    let mut apply = new_application();
    apply.manifest.spec.services[0]
        .environment
        .insert("MESSAGE".into(), "quotes \" and newline\n".into());
    apply.manifest.spec.manifest = Some(piqueld_core::manifest::RepositoryManifest {
        repository: piqueld_core::manifest::GitRepository {
            url: "https://example.com/repo.git".into(),
            branch: "main".into(),
            commit: None,
        },
        path: "infra/app.toml".into(),
        sync: piqueld_core::sync::RepositorySync::Off,
    });
    let saved = api.client.apply_application(&apply).await.unwrap();
    let response = api
        .router()
        .send(request(&format!(
            "/api/v1/applications/{}/manifest",
            saved.application_id
        )))
        .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.header("content-type"), Some("application/toml"));
    assert_eq!(response.header("cache-control"), Some("no-store"));
    assert_eq!(
        response.header("content-disposition"),
        Some("attachment; filename=\"notes.toml\"")
    );
    let downloaded: ApplicationManifest = toml::from_str(response.text()).unwrap();
    assert_eq!(downloaded, apply.manifest);
    assert!(
        api.client
            .deployments(&saved.application_id, None, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
}

#[tokio::test]
async fn application_log_snapshot_validates_bounds_and_preserves_task_identity() {
    let temp = tempfile::tempdir().unwrap();
    let server = Server::tcp(router(state(&temp).await, FakeAuth)).await;
    let client = server.client();
    let app = create_and_inspect(&client, &manifest()).await;
    let logs = client
        .environment_logs(&app.environment_id, Some("web"), 5, 60)
        .await
        .unwrap();
    assert_eq!(logs.items[0].task_id, "task-1");
    assert_eq!(logs.items[0].message, "hello");
    assert!(!logs.truncated);
    let stderr = client
        .filtered_environment_logs(
            &app.environment_id,
            Some("web"),
            5,
            60,
            Some(piqueld_client::LogStream::Stderr),
        )
        .await
        .unwrap();
    assert!(stderr.items.is_empty());
    for (tail, since) in [(0, 60), (1001, 60), (5, 0), (5, 86401)] {
        let error = client
            .environment_logs(&app.environment_id, Some("web"), tail, since)
            .await
            .unwrap_err();
        assert_eq!(api_error(error).status, 400);
    }
}

#[tokio::test]
async fn readiness_distinguishes_engine_reachability_and_does_not_gate_saves() {
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    assert!(api.client.system_readiness().await.unwrap().ready);
    api.runtime.set_unavailable(true);
    let readiness = api.client.system_readiness().await.unwrap();
    assert!(!readiness.ready);
    assert!(matches!(
        readiness.database,
        piqueld_core::api::DependencyStatus::Ready
    ));
    assert!(matches!(
        readiness.docker,
        piqueld_core::api::DependencyStatus::Ready
    ));
    assert!(
        matches!(readiness.swarm, piqueld_core::api::DependencyStatus::Failed { message } if message == "A compatible single-node Swarm manager is required")
    );
    let response = api.router().send(request("/api/v1/system/readiness")).await;
    assert_eq!(response.status, StatusCode::SERVICE_UNAVAILABLE);
    api.save().await;
    assert_eq!(api.client.system_status().await.unwrap().status, "running");
}

#[tokio::test]
async fn service_and_http_share_acceptance_receipts_and_application_views() {
    use piqueld::api::{ApplicationService, Mutation, MutationResponse};
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let service = ApplicationService::new(api.store.clone(), api.runtime.clone()).unwrap();
    let apply = new_application();
    // The same account as the HTTP client, since receipts replay only for theirs.
    let caller = piqueld::api::Actor::Account(piqueld::store::Caller {
        credential_id: "contract-admin",
        user_id: "contract-admin",
    });
    let MutationResponse::Saved(saved) = service
        .accept(
            caller,
            Mutation::save(
                apply.manifest.clone().validate_template().unwrap(),
                None,
                true,
            ),
            Some(0),
            false,
            Some("shared-command"),
        )
        .await
        .unwrap()
    else {
        panic!("expected saved configuration")
    };
    let replay = api
        .client
        .clone()
        .with_request_id("shared-command")
        .apply_and_deploy(&apply)
        .await
        .unwrap();
    assert_eq!(saved.operation_id, Some(replay.operation_id));
    assert_eq!(saved.generation, replay.generation);
    let id = piqueld_core::EnvironmentId::parse(&saved.application_id).unwrap();
    let direct = service
        .clone()
        .environment_detail(&id, &piqueld_core::access::Scope::All)
        .await
        .unwrap();
    let response = api_router(service.clone(), FakeAuth)
        .send(request(&format!("/api/v1/environments/{id}/detail")))
        .await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(
        response.json()["data"],
        serde_json::to_value(direct).unwrap()
    );

    let mut changed = apply.clone();
    changed.expected_generation = Some(saved.generation);
    changed.expected_application_id = Some(saved.application_id);
    changed.manifest.spec.services[0].replicas = 2.into();
    let saved = api
        .client
        .clone()
        .with_request_id("http-command")
        .apply_application(&changed)
        .await
        .unwrap();
    let MutationResponse::Saved(replay) = service
        .accept(
            caller,
            Mutation::save(
                changed.manifest.validate_template().unwrap(),
                changed.expected_application_id,
                false,
            ),
            changed.expected_generation,
            false,
            Some("http-command"),
        )
        .await
        .unwrap()
    else {
        panic!("expected receipt")
    };
    assert_eq!(replay.generation, saved.generation);
    assert_eq!(
        service
            .application(&piqueld_core::ApplicationId::parse(id.as_str()).unwrap())
            .await
            .unwrap()
            .generation,
        saved.generation
    );
}

#[tokio::test]
async fn direct_service_mutations_enforce_preconditions_and_explicit_force() {
    use piqueld::api::{ApplicationError, Mutation, MutationResponse};
    let temp = tempfile::tempdir().unwrap();
    let service = state(&temp).await;
    let manifest = manifest().validate_template().unwrap();
    let id = piqueld_core::ApplicationId::parse("absent-application").unwrap();
    let environment = piqueld_core::EnvironmentId::parse("absent-application").unwrap();
    for mutation in [
        Mutation::save(manifest.clone(), None, false),
        Mutation::deploy(environment.clone()),
        Mutation::Delete { id: environment },
        Mutation::Rename {
            id,
            name: "renamed".into(),
        },
    ] {
        assert!(matches!(
            service.accept(Daemon, mutation, None, false, None).await,
            Err(ApplicationError::PreconditionRequired)
        ));
    }
    assert!(
        service
            .applications(&piqueld_core::access::Scope::All, None, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    let MutationResponse::Saved(saved) = service
        .accept(
            Daemon,
            Mutation::save(manifest.clone(), None, false),
            Some(0),
            false,
            None,
        )
        .await
        .unwrap()
    else {
        panic!("expected saved configuration")
    };
    assert!(matches!(
        service
            .accept(
                Daemon,
                Mutation::save(manifest.clone(), None, false),
                Some(saved.generation),
                false,
                None,
            )
            .await,
        Err(ApplicationError::PreconditionRequired)
    ));
    let id = piqueld_core::EnvironmentId::parse(&saved.application_id).unwrap();
    assert!(matches!(
        service
            .accept(
                Daemon,
                Mutation::save(manifest.clone(), Some("wrong-application".into()), false),
                Some(saved.generation),
                false,
                None,
            )
            .await,
        Err(ApplicationError::Store(
            piqueld::store::StoreError::IdentityConflict
        ))
    ));
    // An explicit override uses the same path for every transport.
    service
        .accept(
            Daemon,
            Mutation::save(manifest, None, true),
            None,
            true,
            None,
        )
        .await
        .unwrap();
    service
        .accept(Daemon, Mutation::Reconcile { id }, None, false, None)
        .await
        .unwrap();
}

#[tokio::test]
async fn direct_service_validates_log_bounds_and_deployment_ownership() {
    use piqueld::api::{ApplicationError, Mutation, MutationResponse};
    let temp = tempfile::tempdir().unwrap();
    let service = state(&temp).await;
    let missing = piqueld_core::EnvironmentId::parse("absent-application").unwrap();
    for (name, tail, since) in [
        (None, 0, 60),
        (None, 1001, 60),
        (None, 5, 0),
        (None, 5, 86401),
        (Some(""), 5, 60),
    ] {
        assert!(matches!(
            service.logs(&missing, name, tail, since, None).await,
            Err(ApplicationError::InvalidLogQuery)
        ));
    }
    assert!(matches!(
        service.logs(&missing, Some("web"), 5, 60, None).await,
        Err(ApplicationError::Store(
            piqueld::store::StoreError::NotFound
        ))
    ));
    assert!(matches!(
        service
            .applications(&piqueld_core::access::Scope::All, None, Some(0))
            .await,
        Err(ApplicationError::InvalidPagination)
    ));
    let MutationResponse::Saved(saved) = service
        .accept(
            Daemon,
            Mutation::save(manifest().validate_template().unwrap(), None, true),
            Some(0),
            false,
            None,
        )
        .await
        .unwrap()
    else {
        panic!("expected deployment")
    };
    let deployment = saved.operation_id.unwrap();
    assert!(matches!(
        service
            .deployment_attempts(&missing, &deployment, None)
            .await,
        Err(ApplicationError::Store(
            piqueld::store::StoreError::NotFound
        ))
    ));
    let id = piqueld_core::EnvironmentId::parse(saved.application_id).unwrap();
    service
        .deployment_attempts(&id, &deployment, None)
        .await
        .unwrap();
    assert_eq!(
        service
            .logs(&id, Some("web"), 5, 60, None)
            .await
            .unwrap()
            .items[0]
            .message,
        "hello"
    );
}

/// Creating an application under the name of a repository-backed one is a
/// name collision, not an edit of it.
#[tokio::test]
async fn creating_over_a_repository_backed_name_collides() {
    use piqueld_client::{GitRepository, RepositoryManifest, edit::ApplicationEdit};
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let saved = api.save().await;
    let repository = ApplicationEdit::Repository(Some(RepositoryManifest {
        repository: GitRepository {
            url: "https://example.com/infra.git".into(),
            branch: "main".into(),
            commit: None,
        },
        path: "app.toml".into(),
        sync: piqueld_core::sync::RepositorySync::Off,
    }));
    let options = piqueld_client::edit::EditOptions {
        expected_generation: Some(1),
        ..Default::default()
    };
    api.client
        .edit_application(&saved.application_id, &repository, &options)
        .await
        .unwrap();
    // A runtime change, which would be an edit of the existing application.
    let mut request = new_application();
    request.manifest.spec.services[0].replicas = 2.into();
    let error = api.client.apply_application(&request).await.unwrap_err();
    assert_eq!(api_error(error).code, "application_name_collision");
}

/// The default analytics window starts 30 days before its end, but never
/// before the Unix epoch, which the daemon would refuse.
#[tokio::test]
async fn analytics_default_window_starts_no_earlier_than_the_epoch() {
    let temp = tempfile::tempdir().unwrap();
    let server = Server::tcp(router(state(&temp).await, FakeAuth)).await;
    let path = "/api/v1/analytics/deployments?until_ms=1000";
    let response = server.send(Method::GET, path, &[], Vec::new()).await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.json()["data"]["since_ms"], 0);
}
