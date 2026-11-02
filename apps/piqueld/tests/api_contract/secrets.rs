//! The stored secret API, secret key diagnostics and recovery.

use super::*;

/// Secret lifecycle history names secrets, never values. A stored secret no
/// environment deployed has no Docker secrets to clean up.
async fn assert_secret_history(client: &Client, application_id: &str) {
    let history = client
        .events(Some(application_id), None, None, 100)
        .await
        .unwrap()
        .items;
    let recorded = |kind: &str| {
        history
            .iter()
            .find(|event| event.kind == kind)
            .unwrap_or_else(|| panic!("{kind} recorded: {history:#?}"))
    };
    assert_eq!(recorded("secret_saved").resource.as_deref(), Some("token"));
    assert_eq!(
        recorded("secret_saved").message.as_deref(),
        Some("Stored secret version 1")
    );
    assert_eq!(
        recorded("secret_deleted").resource.as_deref(),
        Some("token")
    );
    assert!(
        !serde_json::to_string(&history)
            .unwrap()
            .contains("private-token-value")
    );
}

/// Rotating a stored secret with only `previews` is refused rather than
/// resetting its environments to every environment; `environments=all` sets
/// that explicitly.
#[tokio::test]
async fn stored_secret_access_queries_never_widen_environments_implicitly() {
    use piqueld_core::api::{EnvironmentAccess, SecretAccess};
    let temp = tempfile::tempdir().unwrap();
    let api = router(state(&temp).await, FakeAuth);
    let server = Server::tcp(api.clone()).await;
    let client = server.client();
    let app = create_and_inspect(&client, &manifest()).await;
    let none = SecretAccess {
        environments: EnvironmentAccess::Only([].into()),
        previews: false,
    };
    client
        .put_stored_secret(
            &app.environment_id,
            "token",
            0,
            b"one".to_vec(),
            Some(&none),
        )
        .await
        .unwrap();
    let put = |query: &str| api.send(put_stored_token(&app.environment_id, query, 1, "two"));
    put("previews=true")
        .await
        .assert_error(StatusCode::BAD_REQUEST, "query_invalid");
    let stored = client.stored_secrets(&app.environment_id).await.unwrap();
    assert_eq!(
        (stored[0].metadata.generation, &stored[0].access),
        (1, &none)
    );
    let response = put("environments=all&previews=true").await;
    assert_eq!(response.status, StatusCode::OK);
    let stored = client.stored_secrets(&app.environment_id).await.unwrap();
    assert_eq!(
        stored[0].access,
        SecretAccess {
            environments: EnvironmentAccess::All,
            previews: true
        }
    );
}

/// A raw write of `value` to the stored secret `token` of `application`,
/// replacing version `generation`, with the query string `query`, if any.
fn put_stored_token(
    application: &str,
    query: &str,
    generation: u64,
    value: &'static str,
) -> Request<Body> {
    let mut uri = format!("/api/v1/applications/{application}/secrets/token");
    if !query.is_empty() {
        uri = format!("{uri}?{query}");
    }
    Request::builder()
        .method("PUT")
        .uri(uri)
        .header("host", "localhost")
        .header("content-type", "application/octet-stream")
        .header("x-expected-generation", generation.to_string())
        .body(Body::from(value))
        .unwrap()
}

/// Secret uploads refuse a repeated `X-Expected-Generation` rather than
/// trusting the first, and the wrong content type before reading the body.
#[tokio::test]
async fn stored_secret_uploads_check_their_headers_first() {
    let temp = tempfile::tempdir().unwrap();
    let api = router(state(&temp).await, FakeAuth);
    let server = Server::tcp(api.clone()).await;
    let app = create_and_inspect(&server.client(), &manifest()).await;
    let mut repeated = put_stored_token(&app.environment_id, "", 0, "value");
    let headers = repeated.headers_mut();
    headers.append("x-expected-generation", "1".parse().unwrap());
    let response = api.send(repeated).await;
    response.assert_error(StatusCode::BAD_REQUEST, "secret_generation_required");
    // A body over the request limit would fail with 413 once read.
    let mut text = put_stored_token(&app.environment_id, "", 0, "value");
    text.headers_mut()
        .insert("content-type", "text/plain".parse().unwrap());
    let oversized = vec![b'x'; piqueld_core::api::MAX_REQUEST_BODY_BYTES + 1];
    *text.body_mut() = Body::from(oversized);
    let response = api.send(text).await;
    response.assert_error(StatusCode::UNSUPPORTED_MEDIA_TYPE, "secret_content_type");
}

