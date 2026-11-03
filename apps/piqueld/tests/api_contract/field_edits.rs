//! Typed field edits of saved applications: validation, revisions, receipts and history.

use super::*;

#[tokio::test]
async fn environment_visibility_edits_cap_routes_in_the_saved_manifest() {
    use piqueld_core::{
        edit::{ApplicationEdit, EditOptions},
        manifest::Visibility,
    };
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let saved = api.save().await;
    let id = &saved.application_id;
    let set = |visibility, generation| {
        let client = &api.client;
        async move {
            client
                .edit_application(
                    id,
                    &ApplicationEdit::EnvironmentVisibility {
                        environment: "production".into(),
                        visibility,
                    },
                    &EditOptions {
                        expected_generation: Some(generation),
                        ..EditOptions::default()
                    },
                )
                .await
                .unwrap()
        }
    };
    set(Visibility::Private, 1).await;
    let manifest = api
        .client
        .application(id)
        .await
        .unwrap()
        .application
        .to_manifest();
    assert_eq!(
        manifest.spec.environments["production"].visibility,
        Visibility::Private
    );
    // The block declares production, so it stays once back to the default.
    set(Visibility::Public, 2).await;
    let manifest = api
        .client
        .application(id)
        .await
        .unwrap()
        .application
        .to_manifest();
    assert_eq!(
        manifest.spec.environments["production"],
        piqueld_core::manifest::EnvironmentConfig::default()
    );
}

#[tokio::test]
async fn route_field_edits_follow_service_renames_and_removals() {
    use piqueld_client::{
        Route,
        edit::{ApplicationEdit, EditOptions, ServiceEdit},
    };
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let saved = api.save().await;
    let id = &saved.application_id;
    let route = Route::service(
        "notes.example.com".into(),
        piqueld_core::manifest::Visibility::Public,
        "web".into(),
        3000,
    );
    let options = |generation| EditOptions {
        expected_generation: Some(generation),
        ..EditOptions::default()
    };
    api.client
        .edit_application(
            id,
            &ApplicationEdit::Routes(vec![route.clone()]),
            &options(1),
        )
        .await
        .unwrap();
    let app = api.client.application(id).await.unwrap();
    assert_eq!(app.application.to_manifest().spec.routes, vec![route]);
    api.client
        .edit_application(
            id,
            &ApplicationEdit::Service {
                name: "web".into(),
                edit: ServiceEdit::Name("frontend".into()),
            },
            &options(2),
        )
        .await
        .unwrap();
    let app = api.client.application(id).await.unwrap();
    assert_eq!(
        app.application.to_manifest().spec.routes[0]
            .service
            .as_deref(),
        Some("frontend")
    );
    api.client
        .edit_application(
            id,
            &ApplicationEdit::RemoveService("frontend".into()),
            &options(3),
        )
        .await
        .unwrap();
    assert_eq!(
        api.client
            .application(id)
            .await
            .unwrap()
            .application
            .to_manifest()
            .spec
            .routes,
        [] as [piqueld_client::Route; 0]
    );
}

