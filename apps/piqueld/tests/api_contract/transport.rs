//! Transport-level behaviour shared by every endpoint: clients over TCP and the Unix socket, origin checks, structured errors and request limits.

use super::*;

#[tokio::test]
async fn typed_client_exercises_polling_lifecycle_over_tcp() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let server = Server::tcp(router(state(&temp).await, FakeAuth)).await;
    let client = server.client();

    assert!(
        client
            .builds(None, None, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    let missing = client.build_logs(999, None, None).await.unwrap_err();
    assert_eq!(api_error(missing).status, 404);
    assert_lifecycle(&client).await;
}

/// Plans, creates and inspects, replaces, and deletes the contract
/// application through `client`.
async fn assert_lifecycle(client: &Client) {
    let manifest = manifest();
    assert_create_plan(client, &manifest).await;
    let created = create_and_inspect(client, &manifest).await;
    let replaced = replace_and_plan(client, &created, manifest).await;
    assert_ne!(created.operation_id, replaced.operation_id);
    delete(client, &replaced).await;
}

async fn assert_create_plan(client: &Client, manifest: &ApplicationManifest) {
    let preview = client
        .plan_application(
            &ApplyApplicationRequest {
                expected_generation: None,
                expected_application_id: None,
                manifest: manifest.clone(),
            },
            None,
        )
        .await
        .expect("create preview succeeds");
    assert!(matches!(
        preview.plan.actions.as_slice(),
        [action] if matches!(action.kind, ActionKind::ResolveImage { .. })
    ));
}

async fn replace_and_plan(
    client: &Client,
    created: &AcceptedOperation,
    mut manifest: ApplicationManifest,
) -> AcceptedOperation {
    manifest.spec.services[0].replicas = 2.into();
    let request = ApplyApplicationRequest {
        expected_generation: Some(created.generation),
        expected_application_id: Some(created.environment_id.clone()),
        manifest,
    };
    let replaced = client
        .apply_and_deploy(&request)
        .await
        .expect("replacement succeeds");
    assert_eq!(replaced.environment_id, created.environment_id);
    let mut preview_request = request;
    preview_request.expected_generation = Some(replaced.generation);
    client
        .plan_application(&preview_request, None)
        .await
        .expect("preview succeeds");
    replaced
}

#[tokio::test]
async fn transport_failures_are_structured_safe_and_request_ids_pair() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let state = state(&temp).await;
    let servers = [
        Server::tcp(router(state.clone(), FakeAuth)).await,
        Server::unix(api_router(state, FakeAuth), &temp.path().join("api.sock")),
    ];
    for server in &servers {
        assert_transport_failures(server).await;
        server.assert_running();
    }
}

/// Asserts `server` refuses oversized, misrouted, and malformed requests
/// with structured errors, and pairs response bodies' request IDs with
/// their headers.
async fn assert_transport_failures(server: &Server) {
    let huge = format!(
        "{{\"manifest\": {{\"padding\": \"{}\"}}}}",
        "x".repeat(3 * 1024 * 1024)
    );
    let json = [("content-type", "application/json")];
    let too_large = server
        .send(
            Method::POST,
            "/api/v1/applications/apply",
            &json,
            huge.into_bytes(),
        )
        .await;
    too_large.assert_error(StatusCode::PAYLOAD_TOO_LARGE, "request_body_too_large");
    let malformed = server
        .send(
            Method::POST,
            "/api/v1/applications/plan",
            &json,
            b"{\"manifest\": {\"broken\"".to_vec(),
        )
        .await;
    malformed.assert_error(StatusCode::BAD_REQUEST, "json_malformed");

    for (method, path, status, code) in [
        (
            Method::GET,
            "/api/v1/does-not-exist",
            StatusCode::NOT_FOUND,
            "endpoint_not_found",
        ),
        (
            Method::PUT,
            "/api/v1/applications",
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
        ),
        (
            Method::GET,
            "/api/v1/applications?cursor=bogus",
            StatusCode::BAD_REQUEST,
            "pagination_invalid",
        ),
        (
            Method::POST,
            "/api/v1/environments/doesnotexist1/deploy?branch=a&branch=b",
            StatusCode::BAD_REQUEST,
            "query_invalid",
        ),
    ] {
        let response = server.send(method, path, &[], Vec::new()).await;
        assert_eq!((response.status, response.code().as_str()), (status, code));
    }
    for page in ["limit=0", "limit=101"] {
        let path = format!("/api/v1/applications?{page}");
        let response = server.send(Method::GET, &path, &[], Vec::new()).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST, "{page}");
    }

    let paired = server
        .send(
            Method::GET,
            "/api/v1/applications/doesnotexist1",
            &[],
            Vec::new(),
        )
        .await;
    assert_eq!(paired.status, StatusCode::NOT_FOUND);
    let header_id = paired.header("x-request-id").expect("request id header");
    assert_eq!(paired.json()["request_id"].as_str(), Some(header_id));
}