#[tokio::test]
async fn stored_secret_api_is_write_only_versioned_and_access_listed() {
    use piqueld_client::edit::{ApplicationEdit, EditOptions, ServiceEdit};
    let temp = tempfile::tempdir().unwrap();
    let api = router(state(&temp).await, FakeAuth);
    let server = Server::tcp(api.clone()).await;
    let client = server.client();
    let app = create_and_inspect(&client, &manifest()).await;
    let response = api
        .send(put_stored_token(
            &app.environment_id,
            "",
            0,
            "private-token-value",
        ))
        .await;
    assert_eq!(response.status, StatusCode::OK);
    assert!(!response.text().contains("private-token-value"));
    let body = response.json();
    assert_eq!(body["data"]["generation"], 1);
    assert_eq!(body["data"]["access"]["environments"], "all");
    assert_eq!(body["data"]["access"]["previews"], false);
    assert_eq!(
        client
            .stored_secrets(&app.environment_id)
            .await
            .unwrap()
            .len(),
        1
    );
    let (client_ref, id) = (&client, &app.environment_id);
    let reference = |secrets, generation| async move {
        let edit = ApplicationEdit::Service {
            name: "web".into(),
            edit: ServiceEdit::Secrets(secrets),
        };
        let options = EditOptions {
            expected_generation: Some(generation),
            ..EditOptions::default()
        };
        client_ref
            .edit_application(id, &edit, &options)
            .await
            .unwrap()
    };
    let mount = piqueld_client::SecretMount {
        name: "token".into(),
        target: "/run/secrets/token".into(),
    };
    let saved = reference(vec![mount.clone()], app.generation).await;
    assert_eq!(
        client
            .application(&app.environment_id)
            .await
            .unwrap()
            .application
            .to_manifest()
            .spec
            .services[0]
            .secrets,
        vec![mount]
    );
    let referenced = client.delete_stored_secret(&app.environment_id, "token", 1);
    assert_eq!(api_error(referenced.await.unwrap_err()).status, 409);
    reference(Vec::new(), saved.generation).await;
    let stale = client.put_stored_secret(&app.environment_id, "token", 0, b"stale".to_vec(), None);
    assert_eq!(api_error(stale.await.unwrap_err()).status, 409);
    let absent = client.stored_secrets("app-absent").await;
    assert_eq!(api_error(absent.unwrap_err()).status, 404);
    client
        .delete_stored_secret(&app.environment_id, "token", 1)
        .await
        .unwrap();
    assert!(
        client
            .stored_secrets(&app.environment_id)
            .await
            .unwrap()
            .is_empty()
    );
    assert_secret_history(&client, &app.environment_id).await;
}

#[tokio::test]
async fn missing_secret_key_is_a_persisted_daemon_diagnostic() {
    let temp = tempfile::tempdir().unwrap();
    let server = Server::tcp(router(state(&temp).await, FakeAuth)).await;
    let client = server.client();
    let app = create_and_inspect(&client, &manifest()).await;
    client
        .put_stored_secret(&app.environment_id, "token", 0, b"value".to_vec(), None)
        .await
        .unwrap();
    std::fs::remove_file(temp.path().join("secrets.key")).unwrap();
    let error = client
        .put_stored_secret(
            &app.environment_id,
            "token",
            1,
            b"replacement".to_vec(),
            None,
        )
        .await
        .unwrap_err();
    let ClientError::Api { status, error, .. } = error else {
        panic!("API error");
    };
    assert_eq!(status.as_u16(), 503);
    assert_eq!(error.code, "secret_storage_unavailable");
    let event = client
        .diagnostic(error.details["diagnostic_id"].as_str().unwrap())
        .await
        .unwrap();
    let diagnostic = event.diagnostic.unwrap();
    assert_eq!(diagnostic.code, "secret_storage_unavailable");
    assert_eq!(
        diagnostic.scope,
        piqueld_core::observability::EventScope::Daemon
    );
    assert!(!diagnostic.retryable);
    assert_eq!(diagnostic.causes, ["Secret master key: missing"]);
    assert_eq!(event.request_id.as_deref(), Some(error.request_id.as_str()));
}