#[tokio::test]
async fn job_field_edit_replaces_jobs_in_order_and_validates_atomically() {
    use piqueld_client::{
        Job, JobRun, JobVolumeAccess,
        edit::{ApplicationEdit, EditOptions},
    };
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let saved = api.save().await;
    let id = &saved.application_id;
    let job = |name: &str, service: &str| Job {
        name: name.into(),
        service: service.into(),
        command: vec!["notes".into(), name.into()],
        environment: std::collections::BTreeMap::new(),
        secrets: Vec::new(),
        run: JobRun::BeforeRollout,
        timeout_seconds: 60,
        volume_access: JobVolumeAccess::ReadOnly,
        stop_service: false,
    };
    let options = |generation| EditOptions {
        expected_generation: Some(generation),
        ..EditOptions::default()
    };
    let jobs = vec![job("seed", "web"), job("migrate", "web")];
    api.client
        .edit_application(id, &ApplicationEdit::Jobs(jobs.clone()), &options(1))
        .await
        .unwrap();
    let jobs_of = async || {
        api.client
            .application(id)
            .await
            .unwrap()
            .application
            .to_manifest()
            .spec
            .jobs
    };
    assert_eq!(jobs_of().await, jobs);
    let error = api
        .client
        .edit_application(
            id,
            &ApplicationEdit::Jobs(vec![job("migrate", "missing")]),
            &options(2),
        )
        .await
        .unwrap_err();
    assert!(
        matches!(&error, ClientError::Api { status, error }
            if *status == http::StatusCode::UNPROCESSABLE_ENTITY
                && error.details.to_string().contains("job_service_missing")),
        "{error:?}"
    );
    assert_eq!(jobs_of().await, jobs);
    let events = api
        .client
        .events(Some(id), None, None, false, None, 100)
        .await
        .unwrap();
    let recorded = events
        .items
        .iter()
        .find(|event| event.kind == "application_edited")
        .unwrap();
    assert_eq!(recorded.phase.as_deref(), Some("jobs"));
}