#[tokio::test]
async fn method_not_allowed_advertises_only_the_matched_route_methods() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let server = Server::tcp(router(state(&temp).await, FakeAuth)).await;

    // The Allow header advertises only the methods registered for the matched
    // route; Axum serves HEAD automatically for every GET route.
    for (method, path, allowed) in [
        (Method::PUT, "/api/v1/applications", "GET, HEAD, POST"),
        (
            Method::POST,
            "/api/v1/applications/app-notes-01",
            "GET, HEAD, DELETE",
        ),
        (Method::POST, "/health", "GET, HEAD"),
    ] {
        let response = server.send(method, path, &[], Vec::new()).await;
        assert_eq!(response.status, StatusCode::METHOD_NOT_ALLOWED, "{path}");
        assert_eq!(
            response.header(http::header::ALLOW),
            Some(allowed),
            "{path}"
        );
    }
}

#[tokio::test]
async fn manifest_validation_media_types_and_unknown_fields_are_rejected_safely() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let server = Server::tcp(router(state(&temp).await, FakeAuth)).await;

    let invalid_manifest = r#"
api_version = "piqueld.dev/v1alpha1"
kind = "Application"
[metadata]
name = "notes"
[[spec.services]]
name = "web"
[spec.services.source]
type = "image"
image = ""
"#;
    let validation = server
        .send(
            Method::POST,
            "/api/v1/applications/plan",
            &[("content-type", "application/toml")],
            invalid_manifest.as_bytes().to_vec(),
        )
        .await;
    validation.assert_error(
        StatusCode::UNPROCESSABLE_ENTITY,
        "manifest_validation_failed",
    );
    let malformed_toml = server
        .send(
            Method::POST,
            "/api/v1/applications/plan",
            &[("content-type", "text/toml")],
            b"api_version = ".to_vec(),
        )
        .await;
    malformed_toml.assert_error(StatusCode::BAD_REQUEST, "toml_malformed");
    let body = malformed_toml.json();
    let message = body["details"]["errors"][0]["message"]
        .as_str()
        .unwrap_or_default();
    assert!(
        message.contains(" at line 1 column ") && !message.contains('\n'),
        "{message}"
    );
    let unknown_field = serde_json::json!({
        "manifest": manifest(),
        "surprise": true
    });
    let unknown = server
        .send(
            Method::POST,
            "/api/v1/applications/apply",
            &[("content-type", "application/json")],
            serde_json::to_vec(&unknown_field).expect("serializes"),
        )
        .await;
    unknown.assert_error(StatusCode::BAD_REQUEST, "json_malformed");
}