#[tokio::test]
async fn secret_cleanup_releases_writers_and_remains_reserved_after_runtime_failure() {
    use piqueld::{
        api::{ApplicationError, Mutation},
        store::StoreError,
    };
    use piqueld_core::edit::{ApplicationEdit, ServiceEdit};
    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(temp.path().join("db")).await.unwrap());
    let app = manifest()
        .validate_template()
        .unwrap()
        .normalize(piqueld_core::ApplicationId::parse("app-cleanup").unwrap());
    let environment = piqueld_core::EnvironmentId::default_for(app.id());
    store.save_application(&app, None, None).await.unwrap();
    let gate = Arc::new(CleanupGate::default());
    let runtime = Arc::new(FakeRuntime {
        cleanup_gate: Some(gate.clone()),
        ..FakeRuntime::new(&store)
    });
    runtime.set_unavailable(true);
    let service = ApiState::new(store.clone(), runtime.clone()).unwrap();
    store
        .put_secret(Daemon, &environment, "token", 0, b"value".to_vec())
        .await
        .unwrap();
    let cleanup = {
        let service = service.clone();
        let id = environment.clone();
        tokio::spawn(async move { service.delete_secret(Daemon, &id, "token", 1).await })
    };
    tokio::time::timeout(Duration::from_secs(2), gate.started.notified())
        .await
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(2),
        store.put_secret(Daemon, &environment, "other", 0, b"unrelated".to_vec()),
    )
    .await
    .unwrap()
    .unwrap();
    let edit = Mutation::Edit {
        id: app.id().clone(),
        edit: Box::new(ApplicationEdit::Service {
            name: "web".into(),
            edit: ServiceEdit::Secrets(vec![piqueld_core::manifest::SecretMount {
                name: "token".into(),
                target: "/run/secrets/token".into(),
            }]),
        }),
        deploy: false,
    };
    assert!(matches!(
        service.accept(Daemon, edit, Some(1), false, None).await,
        Err(ApplicationError::Store(StoreError::SecretDeleting))
    ));
    gate.release.notify_one();
    assert!(matches!(
        cleanup.await.unwrap(),
        Err(ApplicationError::Runtime(_))
    ));
    let failed = store
        .filtered_events(
            &piqueld_core::observability::EventFilter {
                kind: Some("action_failed".into()),
                ..Default::default()
            },
            &piqueld::store::Visibility::ALL,
            None,
            100,
        )
        .await
        .unwrap()
        .items;
    assert!(
        failed
            .iter()
            .any(|event| event.phase.as_deref() == Some("remove_secrets")
                && event.resource.as_deref() == Some("token")
                && event.environment_id.as_ref() == Some(&environment)
                && event.diagnostic.is_some()),
        "{failed:#?}"
    );
    assert!(
        service
            .secrets(&environment)
            .await
            .unwrap()
            .iter()
            .find(|s| s.name == "token")
            .unwrap()
            .deleting
    );
    runtime.set_unavailable(false);
    gate.release.notify_one();
    service
        .delete_secret(Daemon, &environment, "token", 1)
        .await
        .unwrap();
    assert_eq!(service.secrets(&environment).await.unwrap().len(), 1);
}