#[tokio::test]
async fn field_edits_save_without_docker_and_deploy_only_the_captured_revision() {
    use piqueld_client::edit::{ApplicationEdit, EditOptions, ServiceEdit};
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    api.runtime.set_unavailable(true);
    let saved = api.save().await;
    let id = &saved.application_id;
    let edit = ApplicationEdit::Service {
        name: "web".into(),
        edit: ServiceEdit::Replicas(3.into()),
    };
    let options = EditOptions {
        expected_generation: Some(saved.generation),
        ..EditOptions::default()
    };
    let keyed = api.client.clone().with_request_id("field-save");
    let receipt = keyed.edit_application(id, &edit, &options).await.unwrap();
    assert_eq!(receipt.generation, 2);
    assert_eq!(receipt.operation_id, None);
    assert!(
        api.client
            .deployments(id, None, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    let app = api.client.application(id).await.unwrap();
    assert_eq!(
        app.application.to_manifest().spec.services[0].replicas,
        3.into()
    );
    assert_eq!(
        app.application.to_manifest().spec.services[0].source,
        manifest().spec.services[0].source
    );
    let stale = api
        .client
        .edit_application(id, &edit, &options)
        .await
        .unwrap_err();
    assert_eq!(api_error(stale).code, "generation_conflict");
    let replay = keyed.edit_application(id, &edit, &options).await.unwrap();
    assert_eq!(replay.generation, receipt.generation);
    let deploy = EditOptions {
        expected_generation: Some(2),
        deploy: true,
        force: false,
    };
    let next = ApplicationEdit::Service {
        name: "web".into(),
        edit: ServiceEdit::EnvironmentEntry(("MESSAGE".into(), Some("hello = world".into()))),
    };
    let deployed = api
        .client
        .edit_application(id, &next, &deploy)
        .await
        .unwrap();
    let operation = deployed.operation_id.unwrap();
    let pending = ApplicationEdit::Service {
        name: "web".into(),
        edit: ServiceEdit::Replicas(5.into()),
    };
    let options = EditOptions {
        expected_generation: Some(3),
        ..EditOptions::default()
    };
    api.client
        .edit_application(id, &pending, &options)
        .await
        .unwrap();
    let snapshot = api.deployed(id, &operation).await;
    assert_eq!(snapshot.spec().services[0].replicas, 3);
    assert_eq!(
        snapshot.spec().services[0].environment["MESSAGE"],
        "hello = world"
    );
    assert_eq!(
        api.client
            .application(id)
            .await
            .unwrap()
            .application
            .to_manifest()
            .spec
            .services[0]
            .replicas,
        5.into()
    );
}

#[tokio::test]
async fn field_edits_validate_atomically_and_track_repository_edits() {
    use piqueld_client::{
        GitRepository, RepositoryManifest,
        edit::{ApplicationEdit, EditOptions, ServiceEdit},
    };
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let saved = api.save().await;
    let id = &saved.application_id;
    let options = EditOptions {
        expected_generation: Some(1),
        ..EditOptions::default()
    };
    let invalid = ApplicationEdit::Service {
        name: "web".into(),
        edit: ServiceEdit::Replicas(0.into()),
    };
    let error = api
        .client
        .edit_application(id, &invalid, &options)
        .await
        .unwrap_err();
    assert!(
        matches!(error, ClientError::Api { status, error } if status == http::StatusCode::UNPROCESSABLE_ENTITY && !error.details.is_null())
    );
    assert_eq!(api.client.application(id).await.unwrap().generation, 1);
    let repository = ApplicationEdit::Repository(Some(RepositoryManifest {
        repository: GitRepository {
            url: "https://example.com/infra.git".into(),
            branch: "main".into(),
            commit: None,
        },
        path: "app.toml".into(),
        sync: piqueld_core::sync::RepositorySync::Off,
    }));
    api.client
        .edit_application(id, &repository, &options)
        .await
        .unwrap();
    let options = EditOptions {
        expected_generation: Some(2),
        force: true,
        deploy: false,
    };
    // Repository-backed configuration is edited like any other. Nothing was
    // read from the repository yet, so nothing counts as edited.
    let edit = ApplicationEdit::Service {
        name: "web".into(),
        edit: ServiceEdit::Replicas(2.into()),
    };
    api.client
        .edit_application(id, &edit, &options)
        .await
        .unwrap();
    api.client
        .edit_application(
            id,
            &ApplicationEdit::RepositoryPath("release.toml".into()),
            &options,
        )
        .await
        .unwrap();
    let edited = api.client.application(id).await.unwrap();
    assert_eq!(edited.application.spec().services[0].replicas, 2.into());
    assert!(edited.repository.is_none() && !edited.edited());
    api.client
        .edit_application(id, &ApplicationEdit::Repository(None), &options)
        .await
        .unwrap();
    let after = api.client.application(id).await.unwrap();
    assert_eq!(
        after.application.spec().services[0].replicas,
        2.into(),
        "disconnecting keeps edits"
    );
    assert!(after.application.spec().manifest.is_none());
}

#[tokio::test]
async fn concurrent_field_edits_reject_stale_revisions_without_losing_updates() {
    use piqueld_client::edit::{ApplicationEdit, EditOptions, ServiceEdit};
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let saved = api.save().await;
    let replicas = ApplicationEdit::Service {
        name: "web".into(),
        edit: ServiceEdit::Replicas(4.into()),
    };
    let env = ApplicationEdit::Service {
        name: "web".into(),
        edit: ServiceEdit::EnvironmentEntry(("X".into(), Some("y".into()))),
    };
    let options = EditOptions {
        expected_generation: Some(1),
        ..EditOptions::default()
    };
    let (a, b) = tokio::join!(
        api.client
            .edit_application(&saved.application_id, &replicas, &options),
        api.client
            .edit_application(&saved.application_id, &env, &options)
    );
    assert_ne!(a.is_ok(), b.is_ok());
    let (retry, error) = if let Err(error) = a {
        (&replicas, error)
    } else {
        (&env, b.unwrap_err())
    };
    assert_eq!(api_error(error).code, "generation_conflict");
    let options = EditOptions {
        expected_generation: Some(2),
        ..EditOptions::default()
    };
    api.client
        .edit_application(&saved.application_id, retry, &options)
        .await
        .unwrap();
    let manifest = api
        .client
        .application(&saved.application_id)
        .await
        .unwrap()
        .application
        .to_manifest();
    assert_eq!(manifest.spec.services[0].replicas, 4.into());
    assert_eq!(manifest.spec.services[0].environment["X"], "y");
}

#[tokio::test]
async fn field_edit_source_endpoints_preserve_other_git_settings() {
    use piqueld_client::{Build, GitRepository, SourceRepository, edit::ServiceEdit};
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let saved = api.save().await;
    let id = &saved.application_id;
    let source = Source::Git {
        repository: SourceRepository::Git(GitRepository {
            url: "https://example.com/first.git".into(),
            branch: "main".into(),
            commit: None,
        }),
        build: Build::Docker {
            dockerfile: "Dockerfile".into(),
            context: ".".into(),
            args: std::collections::BTreeMap::from([("ORIGIN".into(), "https://a".into())]),
            target: Some("runtime".into()),
        },
    };
    api.edit_service_field(id, ServiceEdit::Source(source))
        .await;
    for (edit, pointer, expected) in [
        (
            ServiceEdit::GitUrl("https://example.com/second.git".into()),
            "/source/repository/url",
            "https://example.com/second.git",
        ),
        (
            ServiceEdit::GitBranch("release".into()),
            "/source/repository/branch",
            "release",
        ),
        (
            ServiceEdit::Dockerfile("build/Dockerfile".into()),
            "/source/build/dockerfile",
            "build/Dockerfile",
        ),
        (
            ServiceEdit::Context("build".into()),
            "/source/build/context",
            "build",
        ),
    ] {
        let service = api.edit_service_field(id, edit).await;
        assert_eq!(
            serde_json::to_value(service)
                .unwrap()
                .pointer(pointer)
                .unwrap(),
            expected
        );
    }
    let pinned = api
        .edit_service_field(id, ServiceEdit::GitCommit(Some("a".repeat(40))))
        .await;
    let Source::Git {
        repository: SourceRepository::Git(repository),
        build,
    } = pinned.source
    else {
        panic!("Git source")
    };
    assert_eq!(repository.branch, "release");
    assert_eq!(repository.commit, Some("a".repeat(40)));
    assert_eq!(
        build,
        Build::Docker {
            dockerfile: "build/Dockerfile".into(),
            context: "build".into(),
            args: std::collections::BTreeMap::from([("ORIGIN".into(), "https://a".into())]),
            target: Some("runtime".into()),
        }
    );
    let unpinned = api
        .edit_service_field(id, ServiceEdit::GitCommit(None))
        .await;
    assert!(
        matches!(unpinned.source, Source::Git { repository: SourceRepository::Git(repository), .. } if repository.commit.is_none())
    );
    let image = api
        .edit_service_field(id, ServiceEdit::Image("nginx:stable".into()))
        .await;
    assert_eq!(
        image.source,
        Source::Image {
            image: "nginx:stable".into()
        }
    );
}

#[tokio::test]
async fn field_edit_health_process_and_resource_endpoints_clear_optional_values() {
    use piqueld_client::{HealthCheck, edit::ServiceEdit};
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let saved = api.save().await;
    let id = &saved.application_id;
    api.edit_service_field(
        id,
        ServiceEdit::Healthcheck(Some(HealthCheck::Http {
            port: 8080.into(),
            path: "/health".into(),
            interval_seconds: 10.into(),
            timeout_seconds: 3.into(),
            start_period_seconds: 0.into(),
            retries: 3.into(),
        })),
    )
    .await;
    api.edit_service_field(id, ServiceEdit::HealthPort(9090.into()))
        .await;
    api.edit_service_field(id, ServiceEdit::HealthPath("/live".into()))
        .await;
    api.edit_service_field(id, ServiceEdit::HealthInterval(20.into()))
        .await;
    api.edit_service_field(id, ServiceEdit::HealthStartPeriod(60.into()))
        .await;
    api.edit_service_field(id, ServiceEdit::HealthRetries(5.into()))
        .await;
    let health = api
        .edit_service_field(id, ServiceEdit::HealthTimeout(5.into()))
        .await;
    assert_eq!(
        health.healthcheck,
        Some(HealthCheck::Http {
            port: 9090.into(),
            path: "/live".into(),
            interval_seconds: 20.into(),
            timeout_seconds: 5.into(),
            start_period_seconds: 60.into(),
            retries: 5.into(),
        })
    );
    api.edit_service_field(
        id,
        ServiceEdit::Healthcheck(Some(HealthCheck::Command {
            command: vec!["true".into()],
            interval_seconds: 10.into(),
            timeout_seconds: 3.into(),
            start_period_seconds: 0.into(),
            retries: 3.into(),
        })),
    )
    .await;
    let health = api
        .edit_service_field(
            id,
            ServiceEdit::HealthCommand(vec!["check".into(), "--ready".into()]),
        )
        .await;
    assert!(
        matches!(health.healthcheck, Some(HealthCheck::Command { command, .. }) if command == ["check", "--ready"])
    );
    assert!(
        api.edit_service_field(id, ServiceEdit::Healthcheck(None))
            .await
            .healthcheck
            .is_none()
    );
    api.edit_service_field(id, ServiceEdit::Command(vec!["entrypoint".into()]))
        .await;
    let process = api
        .edit_service_field(
            id,
            ServiceEdit::Arguments(vec!["arg with spaces".into(), "--flag".into()]),
        )
        .await;
    assert_eq!(process.command, ["entrypoint"]);
    assert_eq!(process.arguments, ["arg with spaces", "--flag"]);
    api.edit_service_field(id, ServiceEdit::Cpu(Some(500.into())))
        .await;
    api.edit_service_field(id, ServiceEdit::Memory(Some(1024.into())))
        .await;
    let resources = api
        .edit_service_field(id, ServiceEdit::Cpu(None))
        .await
        .resources
        .unwrap();
    assert_eq!(resources.cpu_millis, None);
    assert_eq!(resources.memory_bytes, Some(1024.into()));
    assert!(
        api.edit_service_field(id, ServiceEdit::Memory(None))
            .await
            .resources
            .is_none()
    );
}

#[tokio::test]
async fn field_edit_resource_lifecycle_rejects_referenced_volume_removal() {
    use piqueld_client::{
        Mount, Volume,
        edit::{ApplicationEdit, EditOptions, ServiceEdit},
    };
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let saved = api.client.create_application("empty", false).await.unwrap();
    let id = &saved.application_id;
    assert!(saved.operation_id.is_none());
    api.edit_field(
        id,
        ApplicationEdit::AddService(Box::new(manifest().spec.services.remove(0))),
    )
    .await;
    api.edit_field(
        id,
        ApplicationEdit::AddVolume(Volume {
            name: "data".into(),
        }),
    )
    .await;
    let mount = Mount {
        volume: "data".into(),
        target: "/var/lib/data".into(),
        read_only: true,
    };
    let service = api
        .edit_service_field(id, ServiceEdit::Mount(mount.clone()))
        .await;
    assert_eq!(service.mounts, [mount]);
    let before = api.client.application(id).await.unwrap();
    let error = api
        .client
        .edit_application(
            id,
            &ApplicationEdit::RemoveVolume("data".into()),
            &EditOptions {
                expected_generation: Some(before.generation),
                ..EditOptions::default()
            },
        )
        .await
        .unwrap_err();
    assert!(
        matches!(error, ClientError::Api { status, .. } if status == http::StatusCode::UNPROCESSABLE_ENTITY)
    );
    assert_eq!(
        api.client.application(id).await.unwrap().generation,
        before.generation
    );
    api.edit_service_field(id, ServiceEdit::RemoveMount("/var/lib/data".into()))
        .await;
    assert_eq!(
        api.edit_field(id, ApplicationEdit::RemoveVolume("data".into()))
            .await
            .spec
            .volumes,
        [] as [piqueld_core::Volume; 0]
    );
    api.edit_service_field(
        id,
        ServiceEdit::EnvironmentEntry(("KEY".into(), Some("value".into()))),
    )
    .await;
    assert!(
        api.edit_service_field(id, ServiceEdit::EnvironmentEntry(("KEY".into(), None)))
            .await
            .environment
            .is_empty()
    );
    let renamed = api
        .edit_service_field(id, ServiceEdit::Name("worker".into()))
        .await;
    assert_eq!(renamed.name, "worker");
    assert_eq!(
        api.edit_field(id, ApplicationEdit::RemoveService("worker".into()))
            .await
            .spec
            .services,
        [] as [piqueld_core::Service; 0]
    );
    assert_eq!(
        api.edit_field(id, ApplicationEdit::Name("renamed".into()))
            .await
            .metadata
            .name,
        "renamed"
    );
}

#[tokio::test]
async fn field_edit_receipts_survive_restart_and_reject_key_reuse() {
    use piqueld_client::edit::{ApplicationEdit, EditOptions, ServiceEdit};
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let saved = api.save().await;
    let edit = ApplicationEdit::Service {
        name: "web".into(),
        edit: ServiceEdit::Replicas(3.into()),
    };
    let options = EditOptions {
        expected_generation: Some(1),
        ..EditOptions::default()
    };
    let receipt = api
        .client
        .clone()
        .with_request_id("field-save")
        .edit_application(&saved.application_id, &edit, &options)
        .await
        .unwrap();
    drop(api);
    let api = AcceptanceApi::start(&temp).await;
    let keyed = api.client.clone().with_request_id("field-save");
    let replay = keyed
        .edit_application(&saved.application_id, &edit, &options)
        .await
        .unwrap();
    assert_eq!(replay.generation, receipt.generation);
    let changed = ApplicationEdit::Service {
        name: "web".into(),
        edit: ServiceEdit::Replicas(5.into()),
    };
    let error = keyed
        .edit_application(&saved.application_id, &changed, &options)
        .await
        .unwrap_err();
    assert_eq!(api_error(error).code, "request_id_conflict");
}

#[tokio::test]
async fn field_edit_requires_an_explicit_value_and_revision() {
    let temp = tempfile::tempdir().unwrap();
    let state = state(&temp).await;
    let saved = save(&state, manifest()).await;
    let app = router(state, FakeAuth);
    for (query, body, status) in [
        ("?expected_generation=1", "{}", StatusCode::BAD_REQUEST),
        (
            "?expected_generation=1",
            r#"{"value":500,"unexpected":true}"#,
            StatusCode::BAD_REQUEST,
        ),
        ("", r#"{"value":500}"#, StatusCode::BAD_REQUEST),
        (
            "?expected_generation=1",
            r#"{"value":null}"#,
            StatusCode::OK,
        ),
    ] {
        let response = app
            .send(
                Request::builder()
                    .method(Method::PUT)
                    .uri(format!(
                        "/api/v1/applications/{}/services/web/resources/cpu{query}",
                        saved.application_id
                    ))
                    .header("host", "localhost")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await;
        assert_eq!(response.status, status, "body {body}");
    }
}

#[tokio::test]
async fn typed_edits_record_safe_field_history() {
    use piqueld_client::edit::{ApplicationEdit, EditOptions, ServiceEdit};
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let saved = api.save().await;
    let edit = ApplicationEdit::Service {
        name: "web".into(),
        edit: ServiceEdit::EnvironmentEntry(("SECRET".into(), Some("private-value".into()))),
    };
    api.client
        .edit_application(
            &saved.application_id,
            &edit,
            &EditOptions {
                expected_generation: Some(saved.generation),
                ..EditOptions::default()
            },
        )
        .await
        .unwrap();
    let events = api
        .client
        .events(Some(&saved.application_id), None, None, false, None, 100)
        .await
        .unwrap();
    let recorded = events
        .items
        .iter()
        .find(|e| e.kind == "application_edited")
        .unwrap();
    assert_eq!(recorded.phase.as_deref(), Some("service"));
    assert_eq!(recorded.resource.as_deref(), Some("web"));
    assert_eq!(recorded.generation, Some(2));
    assert!(
        !serde_json::to_string(recorded)
            .unwrap()
            .contains("private-value")
    );
}