#[tokio::test]
async fn tcp_rejects_untrusted_authorities() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let server = Server::tcp(piqueld::api::http::web_router_with_hosts(
        state(&temp).await,
        UiAssets::Disabled,
        FakeAuth,
        vec!["daemon.example.ts.net".into()],
    ))
    .await;
    for (host, expected) in [
        ("attacker.example", StatusCode::FORBIDDEN),
        ("127.0.0.1:7845", StatusCode::OK),
        ("localhost:7845", StatusCode::OK),
        ("[::1]:7845", StatusCode::OK),
        ("[::1]attacker.example", StatusCode::FORBIDDEN),
        ("daemon.example.ts.net:7845", StatusCode::OK),
        (
            "daemon.example.ts.net.attacker.example",
            StatusCode::FORBIDDEN,
        ),
    ] {
        let response = server
            .send(
                Method::GET,
                "/api/v1/system/status",
                &[("host", host)],
                Vec::new(),
            )
            .await;
        assert_eq!(response.status, expected, "host {host}");
        assert!(response.headers.contains_key("x-request-id"));
    }
}

#[tokio::test]
async fn tcp_login_uses_canonical_origin_behind_https_proxy() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp).await;
    let store = Store::open(database(&temp)).await.unwrap();
    let origin = "https://daemon.example.ts.net:8443";
    let auth = piqueld::auth::Auth::new(&store, origin).unwrap();
    let server = Server::tcp(piqueld::api::http::web_router_with_hosts(
        state,
        UiAssets::Disabled,
        auth,
        vec!["daemon.example.ts.net".into()],
    ))
    .await;
    // A TLS proxy forwards an ordinary HTTP request, retaining the browser's
    // HTTPS Origin. Its upstream Host may preserve the name or use localhost.
    for (host, browser_origin, site, expected_error) in [
        (
            "daemon.example.ts.net:8443",
            Some(origin),
            "same-origin",
            None,
        ),
        ("localhost:7845", Some(origin), "same-origin", None),
        (
            "attacker.example",
            Some(origin),
            "same-origin",
            Some("browser_access_denied"),
        ),
        (
            "localhost",
            Some(origin),
            "cross-site",
            Some("browser_access_denied"),
        ),
        (
            "localhost",
            Some("https://attacker.example"),
            "same-origin",
            Some("origin_mismatch"),
        ),
        (
            "localhost",
            Some("http://daemon.example.ts.net:8443"),
            "same-origin",
            Some("origin_mismatch"),
        ),
        (
            "localhost",
            Some("https://daemon.example.ts.net"),
            "same-origin",
            Some("origin_mismatch"),
        ),
        (
            "localhost",
            Some("null"),
            "same-origin",
            Some("origin_mismatch"),
        ),
        ("localhost", None, "same-origin", Some("origin_mismatch")),
    ] {
        let mut headers = vec![
            ("host", host),
            ("sec-fetch-site", site),
            // Forwarded headers must neither be required nor override policy.
            ("x-forwarded-host", "attacker.example"),
            ("x-forwarded-proto", "http"),
        ];
        if let Some(origin) = browser_origin {
            headers.push(("origin", origin));
        }
        let response = server
            .send(
                Method::POST,
                "/api/v1/auth/login/start",
                &headers,
                Vec::new(),
            )
            .await;
        if let Some(code) = expected_error {
            response.assert_error(StatusCode::FORBIDDEN, code);
        } else {
            assert_eq!(response.status, StatusCode::OK, "headers {headers:?}");
            assert!(response.header("set-cookie").unwrap().contains("Secure"));
            serde_json::from_value::<piqueld_core::auth::Ceremony>(response.json()).unwrap();
        }
    }
    // The browser trust policy runs before authentication, so an untrusted
    // host learns nothing about the route's credentials. Like every API
    // response, the refusal is never cached.
    let response = server
        .send(
            Method::GET,
            "/api/v1/system/status",
            &[("host", "attacker.example")],
            Vec::new(),
        )
        .await;
    response.assert_error(StatusCode::FORBIDDEN, "browser_access_denied");
    assert_eq!(response.header("cache-control"), Some("no-store"));
}