#[tokio::test]
async fn secret_key_recovery_api_discards_values_only_for_an_unusable_key() {
    let temp = tempfile::tempdir().unwrap();
    let store = Arc::new(Store::open(temp.path().join("db")).await.unwrap());
    seed_account(
        &temp.path().join("db"),
        "contract-admin",
        &[("admin", None)],
    )
    .await;
    let application = manifest()
        .validate_template()
        .unwrap()
        .normalize(piqueld_core::ApplicationId::parse("app-key-api").unwrap());
    store
        .save_application(&application, None, None)
        .await
        .unwrap();
    store
        .put_stored_secret(
            Daemon,
            application.id(),
            "token",
            0,
            b"private-value".to_vec(),
            None,
        )
        .await
        .unwrap();
    let runtime = Arc::new(FakeRuntime::new(&store));
    let server = Server::tcp(router(ApiState::new(store, runtime).unwrap(), FakeAuth)).await;
    let client = server.client();
    assert_eq!(
        api_error(client.recover_secret_key().await.unwrap_err()).status,
        409
    );
    let unavailable = || async {
        client
            .stored_secrets(application.id().as_str())
            .await
            .unwrap()[0]
            .metadata
            .unavailable
    };
    assert!(!unavailable().await);

    std::fs::remove_file(temp.path().join("secrets.key")).unwrap();
    let recovery = client.recover_secret_key().await.unwrap();
    assert_eq!(recovery.discarded_versions, 1);
    assert_eq!(recovery.affected_applications, 1);
    assert!(unavailable().await);
    client
        .put_stored_secret(
            application.id().as_str(),
            "token",
            1,
            b"new-value".to_vec(),
            None,
        )
        .await
        .unwrap();
    assert!(!unavailable().await);
}

#[tokio::test]
async fn deploys_and_previews_fail_when_an_environment_may_not_mount_a_stored_secret() {
    use piqueld_core::api::{EnvironmentAccess, SecretAccess};
    let temp = tempfile::tempdir().unwrap();
    let service = state(&temp).await;
    let (saved, staging) = two_environments(&service).await;
    let production = piqueld_core::EnvironmentId::parse(&saved.application_id).unwrap();
    let server = Server::tcp(router(service, FakeAuth)).await;
    let client = server.client();
    let production_only = SecretAccess {
        environments: EnvironmentAccess::Only([production.clone()].into()),
        previews: false,
    };
    let stored = client
        .put_stored_secret(
            &saved.application_id,
            "stripe-prod",
            0,
            b"live".to_vec(),
            Some(&production_only),
        )
        .await
        .unwrap();
    assert_eq!(stored.access, production_only);
    let mut manifest = manifest();
    manifest.spec.services[0]
        .secrets
        .push(piqueld_core::manifest::SecretMount {
            name: "stripe-prod".into(),
            target: "/run/secrets/stripe".into(),
        });
    let request = piqueld_client::ApplyApplicationRequest {
        manifest,
        expected_generation: Some(saved.generation),
        expected_application_id: Some(saved.application_id.clone()),
    };
    let saved = client.apply_application(&request).await.unwrap();
    let request = piqueld_client::ApplyApplicationRequest {
        expected_generation: None,
        ..request
    };
    let denied = |error: ClientError| {
        let ClientError::Api { status, error, .. } = error else {
            panic!("API error expected")
        };
        assert_eq!(status.as_u16(), 409);
        assert_eq!(error.code, "secret_access_denied");
        assert_eq!(
            error.details,
            serde_json::json!({"environment": "staging", "secret": "stripe-prod"})
        );
    };
    denied(
        client
            .plan_application(&request, Some(staging.id.as_str()))
            .await
            .unwrap_err(),
    );
    denied(
        client
            .deploy_environment(staging.id.as_str(), saved.generation, None)
            .await
            .unwrap_err(),
    );
    client
        .plan_application(&request, Some(production.as_str()))
        .await
        .unwrap();
    client
        .deploy_environment(production.as_str(), saved.generation, None)
        .await
        .unwrap();
}