#[tokio::test]
async fn tcp_rejects_cross_site_mutations_without_creating_deployments() {
    let temp = tempfile::tempdir().unwrap();
    let server = Server::tcp(router(state(&temp).await, FakeAuth)).await;
    // Apply is a real POST handler, so successful controls prove the
    // middleware allows same-origin and non-browser clients through.
    for (headers, expected) in [
        (
            vec![("sec-fetch-site", "cross-site")],
            StatusCode::FORBIDDEN,
        ),
        (vec![("sec-fetch-site", "same-site")], StatusCode::FORBIDDEN),
        (
            vec![
                ("origin", "http://localhost"),
                ("sec-fetch-site", "same-origin"),
            ],
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ),
        (
            vec![("host", "localhost:80"), ("origin", "http://localhost")],
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
        ),
        (vec![], StatusCode::UNSUPPORTED_MEDIA_TYPE),
    ] {
        let response = server
            .send(
                Method::POST,
                "/api/v1/applications/apply",
                &headers,
                Vec::new(),
            )
            .await;
        assert_eq!(response.status, expected, "headers {headers:?}");
    }
    let client = server.client();
    let saved = client.apply_application(&new_application()).await.unwrap();
    for action in ["deploy", "reconcile"] {
        let response = server
            .send(
                Method::POST,
                &format!(
                    "/api/v1/applications/{}/{action}?force=true",
                    saved.application_id
                ),
                &[
                    ("sec-fetch-site", "cross-site"),
                    ("content-type", "application/x-www-form-urlencoded"),
                ],
                Vec::new(),
            )
            .await;
        response.assert_error(StatusCode::FORBIDDEN, "browser_access_denied");
    }
    assert!(
        client
            .deployments(&saved.application_id, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
}

#[tokio::test]
async fn typed_edits_enforce_the_tcp_browser_policy() {
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let client = &api.client;
    let saved = api.save().await;
    for (method, field, body) in [
        (
            Method::PUT,
            "services/web/replicas",
            br#"{"value":3}"#.to_vec(),
        ),
        (Method::DELETE, "services/web", Vec::new()),
    ] {
        let response = api
            .server
            .send(
                method,
                &format!(
                    "/api/v1/applications/{}/{field}?force=true&deploy=true",
                    saved.application_id
                ),
                &[
                    ("sec-fetch-site", "cross-site"),
                    ("content-type", "application/json"),
                ],
                body,
            )
            .await;
        response.assert_error(StatusCode::FORBIDDEN, "browser_access_denied");
    }
    let unchanged = client.application(&saved.application_id).await.unwrap();
    assert_eq!(unchanged.generation, saved.generation);
    assert_eq!(unchanged.application.spec().services.len(), 1);
    assert!(
        client
            .deployments(&saved.application_id, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    let edit = api
        .server
        .send(
            Method::PUT,
            &format!(
                "/api/v1/applications/{}/services/web/replicas?expected_generation=1",
                saved.application_id
            ),
            &[
                ("origin", "http://localhost"),
                ("sec-fetch-site", "same-origin"),
                ("content-type", "application/json"),
            ],
            br#"{"value":3}"#.to_vec(),
        )
        .await;
    assert_eq!(edit.status, StatusCode::OK);
    let edited = client.application(&saved.application_id).await.unwrap();
    assert_eq!(edited.generation, saved.generation + 1);
    assert_eq!(edited.application.spec().services[0].replicas, 3.into());
}

#[tokio::test]
async fn typed_client_exercises_the_lifecycle_over_a_unix_socket() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let socket = temp.path().join("contract.sock");
    let server = Server::unix(router(state(&temp).await, FakeAuth), &socket);
    let client = server.client();

    let status = client.system_status().await.expect("status over unix");
    assert_eq!(status.api_version, "v1");
    assert_lifecycle(&client).await;
}

#[tokio::test]
async fn diagnostic_ids_correlate_api_failures() {
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let app = api.deploy().await;
    api.runtime.set_unavailable(true);
    let error = api
        .client
        .environment_logs(&app.environment_id, Some("web"), 5, 60)
        .await
        .unwrap_err();
    let ClientError::Api { error, .. } = error else {
        panic!("API error");
    };
    let id = error.details["diagnostic_id"].as_str().unwrap();
    let event = api.client.diagnostic(id).await.unwrap();
    assert_eq!(event.request_id.as_deref(), Some(error.request_id.as_str()));
    assert_eq!(
        event.environment_id.as_ref().map(ToString::to_string),
        Some(app.environment_id.clone())
    );
    assert_eq!(event.scope, piqueld_core::observability::EventScope::Daemon);
    let errors = api
        .client
        .filtered_events(
            &piqueld_client::observability::EventFilter {
                errors_only: true,
                ..Default::default()
            },
            None,
            100,
        )
        .await
        .unwrap();
    assert!(
        errors
            .items
            .iter()
            .any(|e| e.diagnostic.as_ref().is_some_and(|d| d.id == id))
    );
    assert!(api.client.daemon_stats().await.unwrap().diagnostics > 0);
}

/// Expensive requests beyond their kind's budget answer 503 `server_busy`
/// at once, without recording a diagnostic, until the budget frees up.
#[tokio::test]
async fn expensive_requests_beyond_their_budget_are_refused() {
    let temp = tempfile::tempdir().unwrap();
    let state = state(&temp).await;
    let server = Server::tcp(router(state.clone(), FakeAuth)).await;
    let analytics = async || {
        let path = "/api/v1/analytics/deployments";
        server.send(Method::GET, path, &[], Vec::new()).await
    };
    let analysing = piqueld::api::Expensive::Analytics;
    let held: Vec<_> = std::iter::from_fn(|| state.admit(analysing).ok()).collect();
    let refused = analytics().await;
    refused.assert_error(StatusCode::SERVICE_UNAVAILABLE, "server_busy");
    assert!(refused.json()["details"].get("diagnostic_id").is_none());
    drop(held);
    assert_eq!(analytics().await.status, StatusCode::OK);
}

#[tokio::test]
async fn invalid_path_parameters_return_correlated_json_errors() {
    let temp = tempfile::tempdir().unwrap();
    let application = api_router(state(&temp).await, FakeAuth);
    for (method, path) in [
        (Method::GET, "/api/v1/builds/not-a-number/logs"),
        (Method::GET, "/api/v1/diagnostics/%FF"),
        (Method::POST, "/api/v1/notifications/deliveries/%FF/retry"),
        (Method::GET, "/api/v1/environments/%FF/secrets"),
        (Method::PUT, "/api/v1/applications/app-test/secrets/%FF"),
        (Method::DELETE, "/api/v1/environments/app-test/secrets/%FF"),
        (Method::GET, "/api/v1/applications/%FF"),
        (
            Method::PUT,
            "/api/v1/applications/app-test/services/%FF/replicas",
        ),
        (
            Method::PUT,
            "/api/v1/applications/app-test/services/web/environment/%FF",
        ),
        (Method::DELETE, "/api/v1/applications/%FF/repository"),
        (Method::DELETE, "/api/v1/applications/app-test/services/%FF"),
        (Method::DELETE, "/api/v1/applications/app-test/volumes/%FF"),
        (
            Method::DELETE,
            "/api/v1/applications/app-test/services/web/environment/%FF",
        ),
    ] {
        let response = application.send(request_as(method, path)).await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
        assert_eq!(response.header("content-type"), Some("application/json"));
        let error: piqueld_core::api::ErrorBody = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(error.code, "path_invalid");
        assert_eq!(
            Some(error.request_id.as_str()),
            response.header("x-request-id")
        );
    }
}
