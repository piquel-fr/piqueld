//! Focused API/client coverage for the polling application lifecycle.

use async_trait::async_trait;
use axum::{body::Body, http::Request, serve};
use http_body_util::BodyExt;
use piqueld::api::Actor::Daemon;
use piqueld::api::http::{
    ApiState, Authenticator, EmbeddedBundle, UiAssets, api_router, router, web_router,
};
use piqueld::application::{BoundaryError, RuntimeBoundary};
use piqueld::store::{Store, StoredEnvironment};
use piqueld_client::{AcceptedOperation, ApplyApplicationRequest, Client};
use piqueld_core::{
    InstanceId, NormalizedApplication, ObservedApplication, ResolutionSet, compile_application,
    manifest::{ApplicationManifest, ApplicationTemplate, Source, ValidatedSource},
    planner::ActionKind,
    resource::{ResolvedSource, image_repository},
};
use std::{collections::BTreeMap, future::IntoFuture, sync::Arc};
use tempfile::TempDir;
use tokio::net::TcpListener;
use tower::ServiceExt;

/// Signs every request in as an administrator, so contract tests exercise the
/// API without signing in. Authentication and authorization are covered with
/// the real passkey service below.
#[derive(Clone)]
struct FakeAuth;

impl Authenticator for FakeAuth {
    fn guard<S: Clone + Send + Sync + 'static>(self, router: axum::Router<S>) -> axum::Router<S> {
        router.layer(axum::Extension(piqueld::auth::Identity {
            user: piqueld_core::auth::User {
                id: "contract-admin".into(),
                username: "admin".into(),
                display_name: String::new(),
            },
            credential_id: "contract-admin".into(),
            kind: piqueld::store::CredentialKind::Token,
            grants: piqueld_core::access::Grants::admin(),
            scoped: false,
        }))
    }
}

#[derive(Default)]
struct CleanupGate {
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

struct FakeRuntime {
    cleanup_gate: Option<Arc<CleanupGate>>,
    instance: InstanceId,
    unavailable: std::sync::atomic::AtomicBool,
}

#[async_trait]
impl RuntimeBoundary for FakeRuntime {
    async fn logs(
        &self,
        _application: &piqueld_core::EnvironmentId,
        service: Option<&str>,
        tail: u16,
        since: u32,
        stream: Option<piqueld_core::api::LogStream>,
    ) -> Result<piqueld_core::api::ApplicationLogs, BoundaryError> {
        self.check_available().await?;
        assert_eq!((service, tail, since), (Some("web"), 5, 60));
        if stream == Some(piqueld_core::api::LogStream::Stderr) {
            return Ok(piqueld_core::api::ApplicationLogs::default());
        }
        Ok(piqueld_core::api::ApplicationLogs {
            items: vec![piqueld_core::api::LogRecord {
                service: "web".into(),
                task_id: "task-1".into(),
                timestamp: "2026-09-13T12:00:00Z".into(),
                stream: "stdout".into(),
                message: "hello".into(),
            }],
            truncated: false,
        })
    }

    /// Only `web` has a running task.
    async fn create_exec(
        &self,
        _environment: &piqueld_core::EnvironmentId,
        request: &piqueld_core::exec::ExecRequest,
    ) -> Result<Option<piqueld::docker::Exec>, BoundaryError> {
        Ok(
            (request.service.as_str() == "web").then(|| piqueld::docker::Exec {
                id: "exec-1".into(),
                task: "task-1".into(),
                tty: request.tty.is_some(),
            }),
        )
    }

    /// Echoes standard input, then reports how many bytes it read as the exit code.
    async fn run_exec(
        &self,
        _exec: &piqueld::docker::Exec,
        mut io: piqueld::docker::ExecIo,
    ) -> Result<i64, BoundaryError> {
        use piqueld_core::exec::{ExecInput, ExecOutput};
        let mut read = 0;
        while let Some(ExecInput::Stdin(data)) = io.input.recv().await {
            read += data.len();
            io.output.send(ExecOutput::Stdout(data)).await.unwrap();
        }
        io.output
            .send(ExecOutput::Stderr(b"closed".to_vec()))
            .await
            .unwrap();
        Ok(i64::try_from(read).unwrap())
    }

    fn trigger_reconciliation(&self) {}

    async fn readiness(&self) -> (bool, bool) {
        (
            true,
            !self.unavailable.load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    async fn remove_secrets(
        &self,
        _application: &piqueld_core::EnvironmentId,
        _names: &[String],
    ) -> Result<(), BoundaryError> {
        if let Some(gate) = &self.cleanup_gate {
            gate.started.notify_one();
            gate.release.notified().await;
        }
        self.check_available().await
    }

    async fn prepare(
        &self,
        environment: &piqueld_core::EnvironmentId,
        application: &NormalizedApplication,
        _reusable: &piqueld_core::ResolutionSet,
    ) -> Result<piqueld_core::ResolvedApplication, BoundaryError> {
        let sources = application
            .spec()
            .services
            .iter()
            .map(|service| {
                let ValidatedSource::Image { image } = &service.source else {
                    panic!("expected image fixture")
                };
                let repository =
                    image_repository(image).expect("validated fixture image has a repository");
                (
                    service.name.clone(),
                    ResolvedSource::parse_image(
                        image.clone(),
                        format!("{repository}@sha256:{}", "a".repeat(64)),
                    )
                    .unwrap(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        let resolved = compile_application(
            application,
            environment,
            self.instance.clone(),
            &ResolutionSet {
                sources,
                secret_names: BTreeMap::default(),
            },
        )
        .map_err(BoundaryError::Compilation)?;
        Ok(resolved)
    }

    async fn check_available(&self) -> Result<(), BoundaryError> {
        if self.unavailable.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(piqueld::docker::DockerError::Unavailable("observe application").into());
        }
        Ok(())
    }

    async fn observe(
        &self,
        _application: &StoredEnvironment,
    ) -> Result<ObservedApplication, BoundaryError> {
        self.check_available().await?;
        Ok(ObservedApplication::default())
    }
}

fn manifest() -> ApplicationManifest {
    serde_json::from_value(serde_json::json!({
        "api_version": "piqueld.dev/v1alpha1",
        "kind": "Application",
        "metadata": {"name": "notes"},
        "spec": {"services": [{
            "name": "web",
            "source": {"type": "image", "image": "ghcr.io/example/notes:1"}
        }]}
    }))
    .expect("fixture is valid")
}

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
        unavailable: std::sync::atomic::AtomicBool::new(false),
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

async fn state(temp: &TempDir) -> ApiState {
    let store = Arc::new(
        Store::open(temp.path().join("state.db"))
            .await
            .expect("fresh database opens"),
    );
    // `FakeAuth` signs requests in with this administrator's credential,
    // which mutations re-read from the database.
    seed_account(
        &temp.path().join("state.db"),
        "contract-admin",
        &[("admin", None)],
    )
    .await;
    let instance = InstanceId::parse(store.instance_id().to_owned()).expect("valid instance ID");
    ApiState::new(
        Arc::clone(&store),
        Arc::new(FakeRuntime {
            cleanup_gate: None,
            instance,
            unavailable: std::sync::atomic::AtomicBool::new(false),
        }),
    )
}

/// Stand-in for the compile-time bundle: a shell, unhashed assets (including
/// a short all-hex stem that must not read as a digest), and a content-hashed
/// asset, mirroring what the build script emits.
static TEST_BUNDLE: &EmbeddedBundle = &[
    ("added.css", b"body{}" as &'static [u8]),
    ("app.js", b"console.log('dashboard');" as &'static [u8]),
    (
        "index.html",
        b"<!doctype html><html><body><main>dashboard-shell</main></body></html>" as &'static [u8],
    ),
    ("piqueld-ui-a1b2c3d4_bg.wasm", b"\0asm" as &'static [u8]),
];

#[tokio::test]
async fn typed_client_exercises_polling_lifecycle_over_tcp() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("TCP listener binds");
    let address = listener.local_addr().expect("listener address is readable");
    let server = tokio::spawn(serve(listener, router(state(&temp).await, FakeAuth)).into_future());
    let client = Client::tcp(&format!("http://{address}/")).expect("valid client endpoint");
    let manifest = manifest();

    assert!(
        client
            .builds(None, None, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    assert!(
        matches!(client.build_logs(999,None,None).await.unwrap_err(),piqueld_client::ClientError::Api{status,..} if status.as_u16()==404)
    );
    assert_create_plan(&client, &manifest).await;
    let created = create_and_inspect(&client, &manifest).await;
    let replaced = replace_and_plan(&client, &created, manifest).await;
    assert_ne!(created.operation_id, replaced.operation_id);
    delete(&client, &replaced).await;

    server.abort();
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

async fn create_and_inspect(client: &Client, manifest: &ApplicationManifest) -> AcceptedOperation {
    let mut request = ApplyApplicationRequest {
        expected_generation: Some(0),
        expected_application_id: None,
        manifest: manifest.clone(),
    };
    let created = client
        .apply_and_deploy(&request)
        .await
        .expect("apply succeeds");
    request.expected_generation = Some(created.generation);
    request.expected_application_id = Some(created.environment_id.clone());
    let replay = client
        .apply_and_deploy(&request)
        .await
        .expect("apply retry succeeds");
    assert_ne!(created.operation_id, replay.operation_id);
    let summaries = client.applications().await.unwrap();
    assert_eq!(summaries.items.len(), 1);
    assert_eq!(
        summaries.items[0].id,
        created.environment_id.parse().unwrap()
    );
    assert_eq!(summaries.items[0].name, "notes");
    let summary_json = serde_json::to_value(&summaries.items[0]).unwrap();
    assert!(summary_json.get("application").is_none());
    let application = client
        .application(&created.environment_id)
        .await
        .expect("full application read succeeds");
    assert_eq!(application.application.spec().services.len(), 1);
    let detail = client
        .environment_detail(&created.environment_id)
        .await
        .expect("detail succeeds");
    assert_eq!(
        detail.application.application.id().as_str(),
        created.environment_id
    );
    assert_eq!(detail.application.application.spec().services.len(), 1);
    assert!(detail.observed.services.is_empty());
    assert_eq!(detail.environment.resolved_generation, None);
    assert!(detail.diagnostics.is_empty());
    assert_eq!(
        client.operation(&created.operation_id).await.unwrap().kind,
        piqueld_core::OperationKind::Refresh
    );
    replay
}

#[tokio::test]
async fn dashboard_fallback_preserves_api_and_asset_route_precedence() {
    let temp = tempfile::tempdir().expect("temporary directory");

    let application = web_router(
        state(&temp).await,
        UiAssets::Embedded(TEST_BUNDLE),
        FakeAuth,
    );
    assert_dashboard_routes(&application).await;
    assert_api_routes(&application).await;
    assert_api_only_and_ui_modes(&temp).await;
}

/// Exercise the shipped bundle: every script is a same-origin file, so the
/// constant CSP needs no inline-script hashes.
#[cfg(feature = "embedded-ui")]
#[tokio::test]
async fn compiled_dashboard_serves_assets_without_inline_scripts() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let assets = UiAssets::resolve();
    let UiAssets::Embedded(bundle) = assets else {
        panic!("embedded-ui must resolve to the compiled bundle");
    };
    let application = web_router(state(&temp).await, assets, FakeAuth);
    let response = application
        .clone()
        .oneshot(request("/dashboard/"))
        .await
        .expect("compiled dashboard request succeeds");
    assert_eq!(response.status(), axum::http::StatusCode::OK);
    let csp = response.headers()["content-security-policy"]
        .to_str()
        .expect("CSP is text")
        .to_owned();
    let html = response_text(response).await.1;
    assert!(
        csp.contains("script-src 'self' 'wasm-unsafe-eval';"),
        "{csp}"
    );
    assert!(!html.contains("{{"), "every shell placeholder is filled");

    let scripts = html.split("<script").skip(1).collect::<Vec<_>>();
    assert!(!scripts.is_empty(), "the shell must load the dashboard");
    for script in scripts {
        let (attributes, rest) = script.split_once('>').expect("script opening tag");
        assert!(
            attributes
                .split_whitespace()
                .any(|attr| attr.starts_with("src=")),
            "inline script in the shell: {script}"
        );
        assert!(
            rest.starts_with("</script>"),
            "script with a body: {script}"
        );
    }

    for extension in [".js", ".wasm", ".css"] {
        assert!(
            bundle
                .iter()
                .any(|(name, _)| name.ends_with(extension) && html.contains(name)),
            "the shell must reference a bundled {extension} asset"
        );
    }
    for (name, expected) in bundle {
        let response = application
            .clone()
            .oneshot(request(&format!("/dashboard/{name}")))
            .await
            .expect("compiled asset request succeeds");
        assert_eq!(response.status(), axum::http::StatusCode::OK, "{name}");
        let body = response
            .into_body()
            .collect()
            .await
            .expect("asset body")
            .to_bytes();
        assert_eq!(body.as_ref(), *expected, "{name}");
    }
}

async fn assert_dashboard_routes(application: &axum::Router) {
    let root = response_text(
        application
            .clone()
            .oneshot(request("/"))
            .await
            .expect("root request succeeds"),
    )
    .await;
    assert_eq!(root.0, axum::http::StatusCode::PERMANENT_REDIRECT);
    assert_eq!(root.2.as_deref(), Some("/dashboard/"));

    let dashboard_root = response_text(
        application
            .clone()
            .oneshot(request("/dashboard"))
            .await
            .expect("dashboard root request succeeds"),
    )
    .await;
    assert_eq!(dashboard_root.0, axum::http::StatusCode::PERMANENT_REDIRECT);
    assert_eq!(dashboard_root.2.as_deref(), Some("/dashboard/"));

    let dashboard = response_text(
        application
            .clone()
            .oneshot(request("/dashboard/"))
            .await
            .expect("dashboard request succeeds"),
    )
    .await;
    assert_eq!(dashboard.0, axum::http::StatusCode::OK);
    assert!(dashboard.1.contains("dashboard-shell"));

    let deep_link = response_text(
        application
            .clone()
            .oneshot(request("/dashboard/applications/notes"))
            .await
            .expect("deep link request succeeds"),
    )
    .await;
    assert_eq!(deep_link.0, axum::http::StatusCode::OK);
    assert!(deep_link.1.contains("dashboard-shell"));

    let asset = response_text(
        application
            .clone()
            .oneshot(request("/dashboard/app.js"))
            .await
            .expect("asset request succeeds"),
    )
    .await;
    assert_eq!(asset.0, axum::http::StatusCode::OK);
    assert!(asset.1.contains("console.log"));

    let hashed = response_text(
        application
            .clone()
            .oneshot(request("/dashboard/piqueld-ui-a1b2c3d4_bg.wasm"))
            .await
            .expect("hashed asset request succeeds"),
    )
    .await;
    assert_eq!(hashed.0, axum::http::StatusCode::OK);
    assert!(hashed.1.starts_with('\u{0}'));

    let missing_asset = response_text(
        application
            .clone()
            .oneshot(request("/dashboard/missing.js"))
            .await
            .expect("missing asset request succeeds"),
    )
    .await;
    assert_eq!(missing_asset.0, axum::http::StatusCode::NOT_FOUND);
    assert!(!missing_asset.1.contains("dashboard-shell"));

    let api_error = response_text(
        application
            .clone()
            .oneshot(request("/api/v1/unknown"))
            .await
            .expect("unknown API request succeeds"),
    )
    .await;
    assert_eq!(api_error.0, axum::http::StatusCode::NOT_FOUND);
    assert!(api_error.1.contains("endpoint_not_found"));
    assert!(!api_error.1.contains("dashboard-shell"));

    let outside = response_text(
        application
            .clone()
            .oneshot(request("/unknown"))
            .await
            .expect("unknown web request succeeds"),
    )
    .await;
    assert_eq!(outside.0, axum::http::StatusCode::NOT_FOUND);
    assert!(!outside.1.contains("dashboard-shell"));
}

async fn assert_api_routes(application: &axum::Router) {
    let health = response_text(
        application
            .clone()
            .oneshot(request("/health"))
            .await
            .expect("health request succeeds"),
    )
    .await;
    assert_eq!(health.0, axum::http::StatusCode::OK);
    assert_eq!(health.1, r#"{"status":"ok"}"#);

    let openapi = response_text(
        application
            .clone()
            .oneshot(request("/api/v1/openapi.json"))
            .await
            .expect("OpenAPI request succeeds"),
    )
    .await;
    assert_eq!(openapi.0, axum::http::StatusCode::OK);
    let openapi: serde_json::Value = serde_json::from_str(&openapi.1).unwrap();
    assert_eq!(openapi["openapi"], "3.0.3");
    assert!(openapi["paths"]["/api/v1/environments/{id}/detail"].is_object());
}

async fn assert_api_only_and_ui_modes(temp: &TempDir) {
    let api_only = api_router(state(temp).await, FakeAuth);
    let unix_root = response_text(
        api_only
            .clone()
            .oneshot(request("/"))
            .await
            .expect("API-only root request succeeds"),
    )
    .await;
    assert_eq!(unix_root.0, axum::http::StatusCode::NOT_FOUND);
    assert!(!unix_root.1.contains("endpoint_not_found"));

    let unix_health = response_text(
        api_only
            .clone()
            .oneshot(request("/health"))
            .await
            .expect("API-only health request succeeds"),
    )
    .await;
    assert_eq!(unix_health.0, axum::http::StatusCode::NOT_FOUND);

    let api_only_error = response_text(
        api_only
            .oneshot(request("/api/v1/unknown"))
            .await
            .expect("API-only unknown request succeeds"),
    )
    .await;
    assert_eq!(api_only_error.0, axum::http::StatusCode::NOT_FOUND);
    assert!(api_only_error.1.contains("endpoint_not_found"));

    let disabled = web_router(state(temp).await, UiAssets::Disabled, FakeAuth);
    let disabled_root = response_text(
        disabled
            .clone()
            .oneshot(request("/"))
            .await
            .expect("disabled UI root request succeeds"),
    )
    .await;
    assert_eq!(disabled_root.0, axum::http::StatusCode::NOT_FOUND);
    let disabled_dashboard = response_text(
        disabled
            .oneshot(request("/dashboard/"))
            .await
            .expect("disabled UI dashboard request succeeds"),
    )
    .await;
    assert_eq!(disabled_dashboard.0, axum::http::StatusCode::NOT_FOUND);
}

fn request(uri: &str) -> Request<Body> {
    Request::builder()
        .header("host", "localhost")
        .uri(uri)
        .body(Body::empty())
        .expect("request is valid")
}

#[tokio::test]
async fn dashboard_responses_carry_security_headers() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let application = web_router(
        state(&temp).await,
        UiAssets::Embedded(TEST_BUNDLE),
        FakeAuth,
    );
    let shell = application
        .clone()
        .oneshot(request("/dashboard/"))
        .await
        .expect("dashboard request succeeds");
    assert_eq!(shell.status(), axum::http::StatusCode::OK);
    let headers = shell.headers().clone();
    let csp = headers
        .get("content-security-policy")
        .and_then(|value| value.to_str().ok())
        .expect("dashboard carries a content security policy");
    assert!(csp.contains("default-src 'self'"), "{csp}");
    assert!(csp.contains("wasm-unsafe-eval"), "{csp}");
    assert!(csp.contains("frame-ancestors 'none'"), "{csp}");
    assert_eq!(headers["x-content-type-options"], "nosniff");
    assert_eq!(headers["referrer-policy"], "no-referrer");

    let head = application
        .clone()
        .oneshot(
            Request::builder()
                .header("host", "localhost")
                .method(Method::HEAD)
                .uri("/dashboard/")
                .body(Body::empty())
                .expect("HEAD request is valid"),
        )
        .await
        .expect("dashboard HEAD succeeds");
    assert_eq!(head.status(), StatusCode::OK);
    assert!(
        head.into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .is_empty()
    );

    let method_not_allowed = application
        .clone()
        .oneshot(
            Request::builder()
                .header("host", "localhost")
                .method(Method::POST)
                .uri("/dashboard/")
                .body(Body::empty())
                .expect("POST request is valid"),
        )
        .await
        .expect("dashboard POST completes");
    assert_eq!(method_not_allowed.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(
        method_not_allowed.headers().get(http::header::ALLOW),
        Some(&HeaderValue::from_static("GET, HEAD"))
    );
    for (method, path, status) in [
        (Method::GET, "/", StatusCode::PERMANENT_REDIRECT),
        (Method::GET, "/dashboard/missing.js", StatusCode::NOT_FOUND),
        (Method::POST, "/dashboard", StatusCode::METHOD_NOT_ALLOWED),
    ] {
        let response = application
            .clone()
            .oneshot(
                Request::builder()
                    .header("host", "localhost")
                    .method(method)
                    .uri(path)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        for name in [
            "content-security-policy",
            "x-content-type-options",
            "referrer-policy",
        ] {
            assert_eq!(
                response.headers().get_all(name).iter().count(),
                1,
                "{path}: {name}"
            );
            assert_eq!(response.headers().get(name), headers.get(name));
        }
    }
    for path in ["/health", "/api/v1/missing"] {
        let response = application.clone().oneshot(request(path)).await.unwrap();
        assert!(
            !response.headers().contains_key("content-security-policy"),
            "{path}"
        );
    }
}

#[tokio::test]
async fn dashboard_cache_policy_tracks_content_hashing_and_shell_fallbacks() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let application = web_router(
        state(&temp).await,
        UiAssets::Embedded(TEST_BUNDLE),
        FakeAuth,
    );

    let hashed = application
        .clone()
        .oneshot(request("/dashboard/piqueld-ui-a1b2c3d4_bg.wasm"))
        .await
        .expect("hashed asset request succeeds");
    assert_eq!(
        header_value(&hashed, "content-type"),
        Some("application/wasm")
    );
    assert_eq!(
        header_value(&hashed, "cache-control"),
        Some("public, max-age=31536000, immutable")
    );

    let unhashed = application
        .clone()
        .oneshot(request("/dashboard/app.js"))
        .await
        .expect("unhashed asset request succeeds");
    assert_eq!(
        header_value(&unhashed, "content-type"),
        Some("text/javascript; charset=utf-8")
    );
    assert_eq!(header_value(&unhashed, "cache-control"), Some("no-cache"));

    // A short all-hex stem is an ordinary name, not a content digest.
    let hex_stemmed = application
        .clone()
        .oneshot(request("/dashboard/added.css"))
        .await
        .expect("hex-stemmed asset request succeeds");
    assert_eq!(
        header_value(&hex_stemmed, "cache-control"),
        Some("no-cache")
    );

    for path in ["/dashboard/", "/dashboard/applications/notes"] {
        let shell = response_text(
            application
                .clone()
                .oneshot(request(path))
                .await
                .unwrap_or_else(|_| panic!("{path} succeeds")),
        )
        .await;
        assert_eq!(shell.0, axum::http::StatusCode::OK, "{path}");
        assert!(shell.1.contains("dashboard-shell"), "{path}");
    }

    let shell_headers = application
        .clone()
        .oneshot(request("/dashboard/"))
        .await
        .expect("shell request succeeds");
    assert_eq!(
        header_value(&shell_headers, "cache-control"),
        Some("no-store")
    );
}

fn header_value<'a>(response: &'a axum::response::Response, name: &str) -> Option<&'a str> {
    response
        .headers()
        .get(name)
        .and_then(|value| value.to_str().ok())
}

async fn response_text(
    response: axum::response::Response,
) -> (axum::http::StatusCode, String, Option<String>) {
    let status = response.status();
    let location = response
        .headers()
        .get(axum::http::header::LOCATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let body = response
        .into_body()
        .collect()
        .await
        .expect("response body is readable")
        .to_bytes();
    (
        status,
        String::from_utf8(body.into_iter().collect()).expect("response is UTF-8"),
        location,
    )
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

async fn delete(client: &Client, created: &AcceptedOperation) {
    let deleted = client
        .delete_application_with_generation(&created.environment_id, Some(created.generation))
        .await
        .expect("delete succeeds");
    assert_eq!(
        client
            .operation(&deleted.operations[0].operation_id)
            .await
            .unwrap()
            .kind,
        piqueld_core::OperationKind::Delete
    );
}

// ---------------------------------------------------------------------------
// Raw-transport helpers and negative-path coverage restored from the former
// in-crate API suite.
// ---------------------------------------------------------------------------

use http::{HeaderMap, HeaderValue, Method, Request as HttpRequest, StatusCode, Uri};
use http_body_util::Full;
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;

struct RawResponse {
    status: StatusCode,
    headers: HeaderMap,
    body: serde_json::Value,
}

impl RawResponse {
    fn assert_error(&self, status: StatusCode, code: &str) {
        assert_eq!(self.status, status);
        assert_eq!(self.code(), code);
    }

    fn code(&self) -> &str {
        self.body
            .get("code")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
    }

    fn request_id(&self) -> Option<&str> {
        self.body
            .get("request_id")
            .and_then(serde_json::Value::as_str)
    }
}

async fn send_raw(
    target: Target<'_>,
    method: Method,
    path: &str,
    headers: &[(&str, &str)],
    body: Vec<u8>,
) -> RawResponse {
    let uri = Uri::builder().path_and_query(path).build().expect("uri");
    let mut builder = HttpRequest::builder()
        .method(method)
        .uri(uri)
        .header(http::header::CONTENT_LENGTH, body.len());
    if !headers
        .iter()
        .any(|(name, _)| name.eq_ignore_ascii_case("host"))
    {
        builder = builder.header("host", "localhost");
    }
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let request = builder
        .body(Full::new(Bytes::from(body)))
        .expect("request builds");
    let response = match target {
        Target::Tcp(address) => {
            let stream = tokio::net::TcpStream::connect(address)
                .await
                .expect("tcp connects");
            let (mut sender, connection) =
                hyper::client::conn::http1::handshake(TokioIo::new(stream))
                    .await
                    .expect("handshake");
            tokio::spawn(async move {
                let _ = connection.with_upgrades().await;
            });
            sender.send_request(request).await.expect("response")
        }
        Target::Unix(path) => {
            let stream = tokio::net::UnixStream::connect(path)
                .await
                .expect("unix connects");
            let (mut sender, connection) =
                hyper::client::conn::http1::handshake(TokioIo::new(stream))
                    .await
                    .expect("handshake");
            tokio::spawn(async move {
                let _ = connection.with_upgrades().await;
            });
            sender.send_request(request).await.expect("response")
        }
    };
    let status = response.status();
    let headers = response.headers().clone();
    let bytes = BodyExt::collect(response.into_body())
        .await
        .expect("body collects")
        .to_bytes();
    let body = if bytes.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null)
    };
    RawResponse {
        status,
        headers,
        body,
    }
}

#[derive(Clone, Copy)]
enum Target<'a> {
    Tcp(std::net::SocketAddr),
    Unix(&'a std::path::Path),
}

#[tokio::test]
async fn transport_failures_are_structured_safe_and_request_ids_pair() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let state = state(&temp).await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let tcp_server = tokio::spawn(serve(listener, router(state.clone(), FakeAuth)).into_future());
    let socket = temp.path().join("api.sock");
    let listener = tokio::net::UnixListener::bind(&socket).unwrap();
    let unix_server = tokio::spawn(serve(listener, api_router(state, FakeAuth)).into_future());

    for target in [Target::Tcp(address), Target::Unix(&socket)] {
        target.assert_failures().await;
    }
    for server in [tcp_server, unix_server] {
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }
}

impl Target<'_> {
    async fn assert_failures(self) {
        let huge = format!(
            "{{\"manifest\": {{\"padding\": \"{}\"}}}}",
            "x".repeat(3 * 1024 * 1024)
        );
        let too_large = send_raw(
            self,
            Method::POST,
            "/api/v1/applications/apply",
            &[("content-type", "application/json")],
            huge.into_bytes(),
        )
        .await;
        assert_eq!(too_large.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(too_large.code(), "request_body_too_large");

        let missing = send_raw(self, Method::GET, "/api/v1/does-not-exist", &[], Vec::new()).await;
        assert_eq!(missing.status, StatusCode::NOT_FOUND);
        assert_eq!(missing.code(), "endpoint_not_found");

        let not_allowed =
            send_raw(self, Method::PUT, "/api/v1/applications", &[], Vec::new()).await;
        assert_eq!(not_allowed.status, StatusCode::METHOD_NOT_ALLOWED);
        assert_eq!(not_allowed.code(), "method_not_allowed");

        let bad_cursor = send_raw(
            self,
            Method::GET,
            "/api/v1/applications?cursor=bogus",
            &[],
            Vec::new(),
        )
        .await;
        assert_eq!(bad_cursor.status, StatusCode::BAD_REQUEST);
        assert_eq!(bad_cursor.code(), "pagination_invalid");

        let bad_limit = send_raw(
            self,
            Method::GET,
            "/api/v1/applications?limit=0",
            &[],
            Vec::new(),
        )
        .await;
        assert_eq!(bad_limit.status, StatusCode::BAD_REQUEST);

        let oversized_page = send_raw(
            self,
            Method::GET,
            "/api/v1/applications?limit=101",
            &[],
            Vec::new(),
        )
        .await;
        assert_eq!(oversized_page.status, StatusCode::BAD_REQUEST);

        let malformed = send_raw(
            self,
            Method::POST,
            "/api/v1/applications/plan",
            &[("content-type", "application/json")],
            b"{\"manifest\": {\"broken\"".to_vec(),
        )
        .await;
        assert_eq!(malformed.status, StatusCode::BAD_REQUEST);
        assert_eq!(malformed.code(), "json_malformed");

        let repeated_branch = send_raw(
            self,
            Method::POST,
            "/api/v1/environments/doesnotexist1/deploy?branch=a&branch=b",
            &[],
            Vec::new(),
        )
        .await;
        assert_eq!(repeated_branch.status, StatusCode::BAD_REQUEST);
        assert_eq!(repeated_branch.code(), "query_invalid");

        let paired = send_raw(
            self,
            Method::GET,
            "/api/v1/applications/doesnotexist1",
            &[],
            Vec::new(),
        )
        .await;
        assert_eq!(paired.status, StatusCode::NOT_FOUND);
        let header_id = paired
            .headers
            .get("x-request-id")
            .and_then(|value| value.to_str().ok())
            .expect("request id header");
        assert_eq!(paired.request_id(), Some(header_id));
    }
}

#[tokio::test]
async fn method_not_allowed_advertises_only_the_matched_route_methods() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("binds");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(serve(listener, router(state(&temp).await, FakeAuth)).into_future());

    // The Allow header advertises only the methods registered for the matched
    // route; Axum serves HEAD automatically for every GET route.
    let collection = send_raw(
        Target::Tcp(address),
        Method::PUT,
        "/api/v1/applications",
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(
        collection.headers.get(http::header::ALLOW),
        Some(&HeaderValue::from_static("GET, HEAD, POST"))
    );

    let by_id = send_raw(
        Target::Tcp(address),
        Method::POST,
        "/api/v1/applications/app-notes-01",
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(by_id.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(
        by_id.headers.get(http::header::ALLOW),
        Some(&HeaderValue::from_static("GET, HEAD, DELETE"))
    );

    let health = send_raw(
        Target::Tcp(address),
        Method::POST,
        "/health",
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(health.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(
        health.headers.get(http::header::ALLOW),
        Some(&HeaderValue::from_static("GET, HEAD"))
    );

    server.abort();
}

#[tokio::test]
async fn manifest_validation_media_types_and_unknown_fields_are_rejected_safely() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("binds");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(serve(listener, router(state(&temp).await, FakeAuth)).into_future());

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
    let validation = send_raw(
        Target::Tcp(address),
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
    let malformed_toml = send_raw(
        Target::Tcp(address),
        Method::POST,
        "/api/v1/applications/plan",
        &[("content-type", "text/toml")],
        b"api_version = ".to_vec(),
    )
    .await;
    malformed_toml.assert_error(StatusCode::BAD_REQUEST, "toml_malformed");
    let message = malformed_toml.body["details"]["errors"][0]["message"]
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
    let unknown = send_raw(
        Target::Tcp(address),
        Method::POST,
        "/api/v1/applications/apply",
        &[("content-type", "application/json")],
        serde_json::to_vec(&unknown_field).expect("serializes"),
    )
    .await;
    unknown.assert_error(StatusCode::BAD_REQUEST, "json_malformed");

    server.abort();
}

#[tokio::test]
async fn toml_save_exposes_summary_and_full_configuration() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("binds");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(serve(listener, router(state(&temp).await, FakeAuth)).into_future());

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
    let created = send_raw(
        Target::Tcp(address),
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
    assert!(created.body["data"]["operation_id"].is_null());
    let application_id = created.body["data"]["application_id"]
        .as_str()
        .expect("accepted application ID");

    let listed = send_raw(
        Target::Tcp(address),
        Method::GET,
        "/api/v1/applications",
        &[],
        Vec::new(),
    )
    .await;
    let summary = &listed.body["data"]["items"][0];
    assert_eq!(summary["id"], application_id);
    assert_eq!(summary["name"], "tomlnotes");
    assert!(summary.get("application").is_none());

    let full = send_raw(
        Target::Tcp(address),
        Method::GET,
        &format!("/api/v1/applications/{application_id}"),
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(full.status, StatusCode::OK);
    assert_eq!(full.body["data"]["application"]["id"], application_id);
    assert_eq!(
        full.body["data"]["application"]["spec"]["services"][0]["name"],
        "web"
    );

    server.abort();
}

#[tokio::test]
async fn tcp_rejects_untrusted_authorities() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("binds");
    let address = listener.local_addr().expect("address");
    let application = piqueld::api::http::web_router_with_hosts(
        state(&temp).await,
        UiAssets::Disabled,
        FakeAuth,
        vec!["daemon.example.ts.net".into()],
    );
    let server = tokio::spawn(serve(listener, application).into_future());
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
        let response = send_raw(
            Target::Tcp(address),
            Method::GET,
            "/api/v1/system/status",
            &[("host", host)],
            Vec::new(),
        )
        .await;
        assert_eq!(response.status, expected, "host {host}");
        assert!(response.headers.contains_key("x-request-id"));
    }
    server.abort();
}

#[tokio::test]
async fn tcp_login_uses_canonical_origin_behind_https_proxy() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp).await;
    let store = Store::open(temp.path().join("state.db")).await.unwrap();
    let origin = "https://daemon.example.ts.net:8443";
    let auth = piqueld::auth::Auth::new(&store, origin).unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let application = piqueld::api::http::web_router_with_hosts(
        state,
        UiAssets::Disabled,
        auth,
        vec!["daemon.example.ts.net".into()],
    );
    let server = tokio::spawn(serve(listener, application).into_future());
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
        let response = send_raw(
            Target::Tcp(address),
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
            assert!(
                response.headers["set-cookie"]
                    .to_str()
                    .unwrap()
                    .contains("Secure")
            );
            serde_json::from_value::<piqueld_core::auth::Ceremony>(response.body).unwrap();
        }
    }
    server.abort();
}

#[tokio::test]
async fn tcp_rejects_cross_site_mutations_without_creating_deployments() {
    let temp = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(serve(listener, router(state(&temp).await, FakeAuth)).into_future());
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
        let response = send_raw(
            Target::Tcp(address),
            Method::POST,
            "/api/v1/applications/apply",
            &headers,
            Vec::new(),
        )
        .await;
        assert_eq!(response.status, expected, "headers {headers:?}");
    }
    let client = Client::tcp(&format!("http://{address}")).unwrap();
    let saved = client
        .apply_application(&ApplyApplicationRequest {
            manifest: manifest(),
            expected_generation: Some(0),
            expected_application_id: None,
        })
        .await
        .unwrap();
    for action in ["deploy", "reconcile"] {
        let response = send_raw(
            Target::Tcp(address),
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
        assert_eq!(response.status, StatusCode::FORBIDDEN);
        assert_eq!(response.body["code"], "browser_access_denied");
    }
    assert!(
        client
            .deployments(&saved.application_id, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    server.abort();
}

#[tokio::test]
async fn typed_edits_enforce_the_tcp_browser_policy() {
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let address = api.address;
    let client = &api.client;
    let saved = client
        .apply_application(&AcceptanceApi::request())
        .await
        .unwrap();
    for (method, field, body) in [
        (
            Method::PUT,
            "services/web/replicas",
            br#"{"value":3}"#.to_vec(),
        ),
        (Method::DELETE, "services/web", Vec::new()),
    ] {
        let response = send_raw(
            Target::Tcp(address),
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
    let edit = send_raw(
        Target::Tcp(address),
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
async fn served_openapi_document_matches_the_generated_snapshot_and_resolves_refs() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("binds");
    let address = listener.local_addr().expect("address");
    let server = tokio::spawn(serve(listener, router(state(&temp).await, FakeAuth)).into_future());

    let document_response = send_raw(
        Target::Tcp(address),
        Method::GET,
        "/api/v1/openapi.json",
        &[],
        Vec::new(),
    )
    .await;
    assert_eq!(document_response.status, StatusCode::OK);
    let generated =
        serde_json::to_value(piqueld::api::http::openapi_document()).expect("document serializes");
    assert_eq!(document_response.body, generated);
    for schema in ["ImageReference", "RepositoryDigest", "ImmutableImage"] {
        assert!(
            generated
                .pointer(&format!("/components/schemas/{schema}/pattern"))
                .and_then(serde_json::Value::as_str)
                .is_some_and(|pattern| !pattern.is_empty()),
            "{schema} must expose its validation pattern"
        );
    }

    let text = serde_json::to_string(&generated).expect("document stringifies");
    assert!(
        !text.contains("\"propertyNames\""),
        "OpenAPI 3.0 does not support propertyNames"
    );
    for (pointer, reference) in [
        (
            "/components/schemas/EnvironmentDetailView/properties/latest_operation",
            "#/components/schemas/Operation",
        ),
        (
            "/components/schemas/ApplicationSpec/properties/manifest",
            "#/components/schemas/RepositoryManifest",
        ),
        (
            "/components/schemas/DesiredService/properties/healthcheck",
            "#/components/schemas/ValidatedHealthCheck",
        ),
    ] {
        assert_nullable_reference(
            generated.pointer(pointer).expect("documented schema"),
            reference,
        );
    }
    let mut unresolved = Vec::new();
    collect_unresolved_refs(&generated, &text, &mut unresolved);
    assert!(unresolved.is_empty(), "unresolved refs: {unresolved:?}");

    server.abort();
}

fn assert_nullable_reference(schema: &serde_json::Value, reference: &str) {
    let variants = schema["oneOf"].as_array().expect("nullable oneOf variants");
    assert!(variants.iter().any(|variant| {
        variant
            .get("$ref")
            .or_else(|| variant.pointer("/allOf/0/$ref"))
            == Some(&serde_json::Value::String(reference.to_owned()))
    }));
    assert!(variants.iter().any(|variant| {
        variant["nullable"] == true
            && variant["enum"] == serde_json::json!([null])
            && variant["type"] == "string"
    }));
}

fn collect_unresolved_refs(value: &serde_json::Value, document: &str, out: &mut Vec<String>) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(reference) = map.get("$ref").and_then(serde_json::Value::as_str) {
                let pointer = reference.strip_prefix("#").unwrap_or(reference);
                let resolved = document
                    .parse::<serde_json::Value>()
                    .is_ok_and(|doc| doc.pointer(pointer).is_some());
                if !resolved {
                    out.push(reference.to_owned());
                }
            }
            for child in map.values() {
                collect_unresolved_refs(child, document, out);
            }
        }
        serde_json::Value::Array(items) => {
            for child in items {
                collect_unresolved_refs(child, document, out);
            }
        }
        _ => {}
    }
}

#[tokio::test]
async fn typed_client_exercises_the_lifecycle_over_a_unix_socket() {
    let temp = tempfile::tempdir_in(".").expect("temporary directory");
    let data_dir = temp.path().join("state");
    std::fs::create_dir(&data_dir).expect("data dir exists");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o700))
            .expect("data dir is private");
    }
    // The fixture is owned by this process; Nix's synthetic root is not.
    let cwd = std::env::current_dir().expect("working directory");
    piqueld::prepare_data_dir(data_dir.strip_prefix(&cwd).expect("fixture is below cwd"))
        .await
        .expect("data dir prepares");
    // Keep the socket path short enough for Unix even in deeply nested worktrees.
    let socket_dir = tempfile::tempdir().expect("socket directory");
    let socket_path = socket_dir.path().join("contract.sock");
    let listener = tokio::net::UnixListener::bind(&socket_path).expect("unix binds");
    let server = tokio::spawn(serve(listener, router(state(&temp).await, FakeAuth)).into_future());
    let client = Client::unix(&socket_path);

    let status = client.system_status().await.expect("status over unix");
    assert_eq!(status.api_version, "v1");

    assert_create_plan(&client, &manifest()).await;
    let created = create_and_inspect(&client, &manifest()).await;
    let replaced = replace_and_plan(&client, &created, manifest()).await;
    assert_ne!(created.operation_id, replaced.operation_id);
    delete(&client, &replaced).await;

    server.abort();
}

#[tokio::test]
async fn generations_deploy_reconcile_and_event_pagination_share_the_http_contract() {
    let temp = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(serve(listener, router(state(&temp).await, FakeAuth)).into_future());
    let client = Client::tcp(&format!("http://{address}")).unwrap();
    let mut request = ApplyApplicationRequest {
        manifest: manifest(),
        expected_generation: Some(0),
        expected_application_id: None,
    };
    let first = client.apply_and_deploy(&request).await.unwrap();
    assert_eq!(first.generation, 1);
    let stale = client.apply_and_deploy(&request).await.unwrap_err();
    assert!(
        matches!(stale,piqueld_client::ClientError::Api {status,..} if status==axum::http::StatusCode::CONFLICT)
    );
    request.expected_generation = Some(1);
    request.expected_application_id = Some(first.environment_id.clone());
    request.manifest.spec.services[0].replicas = 2.into();
    let changed = client.apply_and_deploy(&request).await.unwrap();
    assert_eq!(changed.generation, 2);
    // A one-time manifest revision needs a repository-backed manifest.
    let revision = piqueld_client::ManifestRevision::Branch("feature".into());
    let unbacked = client
        .deploy_environment(&first.environment_id, 2, Some(&revision))
        .await
        .unwrap_err();
    assert!(
        matches!(unbacked, piqueld_client::ClientError::Api { error, .. } if error.details.to_string().contains("manifest_repository_required"))
    );
    let refreshed = client
        .deploy_environment(&first.environment_id, 2, None)
        .await
        .unwrap();
    assert_eq!(refreshed.generation, 2);
    assert_ne!(refreshed.operation_id, changed.operation_id);
    let reconciled = client
        .reconcile_environment(&first.environment_id, Some(2))
        .await
        .unwrap();
    assert_eq!(reconciled.operation_id, refreshed.operation_id);
    let first_page = client
        .events(None, Some(&first.environment_id), None, 2)
        .await
        .unwrap();
    assert_eq!(first_page.items.len(), 2);
    let second_page = client
        .events(
            None,
            Some(&first.environment_id),
            first_page.next_cursor.as_deref(),
            100,
        )
        .await
        .unwrap();
    assert!(!second_page.items.is_empty());
    assert!(
        second_page
            .items
            .iter()
            .all(|event| event.id > first_page.items.last().unwrap().id)
    );
    let deletion = client
        .delete_application_with_generation(&first.environment_id, Some(2))
        .await
        .unwrap();
    assert_eq!(deletion.generation, 3);
    assert!(
        client
            .deploy_environment(&first.environment_id, 3, None)
            .await
            .is_err()
    );
    let repeated = client
        .delete_application_with_generation(&first.environment_id, Some(deletion.generation))
        .await
        .unwrap();
    assert_eq!(
        repeated.operations[0].operation_id,
        deletion.operations[0].operation_id
    );
    task.abort();
}

struct AcceptanceApi {
    client: Client,
    address: std::net::SocketAddr,
    runtime: Arc<FakeRuntime>,
    store: Arc<Store>,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl AcceptanceApi {
    async fn start(temp: &TempDir) -> Self {
        let store = Arc::new(Store::open(temp.path().join("state.db")).await.unwrap());
        seed_account(
            &temp.path().join("state.db"),
            "contract-admin",
            &[("admin", None)],
        )
        .await;
        let instance = InstanceId::parse(store.instance_id()).unwrap();
        let runtime = Arc::new(FakeRuntime {
            cleanup_gate: None,
            instance,
            unavailable: std::sync::atomic::AtomicBool::new(false),
        });
        let state = ApiState::new(Arc::clone(&store), runtime.clone());
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let client = Client::tcp(&format!("http://{address}")).unwrap();
        let task = tokio::spawn(serve(listener, router(state, FakeAuth)).into_future());
        Self {
            client,
            address,
            runtime,
            store,
            task,
        }
    }

    fn request() -> ApplyApplicationRequest {
        ApplyApplicationRequest {
            manifest: manifest(),
            expected_generation: Some(0),
            expected_application_id: None,
        }
    }
}

impl Drop for AcceptanceApi {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test]
async fn acceptance_receipts_survive_restart_and_supersession() {
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let request = AcceptanceApi::request();
    let keyed = api.client.clone().with_request_id("apply-command-1");
    let accepted = keyed.apply_and_deploy(&request).await.unwrap();
    let mut replacement = request.clone();
    replacement.expected_generation = Some(1);
    replacement.expected_application_id = Some(accepted.environment_id.clone());
    replacement.manifest.spec.services[0].replicas = 2.into();
    api.client.apply_and_deploy(&replacement).await.unwrap();
    drop(api);
    let api = AcceptanceApi::start(&temp).await;
    let keyed = api.client.clone().with_request_id("apply-command-1");
    let replayed = keyed.apply_and_deploy(&request).await.unwrap();
    assert_eq!(replayed.operation_id, accepted.operation_id);
    assert_eq!(replayed.generation, 1);
    assert_eq!(
        api.client
            .application(&accepted.environment_id)
            .await
            .unwrap()
            .generation,
        2
    );
    assert_eq!(
        api.client
            .operation(&accepted.operation_id)
            .await
            .unwrap()
            .state,
        piqueld_core::OperationState::Superseded
    );
    let error = keyed.apply_and_deploy(&replacement).await.unwrap_err();
    assert!(
        matches!(error,piqueld_client::ClientError::Api {error,..} if error.code=="request_id_conflict")
    );
    // Two concurrent requests still produce exactly one durable acceptance.
    let mut fresh = request.clone();
    fresh.manifest.metadata.name = "another".into();
    let concurrent = api.client.clone().with_request_id("concurrent-command");
    let (a, b) = tokio::join!(
        concurrent.apply_and_deploy(&fresh),
        concurrent.apply_and_deploy(&fresh)
    );
    assert_eq!(a.unwrap().operation_id, b.unwrap().operation_id);
}

#[tokio::test]
async fn rename_is_conditioned_idle_only_and_replayable_without_deployment() {
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let accepted = api
        .client
        .apply_and_deploy(&AcceptanceApi::request())
        .await
        .unwrap();
    let request = piqueld_client::RenameApplicationRequest {
        name: "renamed".into(),
        expected_generation: Some(1),
    };
    let keyed = api.client.clone().with_request_id("rename-command");
    let error = keyed
        .rename_application(&accepted.environment_id, &request)
        .await
        .unwrap_err();
    assert!(
        matches!(error,piqueld_client::ClientError::Api {error,..} if error.code=="application_busy")
    );
    api.finish_operation(
        &accepted.operation_id,
        Some(("image_resolution_rejected", "image unavailable")),
    )
    .await;
    let renamed = keyed
        .rename_application(&accepted.environment_id, &request)
        .await
        .unwrap();
    assert_eq!(renamed.application_id, accepted.environment_id);
    assert_eq!(renamed.generation, 2);
    let replay = keyed
        .rename_application(&accepted.environment_id, &request)
        .await
        .unwrap();
    assert_eq!(replay.generation, 2);
    let app = api
        .client
        .application(&accepted.environment_id)
        .await
        .unwrap();
    assert_eq!(app.application.metadata().name.as_str(), "renamed");
    assert_eq!(
        api.client
            .operation(&accepted.operation_id)
            .await
            .unwrap()
            .state,
        piqueld_core::OperationState::Failed
    );
    let mut identical = AcceptanceApi::request();
    identical.manifest.metadata.name = "renamed".into();
    identical.expected_generation = Some(2);
    identical.expected_application_id = Some(accepted.environment_id.clone());
    let no_op = api.client.apply_and_deploy(&identical).await.unwrap();
    assert_ne!(no_op.operation_id, accepted.operation_id);
    assert_eq!(no_op.generation, 3);
    assert_eq!(
        api.client
            .operation(&accepted.operation_id)
            .await
            .unwrap()
            .state,
        piqueld_core::OperationState::Failed
    );
    // The old name now identifies a different app. A captured ID must prevent overwriting it.
    let old_name = api
        .client
        .apply_and_deploy(&AcceptanceApi::request())
        .await
        .unwrap();
    let mut stale = AcceptanceApi::request();
    stale.expected_generation = Some(1);
    stale.expected_application_id = Some(accepted.environment_id);
    stale.manifest.spec.services[0].replicas = 3.into();
    let error = api.client.apply_and_deploy(&stale).await.unwrap_err();
    assert!(
        matches!(error,piqueld_client::ClientError::Api {error,..} if error.code=="identity_conflict")
    );
    assert_eq!(
        api.client
            .application(&old_name.environment_id)
            .await
            .unwrap()
            .application
            .spec()
            .services[0]
            .replicas,
        1.into()
    );
}

#[tokio::test]
async fn receipt_failure_rolls_back_acceptance_and_expired_keys_are_reusable() {
    use sqlx::Connection;
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let mut connection = sqlx::SqliteConnection::connect(&format!(
        "sqlite://{}",
        temp.path().join("state.db").display()
    ))
    .await
    .unwrap();
    sqlx::query("CREATE TRIGGER reject_receipt BEFORE INSERT ON request_receipts BEGIN SELECT RAISE(ABORT,'injected receipt failure'); END").execute(&mut connection).await.unwrap();
    let keyed = api.client.clone().with_request_id("atomic-command");
    let request = AcceptanceApi::request();
    assert!(keyed.apply_and_deploy(&request).await.is_err());
    assert_eq!(
        api.store.list(None, 50).await.unwrap().items,
        [] as [piqueld::store::StoredEnvironment; 0]
    );
    sqlx::query("DROP TRIGGER reject_receipt")
        .execute(&mut connection)
        .await
        .unwrap();
    let accepted = keyed.apply_and_deploy(&request).await.unwrap();
    sqlx::query("UPDATE request_receipts SET expires_at_ms=0")
        .execute(&mut connection)
        .await
        .unwrap();
    let refreshed = keyed
        .deploy_environment(&accepted.environment_id, 1, None)
        .await
        .unwrap();
    assert_ne!(refreshed.operation_id, accepted.operation_id);
}

#[tokio::test]
async fn preview_resolves_images_again_and_redacts_manifest_and_runtime_configuration() {
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let mut request = AcceptanceApi::request();
    request.manifest.spec.services[0]
        .environment
        .insert("TOKEN".into(), "old-private-value".into());
    let normalized = request
        .manifest
        .clone()
        .validate()
        .unwrap()
        .normalize(piqueld_core::ApplicationId::parse("app-preview-01").unwrap());
    let runtime = FakeRuntime {
        cleanup_gate: None,
        instance: InstanceId::parse(api.store.instance_id()).unwrap(),
        unavailable: std::sync::atomic::AtomicBool::new(false),
    };
    let target = runtime
        .prepare(
            &piqueld_core::EnvironmentId::default_for(normalized.id()),
            &normalized,
            &ResolutionSet::default(),
        )
        .await
        .unwrap();
    api.store
        .save_application(
            &ApplicationTemplate::from(&normalized),
            Some(&target),
            Some(0),
        )
        .await
        .unwrap();
    request.expected_generation = Some(1);
    request.expected_application_id = Some(normalized.id().to_string());
    request.manifest.spec.services[0].replicas = 3.into();
    request.manifest.spec.services[0]
        .environment
        .insert("TOKEN".into(), "new-private-value".into());
    request.manifest.spec.services[0].command = vec!["command-private-value".into()];
    let preview = api.client.plan_application(&request, None).await.unwrap();
    assert_eq!(preview.generation, 1);
    assert!(!preview.identical);
    assert!(
        preview
            .changes
            .iter()
            .any(|change| change.field == "services.web.replicas"
                && change.after.as_deref() == Some("3"))
    );
    assert!(
        preview
            .plan
            .actions
            .iter()
            .any(|action| matches!(action.kind, piqueld_core::ActionKind::ResolveImage { .. }))
    );
    let json = serde_json::to_string(&preview).unwrap();
    for secret in [
        "old-private-value",
        "new-private-value",
        "command-private-value",
    ] {
        assert!(!json.contains(secret));
    }
    assert!(json.contains("redacted"));
    assert_eq!(
        api.store
            .application(normalized.id())
            .await
            .unwrap()
            .generation,
        1,
        "preview is read-only"
    );
}

impl AcceptanceApi {
    fn assert_error(error: piqueld_client::ClientError, code: &str) {
        assert!(
            matches!(error, piqueld_client::ClientError::Api { error, .. } if error.code == code)
        );
    }

    async fn finish_operation(&self, id: &str, error: Option<(&str, &str)>) {
        use piqueld_core::OperationState;
        self.store
            .transition_operation(id, OperationState::Requested, OperationState::Running, None)
            .await
            .unwrap();
        let state = if error.is_some() {
            OperationState::Failed
        } else {
            OperationState::Succeeded
        };
        self.store
            .transition_operation(id, OperationState::Running, state, error)
            .await
            .unwrap();
    }

    /// The rendered configuration deployment `operation` of `environment` captured.
    async fn deployed(&self, environment: &str, operation: &str) -> NormalizedApplication {
        self.client
            .deployments(environment, None)
            .await
            .unwrap()
            .items
            .into_iter()
            .find(|deployment| deployment.operation.id == operation)
            .and_then(|deployment| deployment.application)
            .expect("deployment captured a rendered manifest")
    }

    async fn finish_deletion(&self, deletion: &piqueld_core::api::DeletedApplication) {
        for accepted in &deletion.operations {
            self.store
                .transition_operation(
                    &accepted.operation_id,
                    piqueld_core::OperationState::Requested,
                    piqueld_core::OperationState::Running,
                    None,
                )
                .await
                .unwrap();
            let operation = self.store.operation(&accepted.operation_id).await.unwrap();
            self.store
                .finish_delete_operation(&operation)
                .await
                .unwrap();
        }
    }
}

#[tokio::test]
async fn mutations_require_preconditions_but_reconcile_uses_current_intent() {
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let mut request = AcceptanceApi::request();
    request.expected_generation = None;
    AcceptanceApi::assert_error(
        api.client.apply_and_deploy(&request).await.unwrap_err(),
        "precondition_required",
    );
    request.expected_generation = Some(0);
    let accepted = api.client.apply_and_deploy(&request).await.unwrap();
    request.expected_generation = Some(1);
    AcceptanceApi::assert_error(
        api.client.apply_and_deploy(&request).await.unwrap_err(),
        "precondition_required",
    );
    request.expected_application_id = Some(accepted.environment_id.clone());
    request.manifest.spec.services[0].replicas = 2.into();
    let mut competing = request.clone();
    competing.manifest.spec.services[0].replicas = 3.into();
    let (first, second) = tokio::join!(
        api.client.apply_and_deploy(&request),
        api.client.apply_and_deploy(&competing)
    );
    assert_ne!(first.is_ok(), second.is_ok());
    AcceptanceApi::assert_error(first.err().or(second.err()).unwrap(), "generation_conflict");
    AcceptanceApi::assert_error(
        api.client
            .delete_application_with_generation(&accepted.environment_id, None)
            .await
            .unwrap_err(),
        "precondition_required",
    );
    AcceptanceApi::assert_error(
        api.client
            .delete_application_with_generation(&accepted.environment_id, Some(1))
            .await
            .unwrap_err(),
        "generation_conflict",
    );
    let refreshed = api
        .client
        .deploy_environment(&accepted.environment_id, 2, None)
        .await
        .unwrap();
    assert_eq!(refreshed.generation, 2);
    let deletion = api
        .client
        .delete_application_with_preconditions(&accepted.environment_id, Some(1), true, &[])
        .await
        .unwrap();
    assert_eq!(deletion.generation, 3);
    let reconciled = api
        .client
        .reconcile_environment(&accepted.environment_id, None)
        .await
        .unwrap();
    assert_eq!(reconciled.operation_id, deletion.operations[0].operation_id);
    assert!(
        api.client
            .deploy_environment(&accepted.environment_id, 2, None)
            .await
            .is_err()
    );
}

#[tokio::test]
async fn forced_apply_retargets_reused_names_and_creates_absent_names() {
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let mut request = AcceptanceApi::request();
    request.expected_generation = None;
    let original = api
        .client
        .apply_and_deploy_with_force(&request, true)
        .await
        .unwrap();
    let deletion = api
        .client
        .delete_application_with_generation(&original.environment_id, Some(1))
        .await
        .unwrap();
    api.finish_deletion(&deletion).await;
    let replacement = api
        .client
        .apply_and_deploy(&AcceptanceApi::request())
        .await
        .unwrap();
    assert_ne!(replacement.environment_id, original.environment_id);
    request.expected_generation = Some(1);
    request.expected_application_id = Some(original.environment_id);
    request.manifest.spec.services[0].replicas = 4.into();
    AcceptanceApi::assert_error(
        api.client.apply_and_deploy(&request).await.unwrap_err(),
        "identity_conflict",
    );
    let forced = api
        .client
        .apply_and_deploy_with_force(&request, true)
        .await
        .unwrap();
    assert_eq!(forced.environment_id, replacement.environment_id);
    assert_eq!(forced.generation, 2);
    assert_eq!(
        api.client
            .application(&forced.environment_id)
            .await
            .unwrap()
            .application
            .spec()
            .services[0]
            .replicas,
        4.into()
    );
    request.manifest.spec.services[0].replicas = 0.into();
    assert!(
        api.client
            .apply_and_deploy_with_force(&request, true)
            .await
            .is_err(),
        "force must not bypass manifest validation"
    );
    assert_eq!(
        api.client
            .application(&forced.environment_id)
            .await
            .unwrap()
            .generation,
        2
    );
}

#[tokio::test]
async fn forced_receipts_replay_after_restart_without_overwriting_newer_intent() {
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let request = AcceptanceApi::request();
    let keyed = api.client.clone().with_request_id("forced-command");
    let accepted = keyed
        .apply_and_deploy_with_force(&request, true)
        .await
        .unwrap();
    let mut changed = request.clone();
    changed.manifest.spec.services[0].replicas = 3.into();
    let newer = api
        .client
        .apply_and_deploy_with_force(&changed, true)
        .await
        .unwrap();
    drop(api);
    let api = AcceptanceApi::start(&temp).await;
    let keyed = api.client.clone().with_request_id("forced-command");
    let replay = keyed
        .apply_and_deploy_with_force(&request, true)
        .await
        .unwrap();
    assert_eq!(replay.operation_id, accepted.operation_id);
    assert_eq!(replay.generation, accepted.generation);
    assert_eq!(
        api.client
            .operation(&accepted.operation_id)
            .await
            .unwrap()
            .state,
        piqueld_core::OperationState::Superseded
    );
    let current = api
        .client
        .application(&accepted.environment_id)
        .await
        .unwrap();
    assert_eq!(current.generation, newer.generation);
    assert_eq!(current.application.spec().services[0].replicas, 3.into());
    AcceptanceApi::assert_error(
        keyed.apply_and_deploy(&request).await.unwrap_err(),
        "request_id_conflict",
    );
    let separately_invoked = api.client.clone().with_request_id("new-forced-command");
    let reapplied = separately_invoked
        .apply_and_deploy_with_force(&request, true)
        .await
        .unwrap();
    assert_eq!(reapplied.generation, newer.generation + 1);
}

#[tokio::test]
async fn forced_rename_bypasses_revision_but_preserves_busy_and_name_checks() {
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let accepted = api
        .client
        .apply_and_deploy(&AcceptanceApi::request())
        .await
        .unwrap();
    let mut rename = piqueld_client::RenameApplicationRequest {
        name: "renamed".into(),
        expected_generation: None,
    };
    AcceptanceApi::assert_error(
        api.client
            .rename_application(&accepted.environment_id, &rename)
            .await
            .unwrap_err(),
        "precondition_required",
    );
    AcceptanceApi::assert_error(
        api.client
            .rename_application_with_force(&accepted.environment_id, &rename, true)
            .await
            .unwrap_err(),
        "application_busy",
    );
    api.finish_operation(&accepted.operation_id, None).await;
    rename.expected_generation = Some(99);
    let renamed = api
        .client
        .rename_application_with_force(&accepted.environment_id, &rename, true)
        .await
        .unwrap();
    assert_eq!(renamed.generation, 2);
    assert_eq!(renamed.application_id, accepted.environment_id);
    api.client
        .apply_and_deploy(&AcceptanceApi::request())
        .await
        .unwrap();
    rename.name = "notes".into();
    AcceptanceApi::assert_error(
        api.client
            .rename_application_with_force(&accepted.environment_id, &rename, true)
            .await
            .unwrap_err(),
        "application_name_collision",
    );
}

#[tokio::test]
async fn docker_outage_allows_acceptance_and_preserves_receipt_replay() {
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let request = AcceptanceApi::request();
    let keyed = api.client.clone().with_request_id("outage-replay");
    let accepted = keyed.apply_and_deploy(&request).await.unwrap();
    api.runtime
        .unavailable
        .store(true, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(
        keyed.apply_and_deploy(&request).await.unwrap().operation_id,
        accepted.operation_id
    );
    let mut changed = request.clone();
    changed.expected_generation = Some(accepted.generation);
    changed.expected_application_id = Some(accepted.environment_id.clone());
    changed.manifest.spec.services[0].replicas = 2.into();
    assert!(
        matches!(api.client.plan_application(&changed, None).await.unwrap_err(),piqueld_client::ClientError::Api{status,..} if status==StatusCode::SERVICE_UNAVAILABLE)
    );
    let next = api.client.apply_and_deploy(&changed).await.unwrap();
    assert_ne!(next.operation_id, accepted.operation_id);
    AcceptanceApi::assert_error(
        keyed.apply_and_deploy(&changed).await.unwrap_err(),
        "request_id_conflict",
    );
}

// Existing lifecycle scenarios explicitly request deployment; save-only behavior
// is exercised separately below.
trait DeployFixture {
    async fn apply_and_deploy(
        &self,
        request: &ApplyApplicationRequest,
    ) -> Result<AcceptedOperation, piqueld_client::ClientError>;
    async fn apply_and_deploy_with_force(
        &self,
        request: &ApplyApplicationRequest,
        force: bool,
    ) -> Result<AcceptedOperation, piqueld_client::ClientError>;
}
impl DeployFixture for Client {
    async fn apply_and_deploy(
        &self,
        request: &ApplyApplicationRequest,
    ) -> Result<AcceptedOperation, piqueld_client::ClientError> {
        self.apply_and_deploy_with_force(request, false).await
    }
    async fn apply_and_deploy_with_force(
        &self,
        request: &ApplyApplicationRequest,
        force: bool,
    ) -> Result<AcceptedOperation, piqueld_client::ClientError> {
        let saved = self
            .apply_application_with_options(request, force, true)
            .await?;
        Ok(AcceptedOperation {
            environment_id: saved.application_id,
            generation: saved.generation,
            operation_id: saved.operation_id.expect("deployment requested"),
        })
    }
}

#[tokio::test]
async fn saved_configuration_preview_and_deployment_are_separate_even_offline() {
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    api.runtime
        .unavailable
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let mut request = AcceptanceApi::request();
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
        .deployments(&saved.application_id, None)
        .await
        .unwrap();
    assert_eq!(history.items.len(), 2);
    assert_eq!(
        history.items[1].operation.state,
        piqueld_core::OperationState::Superseded
    );
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
    api.runtime
        .unavailable
        .store(false, std::sync::atomic::Ordering::Relaxed);
    request.expected_generation = Some(saved.generation);
    let preview = api.client.plan_application(&request, None).await.unwrap();
    assert!(!preview.identical);
    assert!(!preview.changes.is_empty());
}

#[tokio::test]
async fn deploy_after_rename_captures_saved_name_and_spec() {
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let accepted = api
        .client
        .apply_and_deploy(&AcceptanceApi::request())
        .await
        .unwrap();
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
    let mut edited = AcceptanceApi::request();
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
    let accepted = api
        .client
        .apply_and_deploy(&AcceptanceApi::request())
        .await
        .unwrap();
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
    api.runtime
        .unavailable
        .store(true, std::sync::atomic::Ordering::Relaxed);
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
    api.runtime
        .unavailable
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let mut request = AcceptanceApi::request();
    request.manifest.spec.services[0]
        .environment
        .insert("MESSAGE".into(), "quotes \" and newline\n".into());
    request.manifest.spec.manifest = Some(piqueld_core::manifest::RepositoryManifest {
        repository: piqueld_core::manifest::GitRepository {
            url: "https://example.com/repo.git".into(),
            branch: "main".into(),
            commit: None,
        },
        path: "infra/app.toml".into(),
    });
    let saved = api.client.apply_application(&request).await.unwrap();
    let response = router(
        ApiState::new(api.store.clone(), api.runtime.clone()),
        FakeAuth,
    )
    .oneshot(
        Request::builder()
            .header("host", "localhost")
            .uri(format!(
                "/api/v1/applications/{}/manifest",
                saved.application_id
            ))
            .body(Body::empty())
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "application/toml");
    assert_eq!(response.headers()["cache-control"], "no-store");
    assert_eq!(
        response.headers()["content-disposition"],
        "attachment; filename=\"notes.toml\""
    );
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let downloaded: ApplicationManifest =
        toml::from_str(std::str::from_utf8(&bytes).unwrap()).unwrap();
    assert_eq!(downloaded, request.manifest);
    assert!(
        api.client
            .deployments(&saved.application_id, None)
            .await
            .unwrap()
            .items
            .is_empty()
    );
}

#[tokio::test]
async fn routed_statuses_and_media_types_are_documented_in_openapi() {
    let temp = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(serve(listener, router(state(&temp).await, FakeAuth)).into_future());
    let document = serde_json::to_value(piqueld::api::http::openapi_document()).unwrap();
    let cases = [
        (Method::GET, "/system/status", 200),
        (Method::GET, "/system/configuration", 503),
        (Method::GET, "/applications", 200),
        (Method::GET, "/events", 200),
        (Method::GET, "/applications/{id}", 404),
        (Method::GET, "/environments/{id}/detail", 404),
        (Method::GET, "/environments/{id}/status", 404),
        (Method::GET, "/environments/{id}/deployments", 404),
        (Method::GET, "/operations/{id}", 404),
        (Method::POST, "/applications/apply", 400),
        (Method::POST, "/applications/plan", 400),
        (Method::GET, "/environments/{id}/exec", 426),
    ];
    for (method, suffix, status) in cases {
        let path = format!("/api/v1{suffix}");
        let response = send_raw(
            Target::Tcp(address),
            method.clone(),
            &path.replace("{id}", "app-missing"),
            &[("content-type", "application/json")],
            if method == Method::POST {
                b"{}".to_vec()
            } else {
                Vec::new()
            },
        )
        .await;
        assert_eq!(response.status.as_u16(), status, "{method} {path}");
        response.assert_documented(&document, &method, &path);
    }
    server.abort();
}

impl RawResponse {
    fn assert_documented(&self, document: &serde_json::Value, method: &Method, path: &str) {
        let status = self.status.as_u16().to_string();
        let operation = &document["paths"][path][method.as_str().to_lowercase()];
        let response = &operation["responses"][&status];
        assert!(
            !response.is_null(),
            "undocumented {method} {path} -> {status}"
        );
        let media_type = self.headers[http::header::CONTENT_TYPE]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap();
        assert!(
            !response["content"][media_type].is_null(),
            "undocumented response media type for {method} {path}: {media_type}"
        );
        if self.status.is_client_error() || self.status.is_server_error() {
            let error: piqueld_core::api::ErrorBody =
                serde_json::from_value(self.body.clone()).unwrap();
            assert_ne!(error.code, "");
            assert_ne!(error.message, "");
            let header_id = self
                .headers
                .get("x-request-id")
                .and_then(|value| value.to_str().ok())
                .expect("request ID header");
            assert_ne!(header_id, "");
            assert_eq!(error.request_id, header_id);
        }
    }
}

#[tokio::test]
async fn application_log_snapshot_validates_bounds_and_preserves_task_identity() {
    let temp = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(serve(listener, router(state(&temp).await, FakeAuth)).into_future());
    let client = Client::tcp(&format!("http://{address}/")).unwrap();
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
        assert!(
            matches!(error, piqueld_client::ClientError::Api { status, .. } if status.as_u16() == 400)
        );
    }
    server.abort();
}

/// Exec rechecks the caller's grants when the command starts, so access lost
/// after the connection was authorized cannot start one. History names who
/// started a command.
#[tokio::test]
async fn exec_rechecks_grants_when_the_command_starts() {
    use piqueld::store::{Caller, StoreError};
    use piqueld_core::access::Denied;
    let temp = tempfile::tempdir().unwrap();
    let state = state(&temp).await;
    let mutation =
        piqueld::api::Mutation::save(manifest().validate_template().unwrap(), None, false);
    let Ok(piqueld::api::MutationResponse::Saved(saved)) =
        state.accept(Daemon, mutation, Some(0), false, None).await
    else {
        panic!("save response");
    };
    let database = temp.path().join("state.db");
    let grant = [("apps:exec", Some(saved.application_id.as_str()))];
    seed_account(&database, "operator", &grant).await;
    let environment = piqueld_core::EnvironmentId::parse(saved.application_id.as_str()).unwrap();
    let request = piqueld_core::exec::ExecRequest {
        service: "web".parse().unwrap(),
        command: piqueld_core::exec::ExecCommand::parse(vec!["true".into()]).unwrap(),
        stdin: false,
        tty: None,
    };
    let caller = piqueld::api::Actor::Account(Caller {
        credential_id: "operator",
        user_id: "operator",
    });
    assert!(
        state
            .exec(caller, &environment, &request, "operator")
            .await
            .is_ok()
    );
    let filter = piqueld_core::observability::EventFilter {
        kind: Some("command_started".into()),
        ..Default::default()
    };
    let started = state
        .filtered_events(&filter, &piqueld::store::Visibility::ALL, None, 10)
        .await
        .unwrap()
        .items;
    assert_eq!(started[0].actor_user_id.as_deref(), Some("operator"));
    assert_eq!(started[0].actor_credential_id.as_deref(), Some("operator"));
    let mut connection = <sqlx::SqliteConnection as sqlx::Connection>::connect(&format!(
        "sqlite:{}",
        database.display()
    ))
    .await
    .unwrap();
    sqlx::query("DELETE FROM auth_grants WHERE user_id='operator'")
        .execute(&mut connection)
        .await
        .unwrap();
    assert!(matches!(
        state.exec(caller, &environment, &request, "operator").await,
        Err(piqueld::api::ApplicationError::Store(StoreError::Denied(
            Denied::Hidden
        )))
    ));
}

/// A request key replays only for the account that used it, so another caller
/// reusing it cannot learn the outcome, e.g. of an application since renamed.
/// A creator replays its creation even without `apps:write` on the application.
#[tokio::test]
async fn receipts_replay_only_for_their_account() {
    use piqueld::api::{Actor, ApplicationError, Mutation};
    use piqueld::store::{Caller, StoreError};
    let temp = tempfile::tempdir().unwrap();
    let state = state(&temp).await;
    let database = temp.path().join("state.db");
    seed_account(&database, "author", &[("apps:create", None)]).await;
    let grants = [("apps:create", None), ("apps:write", None)];
    seed_account(&database, "other", &grants).await;
    let save = || Mutation::save(manifest().validate_template().unwrap(), None, false);
    let author = Actor::Account(Caller {
        credential_id: "author",
        user_id: "author",
    });
    let other = Actor::Account(Caller {
        credential_id: "other",
        user_id: "other",
    });
    let original = state
        .accept(author, save(), Some(0), false, Some("create-key"))
        .await
        .unwrap();
    let replayed = state
        .accept(author, save(), Some(0), false, Some("create-key"))
        .await
        .unwrap();
    assert_eq!(format!("{replayed:?}"), format!("{original:?}"));
    for caller in [other, Daemon] {
        assert!(matches!(
            state
                .accept(caller, save(), Some(0), false, Some("create-key"))
                .await,
            Err(ApplicationError::Store(StoreError::ReplayConflict))
        ));
    }
}

#[tokio::test]
async fn exec_streams_over_both_transports_and_records_history_without_the_command() {
    use futures_util::{SinkExt, StreamExt};
    use piqueld_client::exec::{ExecCommand, ExecInput, ExecOutput, ExecRequest};
    use tokio_tungstenite::tungstenite::Message;
    let temp = tempfile::tempdir().unwrap();
    let state = state(&temp).await;
    let socket_dir = tempfile::tempdir().unwrap();
    let socket = socket_dir.path().join("exec.sock");
    let unix = tokio::net::UnixListener::bind(&socket).unwrap();
    let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = tcp.local_addr().unwrap();
    let servers = [
        tokio::spawn(serve(unix, api_router(state.clone(), FakeAuth)).into_future()),
        tokio::spawn(serve(tcp, router(state, FakeAuth)).into_future()),
    ];
    let request = |service: &str| ExecRequest {
        service: service.parse().unwrap(),
        command: ExecCommand::parse(vec!["invite".into(), "create".into()]).unwrap(),
        stdin: true,
        tty: None,
    };
    let app = create_and_inspect(&Client::unix(&socket), &manifest()).await;
    for client in [
        Client::unix(&socket),
        Client::tcp(&format!("http://{address}/")).unwrap(),
    ] {
        let (mut output, mut input) = client
            .exec(&app.environment_id, &request("web"))
            .await
            .unwrap();
        input
            .send(&ExecInput::Stdin(b"hello".to_vec()))
            .await
            .unwrap();
        assert!(
            matches!(output.next().await, Ok(Some(ExecOutput::Stdout(data))) if data == b"hello")
        );
        input.send(&ExecInput::CloseStdin).await.unwrap();
        assert!(
            matches!(output.next().await, Ok(Some(ExecOutput::Stderr(data))) if data == b"closed")
        );
        assert!(matches!(output.next().await, Ok(Some(ExecOutput::Exit(5)))));
        assert!(matches!(output.next().await, Ok(None)));

        // Start failures arrive after the handshake, with their HTTP status.
        let (mut output, _input) = client
            .exec(&app.environment_id, &request("worker"))
            .await
            .unwrap();
        assert!(matches!(
            output.next().await,
            Ok(Some(ExecOutput::Failed { status: 409, error }))
                if error.code == "service_not_running" && !error.request_id.is_empty()
        ));
    }
    // Completion is recorded before the final frame is sent.
    let commands = Client::unix(&socket)
        .events(None, Some(&app.environment_id), None, 100)
        .await
        .unwrap()
        .items
        .into_iter()
        .filter(|event| event.kind.starts_with("command_"))
        .collect::<Vec<_>>();
    assert_eq!(commands.len(), 4);
    for event in &commands {
        assert_eq!(event.resource.as_deref(), Some("web"));
        assert!(!event.message.as_deref().unwrap().contains("invite"));
    }
    assert_eq!(
        commands[1].message.as_deref(),
        Some("Command in task task-1 exited with code 5")
    );

    // A client detaching from a quiet session gets a clean Close reply.
    let (mut socket, _) = tokio_tungstenite::client_async(
        format!(
            "ws://{address}/api/v1/environments/{}/exec",
            app.environment_id
        ),
        tokio::net::TcpStream::connect(address).await.unwrap(),
    )
    .await
    .unwrap();
    let start = serde_json::to_string(&request("web")).unwrap();
    socket.send(Message::text(start)).await.unwrap();
    socket.close(None).await.unwrap();
    let reply = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next()).await;
    assert!(
        matches!(reply, Ok(Some(Ok(Message::Close(_))))),
        "{reply:?}"
    );
    for server in servers {
        server.abort();
    }
}

#[tokio::test]
async fn readiness_distinguishes_engine_reachability_and_does_not_gate_saves() {
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    assert!(api.client.system_readiness().await.unwrap().ready);
    api.runtime
        .unavailable
        .store(true, std::sync::atomic::Ordering::Relaxed);
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
    let response = router(
        ApiState::new(api.store.clone(), api.runtime.clone()),
        FakeAuth,
    )
    .oneshot(
        Request::builder()
            .header("host", "localhost")
            .uri("/api/v1/system/readiness")
            .body(Body::empty())
            .unwrap(),
    )
    .await
    .unwrap();
    assert_eq!(response.status(), 503);
    api.client
        .apply_application(&AcceptanceApi::request())
        .await
        .unwrap();
    assert_eq!(api.client.system_status().await.unwrap().status, "running");
}

#[tokio::test]
async fn service_and_http_share_acceptance_receipts_and_application_views() {
    use piqueld::api::{ApplicationService, Mutation, MutationResponse};
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let service = ApplicationService::new(api.store.clone(), api.runtime.clone());
    let request = AcceptanceApi::request();
    // The same account as the HTTP client, since receipts replay only for theirs.
    let caller = piqueld::api::Actor::Account(piqueld::store::Caller {
        credential_id: "contract-admin",
        user_id: "contract-admin",
    });
    let MutationResponse::Saved(saved) = service
        .accept(
            caller,
            Mutation::save(
                request.manifest.clone().validate_template().unwrap(),
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
        .apply_and_deploy(&request)
        .await
        .unwrap();
    assert_eq!(saved.operation_id, Some(replay.operation_id));
    assert_eq!(saved.generation, replay.generation);
    let id = piqueld_core::EnvironmentId::parse(&saved.application_id).unwrap();
    let direct = service.clone().environment_detail(&id).await.unwrap();
    let response = api_router(service.clone(), FakeAuth)
        .oneshot(
            Request::builder()
                .header("host", "localhost")
                .uri(format!("/api/v1/environments/{id}/detail"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: serde_json::Value =
        serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap();
    assert_eq!(body["data"], serde_json::to_value(direct).unwrap());

    let mut changed = request.clone();
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

#[tokio::test]
async fn route_field_edits_follow_service_renames_and_removals() {
    use piqueld_client::{
        Route,
        edit::{ApplicationEdit, EditOptions, ServiceEdit},
    };
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let saved = api
        .client
        .apply_application(&AcceptanceApi::request())
        .await
        .unwrap();
    let id = &saved.application_id;
    let route = Route::service("notes.example.com".into(), "web".into(), 3000);
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
        Job, JobRun,
        edit::{ApplicationEdit, EditOptions},
    };
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let saved = api
        .client
        .apply_application(&AcceptanceApi::request())
        .await
        .unwrap();
    let id = &saved.application_id;
    let job = |name: &str, service: &str| Job {
        name: name.into(),
        service: service.into(),
        command: vec!["notes".into(), name.into()],
        run: JobRun::BeforeRollout,
        timeout_seconds: 60,
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
        matches!(&error, piqueld_client::ClientError::Api { status, error }
            if *status == http::StatusCode::UNPROCESSABLE_ENTITY
                && error.details.to_string().contains("job_service_missing")),
        "{error:?}"
    );
    assert_eq!(jobs_of().await, jobs);
    let events = api.client.events(Some(id), None, None, 100).await.unwrap();
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
    api.runtime
        .unavailable
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let saved = api
        .client
        .apply_application(&AcceptanceApi::request())
        .await
        .unwrap();
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
            .deployments(id, None)
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
    assert!(
        matches!(stale, piqueld_client::ClientError::Api { error, .. } if error.code == "generation_conflict")
    );
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
async fn field_edits_validate_atomically_and_preserve_git_ownership() {
    use piqueld_client::{
        GitRepository, RepositoryManifest,
        edit::{ApplicationEdit, EditOptions, ServiceEdit},
    };
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let saved = api
        .client
        .apply_application(&AcceptanceApi::request())
        .await
        .unwrap();
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
        matches!(error, piqueld_client::ClientError::Api { status, error } if status == http::StatusCode::UNPROCESSABLE_ENTITY && !error.details.is_null())
    );
    assert_eq!(api.client.application(id).await.unwrap().generation, 1);
    let repository = ApplicationEdit::Repository(Some(RepositoryManifest {
        repository: GitRepository {
            url: "https://example.com/infra.git".into(),
            branch: "main".into(),
            commit: None,
        },
        path: "app.toml".into(),
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
    let blocked = ApplicationEdit::Service {
        name: "web".into(),
        edit: ServiceEdit::Replicas(2.into()),
    };
    let error = api
        .client
        .edit_application(id, &blocked, &options)
        .await
        .unwrap_err();
    assert!(
        matches!(error, piqueld_client::ClientError::Api { error, .. } if error.code == "repository_managed")
    );
    api.client
        .edit_application(
            id,
            &ApplicationEdit::RepositoryPath("release.toml".into()),
            &options,
        )
        .await
        .unwrap();
    let before = api
        .client
        .application(id)
        .await
        .unwrap()
        .application
        .to_manifest();
    api.client
        .edit_application(id, &ApplicationEdit::Repository(None), &options)
        .await
        .unwrap();
    let after = api
        .client
        .application(id)
        .await
        .unwrap()
        .application
        .to_manifest();
    assert_eq!(after.spec.services, before.spec.services);
    assert!(after.spec.manifest.is_none());
    api.client
        .edit_application(id, &blocked, &options)
        .await
        .unwrap();
}

#[tokio::test]
async fn concurrent_field_edits_reject_stale_revisions_without_losing_updates() {
    use piqueld_client::edit::{ApplicationEdit, EditOptions, ServiceEdit};
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let saved = api
        .client
        .apply_application(&AcceptanceApi::request())
        .await
        .unwrap();
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
    assert!(
        matches!(error, piqueld_client::ClientError::Api { error, .. } if error.code == "generation_conflict")
    );
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

impl AcceptanceApi {
    async fn edit_field(
        &self,
        id: &str,
        edit: piqueld_client::edit::ApplicationEdit,
    ) -> piqueld_core::manifest::ApplicationManifest {
        let generation = self.client.application(id).await.unwrap().generation;
        self.client
            .edit_application(
                id,
                &edit,
                &piqueld_client::edit::EditOptions {
                    expected_generation: Some(generation),
                    ..piqueld_client::edit::EditOptions::default()
                },
            )
            .await
            .unwrap();
        self.client
            .application(id)
            .await
            .unwrap()
            .application
            .to_manifest()
    }
    async fn edit_service_field(
        &self,
        id: &str,
        edit: piqueld_client::edit::ServiceEdit,
    ) -> piqueld_core::manifest::Service {
        self.edit_field(
            id,
            piqueld_client::edit::ApplicationEdit::Service {
                name: "web".into(),
                edit,
            },
        )
        .await
        .spec
        .services
        .remove(0)
    }
}

#[tokio::test]
async fn field_edit_source_endpoints_preserve_other_git_settings() {
    use piqueld_client::{Build, GitRepository, SourceRepository, edit::ServiceEdit};
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let saved = api
        .client
        .apply_application(&AcceptanceApi::request())
        .await
        .unwrap();
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
    let saved = api
        .client
        .apply_application(&AcceptanceApi::request())
        .await
        .unwrap();
    let id = &saved.application_id;
    api.edit_service_field(
        id,
        ServiceEdit::Healthcheck(Some(HealthCheck::Http {
            port: 8080.into(),
            path: "/health".into(),
            interval_seconds: 10.into(),
            timeout_seconds: 3.into(),
        })),
    )
    .await;
    api.edit_service_field(id, ServiceEdit::HealthPort(9090.into()))
        .await;
    api.edit_service_field(id, ServiceEdit::HealthPath("/live".into()))
        .await;
    api.edit_service_field(id, ServiceEdit::HealthInterval(20.into()))
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
            timeout_seconds: 5.into()
        })
    );
    api.edit_service_field(
        id,
        ServiceEdit::Healthcheck(Some(HealthCheck::Command {
            command: vec!["true".into()],
            interval_seconds: 10.into(),
            timeout_seconds: 3.into(),
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
        matches!(error, piqueld_client::ClientError::Api { status, .. } if status == http::StatusCode::UNPROCESSABLE_ENTITY)
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
    let saved = api
        .client
        .apply_application(&AcceptanceApi::request())
        .await
        .unwrap();
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
    assert!(
        matches!(error, piqueld_client::ClientError::Api { error, .. } if error.code == "request_id_conflict")
    );
}

#[tokio::test]
async fn field_edit_requires_an_explicit_value_and_revision() {
    let temp = tempfile::tempdir().unwrap();
    let state = state(&temp).await;
    let saved = state
        .accept(
            Daemon,
            piqueld::api::Mutation::save(manifest().validate_template().unwrap(), None, false),
            Some(0),
            false,
            None,
        )
        .await
        .unwrap();
    let piqueld::api::MutationResponse::Saved(saved) = saved else {
        panic!("save receipt")
    };
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
            .clone()
            .oneshot(
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
            .await
            .unwrap();
        assert_eq!(response.status(), status, "body {body}");
    }
}

#[tokio::test]
async fn typed_edits_record_safe_field_history() {
    use piqueld_client::edit::{ApplicationEdit, EditOptions, ServiceEdit};
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let saved = api
        .client
        .apply_application(&AcceptanceApi::request())
        .await
        .unwrap();
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
        .events(Some(&saved.application_id), None, None, 100)
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

#[tokio::test]
async fn diagnostic_ids_correlate_api_failures_and_metrics_routes_are_isolated() {
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let app = api
        .client
        .apply_and_deploy(&AcceptanceApi::request())
        .await
        .unwrap();
    api.runtime
        .unavailable
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let error = api
        .client
        .environment_logs(&app.environment_id, Some("web"), 5, 60)
        .await
        .unwrap_err();
    let piqueld_client::ClientError::Api { error, .. } = error else {
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
    let metrics =
        piqueld::api::http::metrics_router(ApiState::new(api.store.clone(), api.runtime.clone()));
    let response = metrics.clone().oneshot(request("/metrics")).await.unwrap();
    assert_eq!(response.status(), 200);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    assert!(
        std::str::from_utf8(&body)
            .unwrap()
            .contains("piqueld_database_bytes")
    );
    assert_eq!(
        metrics
            .oneshot(request("/api/v1/applications"))
            .await
            .unwrap()
            .status(),
        404
    );
}

#[tokio::test]
async fn event_stream_replays_after_cursor_and_rejects_pruned_history() {
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let app = api
        .client
        .apply_and_deploy(&AcceptanceApi::request())
        .await
        .unwrap();
    api.finish_operation(
        &app.operation_id,
        Some(("service_update_failed", "Update paused")),
    )
    .await;
    let events = api
        .client
        .events(None, None, None, 100)
        .await
        .unwrap()
        .items;
    let after = events[events.len() - 2].id;
    let router = router(
        ApiState::new(api.store.clone(), api.runtime.clone()),
        FakeAuth,
    );
    for limit in [0, 101] {
        let response = router
            .clone()
            .oneshot(request(&format!("/api/v1/events/stream?limit={limit}")))
            .await
            .unwrap();
        assert_eq!(response.status(), 400);
    }
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/api/v1/events/stream?errors_only=true&limit=1")
                .header("host", "localhost")
                .header("last-event-id", format!("v1:{after}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let mut body = response.into_body();
    let frame = tokio::time::timeout(std::time::Duration::from_secs(2), async {
        let mut bytes = Vec::new();
        loop {
            let frame = body.frame().await.unwrap().unwrap();
            if let Ok(data) = frame.into_data() {
                bytes.extend_from_slice(&data);
                if let Some(end) = bytes.windows(2).position(|window| window == b"\n\n") {
                    bytes.truncate(end);
                    return bytes;
                }
            }
        }
    })
    .await
    .unwrap();
    let text = std::str::from_utf8(&frame).unwrap();
    assert!(text.contains("operation_failed"));
    assert!(text.contains(&format!("id: v1:{}", events.last().unwrap().id)));
    api.store.prune_events(i64::MAX).await.unwrap();
    let response = router
        .oneshot(request(&format!("/api/v1/events/stream?cursor=v1:{after}")))
        .await
        .unwrap();
    assert_eq!(response.status(), 410);
}

/// Every documented operation except the public sign-in endpoints must reject
/// anonymous callers on both the website and Unix-socket routers.
#[tokio::test]
async fn every_documented_operation_requires_authentication() {
    const PUBLIC: &[&str] = &[
        "/api/v1/auth/status",
        "/api/v1/auth/setup-link",
        "/api/v1/auth/register/start",
        "/api/v1/auth/register/finish",
        "/api/v1/auth/login/start",
        "/api/v1/auth/login/finish",
        "/api/v1/auth/device/start",
        "/api/v1/auth/device/poll",
    ];
    let temp = TempDir::new().unwrap();
    let store = Arc::new(Store::open(temp.path().join("state.db")).await.unwrap());
    let auth = piqueld::auth::Auth::new(&store, "https://piqueld.example").unwrap();
    let instance = InstanceId::parse(store.instance_id().to_owned()).unwrap();
    let state = ApiState::new(
        Arc::clone(&store),
        Arc::new(FakeRuntime {
            cleanup_gate: None,
            instance,
            unavailable: std::sync::atomic::AtomicBool::new(false),
        }),
    );
    let document = piqueld::api::http::openapi_document();
    let paths = document["paths"].as_object().unwrap();
    let mut checked = 0;
    for listener in [
        web_router(state.clone(), UiAssets::Embedded(TEST_BUNDLE), auth.clone()),
        api_router(state.clone(), auth.clone()),
    ] {
        for (path, item) in paths {
            if PUBLIC.contains(&path.as_str()) {
                continue;
            }
            let uri = path
                .split('/')
                .map(|segment| {
                    if segment.starts_with('{') {
                        "placeholder"
                    } else {
                        segment
                    }
                })
                .collect::<Vec<_>>()
                .join("/");
            for method in ["get", "post", "put", "patch", "delete"] {
                if item.get(method).is_none() {
                    continue;
                }
                let method = method.to_ascii_uppercase();
                let response = listener
                    .clone()
                    .oneshot(
                        Request::builder()
                            .method(method.as_str())
                            .uri(&uri)
                            .header("content-type", "application/json")
                            .header("x-request-id", "anonymous-contract")
                            .body(Body::from("{}"))
                            .unwrap(),
                    )
                    .await
                    .unwrap();
                assert_eq!(
                    response.status(),
                    axum::http::StatusCode::UNAUTHORIZED,
                    "{method} {path} must require authentication"
                );
                assert_eq!(response.headers()["x-request-id"], "anonymous-contract");
                let body = response.into_body().collect().await.unwrap().to_bytes();
                let error: piqueld_core::api::ErrorBody = serde_json::from_slice(&body).unwrap();
                assert_eq!(error.request_id, "anonymous-contract");
                checked += 1;
            }
        }
    }
    assert!(checked > 60, "only {checked} operations were checked");
}

/// Seeds an account holding `grants` (permission, optional application ID)
/// with a non-expiring token, returning the token. The account and credential
/// share `name` as their ID; seeding an existing account again adds grants.
async fn seed_account(
    database: &std::path::Path,
    name: &str,
    grants: &[(&str, Option<&str>)],
) -> String {
    use base64::Engine as _;
    use sha2::Digest as _;
    use sqlx::Connection as _;
    let token = format!("{name:x<43}");
    let hash = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(token.as_bytes()));
    let mut connection = sqlx::SqliteConnection::connect(&format!("sqlite:{}", database.display()))
        .await
        .unwrap();
    sqlx::query(
        "INSERT OR IGNORE INTO auth_users(id,username,display_name,created_at) VALUES(?1,?1,'',1)",
    )
    .bind(name)
    .execute(&mut connection)
    .await
    .unwrap();
    sqlx::query("INSERT OR IGNORE INTO auth_credentials(id,user_id,secret_hash,kind,name,created_at,last_used_at) VALUES(?1,?1,?2,'token','test',1,4102444800)")
        .bind(name)
        .bind(hash)
        .execute(&mut connection)
        .await
        .unwrap();
    for (permission, application) in grants {
        sqlx::query("INSERT INTO auth_grants(user_id,permission,application_id) VALUES(?1,?2,?3)")
            .bind(name)
            .bind(permission)
            .bind(application)
            .execute(&mut connection)
            .await
            .unwrap();
    }
    token
}

/// Two applications, `blog` and `shop`, served by the real authenticator, with
/// a `deployer` holding `apps:deploy` on `blog`, a `creator` holding
/// `apps:create` plus `apps:write` on `blog`, a `lead` holding only
/// `accounts:manage`, and an `auditor` holding only `audit:read`.
struct GrantFixture {
    temp: TempDir,
    state: ApiState,
    router: axum::Router,
    blog: String,
    shop: String,
    deployer: String,
    creator: String,
    lead: String,
    auditor: String,
}

impl GrantFixture {
    async fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let database = temp.path().join("state.db");
        let state = state(&temp).await;
        let mut ids = Vec::new();
        for name in ["blog", "shop"] {
            let mut input = manifest();
            input.metadata.name = name.into();
            let mutation =
                piqueld::api::Mutation::save(input.validate_template().unwrap(), None, false);
            let actor = Daemon;
            let response = state.accept(actor, mutation, Some(0), false, None).await;
            let Ok(piqueld::api::MutationResponse::Saved(saved)) = response else {
                panic!("save response");
            };
            ids.push(saved.application_id);
        }
        let blog = Some(ids[0].as_str());
        let deployer = seed_account(&database, "deployer", &[("apps:deploy", blog)]).await;
        let creator = [("apps:create", None), ("apps:write", blog)];
        let creator = seed_account(&database, "creator", &creator).await;
        let lead = seed_account(&database, "lead", &[("accounts:manage", None)]).await;
        let auditor = seed_account(&database, "auditor", &[("audit:read", None)]).await;
        let store = Store::open(&database).await.unwrap();
        let auth = piqueld::auth::Auth::new(&store, "https://piqueld.example").unwrap();
        let [blog, shop] = <[String; 2]>::try_from(ids).unwrap();
        Self {
            temp,
            router: api_router(state.clone(), auth),
            state,
            blog,
            shop,
            deployer,
            creator,
            lead,
            auditor,
        }
    }

    /// Adds environment `name` to `application`, returning its route.
    async fn environment(&self, application: &str, name: &str) -> String {
        let create = piqueld::api::Mutation::CreateEnvironment {
            application: piqueld_core::ApplicationId::parse(application).unwrap(),
            name: piqueld_core::EnvironmentName::parse(name).unwrap(),
            branch: None,
        };
        let Ok(piqueld::api::MutationResponse::Environment(environment)) =
            self.state.accept(Daemon, create, None, true, None).await
        else {
            panic!("environment");
        };
        format!("/api/v1/environments/{}", environment.id)
    }

    /// Reads the audit trail at `uri` as `token` once it holds `count`
    /// records, since they are written in the background.
    async fn audit(&self, token: &str, uri: &str, count: usize) -> serde_json::Value {
        // Records are written in the background; allow for a loaded machine.
        for _ in 0..500 {
            let (status, body) = self.call(token, "GET", uri, serde_json::Value::Null).await;
            assert_eq!(status, 200, "{body}");
            if body["data"]["items"].as_array().unwrap().len() >= count {
                return body;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("audit trail at {uri} never held {count} records");
    }

    /// Sends a JSON request as `token`, returning the status and JSON body.
    async fn call(
        &self,
        token: &str,
        method: &str,
        uri: &str,
        body: serde_json::Value,
    ) -> (u16, serde_json::Value) {
        let request = Request::builder()
            .method(method)
            .uri(uri)
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap();
        let response = self.router.clone().oneshot(request).await.unwrap();
        let status = response.status().as_u16();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&body).unwrap_or_default())
    }
}

/// Hidden applications and their environments look absent; visible ones name
/// the missing permission. Environments are checked on their application.
/// Accounts without application grants list nothing rather than being refused.
#[tokio::test]
async fn grants_hide_applications_and_name_missing_permissions() {
    let f = GrantFixture::new().await;
    let none = serde_json::Value::Null;
    for uri in ["/api/v1/applications", "/api/v1/builds"] {
        let (status, body) = f.call(&f.lead, "GET", uri, none.clone()).await;
        assert_eq!(status, 200, "{uri}");
        assert_eq!(body["data"]["items"], serde_json::json!([]), "{uri}");
    }
    let (status, body) = f
        .call(&f.deployer, "GET", "/api/v1/applications", none.clone())
        .await;
    assert_eq!(status, 200);
    let listed: Vec<_> = body["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap())
        .collect();
    assert_eq!(listed, [f.blog.as_str()]);
    for uri in [
        format!("/api/v1/applications/{}", f.shop),
        format!("/api/v1/environments/{}", f.shop),
        format!("/api/v1/environments/{}/exec", f.shop),
        "/api/v1/environments/env-unknown".to_owned(),
    ] {
        assert_eq!(
            f.call(&f.deployer, "GET", &uri, none.clone()).await.0,
            404,
            "{uri}"
        );
    }
    let deploy = format!("/api/v1/environments/{}/deploy?force=true", f.blog);
    assert_eq!(
        f.call(&f.deployer, "POST", &deploy, none.clone()).await.0,
        202
    );
    let staging = serde_json::json!({"name": "staging"});
    for (uri, method, body, permission) in [
        (
            format!("/api/v1/applications/{}?force=true", f.blog),
            "DELETE",
            none.clone(),
            "apps:delete",
        ),
        (
            format!("/api/v1/environments/{}?force=true", f.blog),
            "DELETE",
            none.clone(),
            "apps:delete",
        ),
        (
            format!("/api/v1/applications/{}/environments?force=true", f.blog),
            "POST",
            staging,
            "apps:write",
        ),
        (
            format!("/api/v1/environments/{}/exec", f.blog),
            "GET",
            none.clone(),
            "apps:exec",
        ),
        (
            "/api/v1/system/configuration".to_owned(),
            "GET",
            none.clone(),
            "system:read",
        ),
    ] {
        let (status, body) = f.call(&f.deployer, method, &uri, body).await;
        assert_eq!(status, 403, "{method} {uri}");
        assert_eq!(body["code"], "permission_denied");
        assert_eq!(body["details"]["permission"], permission);
    }
}

/// Refusals (including unauthenticated ones), writes, and sensitive reads
/// enter the audit trail with their caller and outcome, routine reads do not,
/// and only `audit:read` reveals other accounts' trails.
#[tokio::test]
async fn audit_trail_records_callers_and_outcomes() {
    let f = GrantFixture::new().await;
    let none = serde_json::Value::Null;
    let blog = format!("/api/v1/applications/{}", f.blog);
    let shop = format!("/api/v1/applications/{}", f.shop);
    let staging = f.environment(&f.blog, "staging").await;
    for (method, uri, status) in [
        ("DELETE", format!("{blog}?force=true"), 403),
        ("GET", shop, 404),
        ("POST", format!("{staging}/deploy?force=true"), 202),
        ("GET", format!("{staging}/exec"), 403),
        ("GET", format!("{blog}/manifest"), 200),
        ("GET", "/api/v1/applications".to_owned(), 200),
    ] {
        assert_eq!(
            f.call(&f.deployer, method, &uri, none.clone()).await.0,
            status
        );
    }
    let body = f.audit(&f.deployer, "/api/v1/audit", 5).await;
    // Records are written in the background, so their order is not asserted.
    // Requests about an environment also name its application.
    let trail: std::collections::BTreeSet<_> = body["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| {
            assert_eq!(event["user_id"], "deployer");
            assert_eq!(event["credential_kind"], "token");
            if !event["environment_id"].is_null() {
                assert_eq!(event["application_id"], f.blog.as_str());
            }
            (
                event["outcome"].as_str().unwrap().to_owned(),
                event["action"].as_str().unwrap().to_owned(),
                event["permission"].as_str().map(str::to_owned),
            )
        })
        .collect();
    let (id, environment) = ("/api/v1/applications/{id}", "/api/v1/environments/{id}");
    assert_eq!(
        trail,
        [
            ("allowed".into(), format!("POST {environment}/deploy"), None),
            (
                "denied".into(),
                format!("GET {environment}/exec"),
                Some("apps:exec".into())
            ),
            ("allowed".into(), format!("GET {id}/manifest"), None),
            ("denied".into(), format!("GET {id}"), None),
            (
                "denied".into(),
                format!("DELETE {id}"),
                Some("apps:delete".into())
            ),
        ]
        .into()
    );
    let others = "/api/v1/audit?user_id=creator";
    assert_eq!(
        f.call(&f.deployer, "GET", others, none.clone()).await.0,
        403
    );
    // A cross-site write is refused, yet still names its valid caller.
    let foreign = Request::post("/api/v1/applications")
        .header("authorization", format!("Bearer {}", f.deployer))
        .header("origin", "https://attacker.example")
        .body(Body::empty())
        .unwrap();
    let response = f.router.clone().oneshot(foreign).await.unwrap();
    assert_eq!(response.status(), 403);
    // So is a throttled one, once the peer's sign-in start allowance is spent.
    let mut status = 0;
    for _ in 0..31 {
        let start = serde_json::json!({});
        status = f
            .call(&f.deployer, "POST", "/api/v1/auth/device/start", start)
            .await
            .0;
    }
    assert_eq!(status, 429);
    let unknown = "pqd_0000000000000000000000000000000000000000000";
    let (status, _) = f.call(unknown, "GET", "/api/v1/applications", none).await;
    assert_eq!(status, 401);
    let body = f.audit(&f.auditor, "/api/v1/audit?outcome=denied", 7).await;
    let anonymous: Vec<_> = body["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["user_id"].is_null())
        .map(|event| event["status"].as_u64().unwrap())
        .collect();
    assert_eq!(anonymous, [401]);
    let deployer = "/api/v1/audit?user_id=deployer&outcome=denied";
    let body = f.audit(&f.auditor, deployer, 6).await;
    assert_eq!(body["data"]["items"].as_array().unwrap().len(), 6);
    let metrics = f.state.prometheus_metrics().await.unwrap();
    assert!(
        metrics.contains("piqueld_access_denied_total 7"),
        "{metrics}"
    );
}

/// Callers read their own trail, so it reveals nothing they could not see:
/// a hidden environment's application is not recorded, and an API token
/// reads only its own requests, not everything its account did.
#[tokio::test]
async fn own_audit_trails_reveal_nothing_hidden() {
    let f = GrantFixture::new().await;
    let none = serde_json::Value::Null;
    let hidden = format!("/api/v1/environments/{}", f.shop);
    assert_eq!(
        f.call(&f.deployer, "GET", &hidden, none.clone()).await.0,
        404
    );
    // Refusals before authorization, like a cross-site request, are no hint.
    let foreign = Request::delete(&hidden)
        .header("authorization", format!("Bearer {}", f.deployer))
        .header("origin", "https://attacker.example")
        .body(Body::empty())
        .unwrap();
    let response = f.router.clone().oneshot(foreign).await.unwrap();
    assert_eq!(response.status(), 403);
    let body = f.audit(&f.deployer, "/api/v1/audit", 2).await;
    for event in body["data"]["items"].as_array().unwrap() {
        assert_eq!(event["environment_id"], f.shop.as_str());
        assert!(event["application_id"].is_null(), "{event}");
    }
    // Usernames match regardless of case, like accounts' own.
    f.audit(&f.auditor, "/api/v1/audit?username=DEPLOYER", 2)
        .await;
    let create = serde_json::json!({
        "action": "create_token",
        "grants": [{"permission": "apps:deploy", "applications": [f.blog]}],
        "name": "ci",
        "days": 1
    });
    let (status, created) = f
        .call(&f.deployer, "POST", "/api/v1/auth/manage", create)
        .await;
    assert_eq!(status, 200, "{created}");
    let token = created["token"].as_str().unwrap();
    let manifest = format!("/api/v1/applications/{}/manifest", f.blog);
    assert_eq!(f.call(token, "GET", &manifest, none.clone()).await.0, 200);
    let body = f.audit(token, "/api/v1/audit", 1).await;
    let items = body["data"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{body}");
    assert_eq!(items[0]["action"], "GET /api/v1/applications/{id}/manifest");
    let account = "/api/v1/audit?credential_id=deployer";
    assert_eq!(f.call(token, "GET", account, none).await.0, 403);
}

/// A command refused when it starts, after its connection was authorized
/// and audited as allowed, is audited and counted as a refusal too.
#[tokio::test]
async fn exec_refusals_after_the_upgrade_are_audited() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::{Message, client::IntoClientRequest};
    let f = GrantFixture::new().await;
    let database = f.temp.path().join("state.db");
    let grants = [
        ("apps:exec", Some(f.blog.as_str())),
        ("apps:read", Some(f.blog.as_str())),
    ];
    let operator = seed_account(&database, "operator", &grants).await;
    let tcp = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = tcp.local_addr().unwrap();
    let server = tokio::spawn(serve(tcp, f.router.clone()).into_future());
    let url = format!("ws://{address}/api/v1/environments/{}/exec", f.blog);
    let mut request = url.into_client_request().unwrap();
    let bearer = format!("Bearer {operator}").parse().unwrap();
    request.headers_mut().insert("authorization", bearer);
    let stream = tokio::net::TcpStream::connect(address).await.unwrap();
    let (mut socket, _) = tokio_tungstenite::client_async(request, stream)
        .await
        .unwrap();
    let mut connection = <sqlx::SqliteConnection as sqlx::Connection>::connect(&format!(
        "sqlite:{}",
        database.display()
    ))
    .await
    .unwrap();
    sqlx::query("DELETE FROM auth_grants WHERE user_id='operator' AND permission='apps:exec'")
        .execute(&mut connection)
        .await
        .unwrap();
    let start = serde_json::json!({"service": "web", "command": ["true"], "stdin": false});
    socket.send(Message::text(start.to_string())).await.unwrap();
    while let Some(Ok(message)) = socket.next().await {
        if message.is_close() {
            break;
        }
    }
    let body = f
        .audit(&f.auditor, "/api/v1/audit?user_id=operator", 2)
        .await;
    let outcomes: Vec<_> = body["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| {
            assert_eq!(event["action"], "GET /api/v1/environments/{id}/exec");
            (event["outcome"].clone(), event["permission"].clone())
        })
        .collect();
    assert!(
        outcomes.contains(&("denied".into(), "apps:exec".into())),
        "{outcomes:?}"
    );
    assert!(
        outcomes.contains(&("allowed".into(), serde_json::Value::Null)),
        "{outcomes:?}"
    );
    server.abort();
}

/// A CLI login's approval and the poll that signs it in are audited for the
/// approving account; polls still awaiting approval are not.
#[tokio::test]
async fn device_sign_ins_are_audited() {
    use serde_json::json;
    let f = GrantFixture::new().await;
    let start = "/api/v1/auth/device/start";
    let poll = "/api/v1/auth/device/poll";
    let mut codes = Vec::new();
    for _ in 0..2 {
        let (status, body) = f.call(&f.deployer, "POST", start, json!({})).await;
        assert_eq!(status, 200, "{body}");
        codes.push((body["device_code"].clone(), body["user_code"].clone()));
    }
    let [(pending, _), (approved, user_code)] = <[_; 2]>::try_from(codes).unwrap();
    let (_, body) = f
        .call(&f.deployer, "POST", poll, json!({"device_code": pending}))
        .await;
    assert_eq!(body["status"], "authorization_pending");
    let approve = json!({"user_code": user_code});
    let (status, _) = f
        .call(&f.deployer, "POST", "/api/v1/auth/device/approve", approve)
        .await;
    assert_eq!(status, 200);
    let (_, body) = f
        .call(&f.deployer, "POST", poll, json!({"device_code": approved}))
        .await;
    assert_eq!(body["status"], "complete", "{body}");
    // Two starts, the approval, and the completing poll.
    let body = f.audit(&f.auditor, "/api/v1/audit", 4).await;
    let polls: Vec<_> = body["data"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|event| event["action"] == format!("POST {poll}"))
        .map(|event| (event["outcome"].clone(), event["user_id"].clone()))
        .collect();
    assert_eq!(polls, [(json!("allowed"), json!("deployer"))]);
}

/// Creating requires `apps:create` and grants the creator matching access;
/// saving an unreadable application by name is refused rather than absent.
#[tokio::test]
async fn creators_receive_matching_grants_on_new_applications() {
    let f = GrantFixture::new().await;
    let name = |value: &str| serde_json::json!({"value": value});
    let (status, body) = f
        .call(&f.creator, "POST", "/api/v1/applications", name("notes"))
        .await;
    assert_eq!(status, 200, "{body}");
    let created = format!(
        "/api/v1/applications/{}",
        body["data"]["application_id"].as_str().unwrap()
    );
    let none = serde_json::Value::Null;
    assert_eq!(f.call(&f.creator, "GET", &created, none).await.0, 200);
    let mut input = manifest();
    input.metadata.name = "shop".into();
    let apply = serde_json::json!({"manifest": input});
    let (status, body) = f
        .call(
            &f.creator,
            "POST",
            "/api/v1/applications/apply?force=true",
            apply,
        )
        .await;
    assert_eq!(status, 403, "{body}");
    assert_eq!(body["details"]["permission"], "apps:write");
    let (status, _) = f
        .call(&f.deployer, "POST", "/api/v1/applications", name("other"))
        .await;
    assert_eq!(status, 403);
}

/// Before the first account exists, only the Unix socket reveals the setup
/// link; remote requests for it are audited as refusals.
#[tokio::test]
async fn setup_link_is_served_only_over_the_unix_socket() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp).await;
    let store = Store::open(temp.path().join("state.db")).await.unwrap();
    let auth = piqueld::auth::Auth::new(&store, "https://piqueld.example").unwrap();
    let path = temp.path().join("setup-link");
    auth.prepare_setup(&path).await.unwrap();
    let link = std::fs::read_to_string(&path).unwrap();
    for (listener, status) in [
        (
            web_router(state.clone(), UiAssets::Embedded(TEST_BUNDLE), auth.clone()),
            404,
        ),
        (api_router(state.clone(), auth.clone()), 200),
    ] {
        let response = listener
            .oneshot(
                Request::builder()
                    .uri("/api/v1/auth/setup-link")
                    .header("host", "localhost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), status);
        if status == 200 {
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let setup: piqueld_core::auth::SetupLink = serde_json::from_slice(&body).unwrap();
            assert_eq!(setup.url, link.trim());
        }
    }
    // The refused remote request is audited in the background.
    let denied = piqueld_core::audit::AuditFilter {
        outcome: Some(piqueld_core::audit::AuditOutcome::Denied),
        ..Default::default()
    };
    for _ in 0..100 {
        let page = state.audit_events(&denied, None, 10).await.unwrap();
        if let [event] = page.items.as_slice() {
            assert_eq!(
                (event.action.as_str(), event.status),
                ("GET /api/v1/auth/setup-link", 404)
            );
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("the refused setup link request was not audited");
}

/// Authentication short circuits still use the common error correlation layers,
/// including storage failures that must not attempt another database write.
#[tokio::test]
async fn authentication_errors_preserve_request_and_diagnostic_ids() {
    use sqlx::Connection as _;

    let temp = TempDir::new().unwrap();
    let state = state(&temp).await;
    let store = Store::open(temp.path().join("state.db")).await.unwrap();
    let mut connection = sqlx::SqliteConnection::connect(&format!(
        "sqlite:{}",
        temp.path().join("state.db").display()
    ))
    .await
    .unwrap();
    // Leave the observability tables healthy, but break authentication reads.
    sqlx::query("ALTER TABLE auth_credentials RENAME TO unavailable_credentials")
        .execute(&mut connection)
        .await
        .unwrap();
    // A diagnostic write would block behind this transaction. Storage failures
    // must return promptly with log-only diagnostic IDs instead.
    let transaction = connection.begin_with("BEGIN IMMEDIATE").await.unwrap();
    for website in [false, true] {
        let auth = piqueld::auth::Auth::new(&store, "https://piqueld.example").unwrap();
        let listener = if website {
            web_router(state.clone(), UiAssets::Embedded(TEST_BUNDLE), auth)
        } else {
            api_router(state.clone(), auth)
        };
        for (method, path, bearer, origin, status, code) in [
            (
                "GET",
                "/api/v1/system/status",
                "invalid".to_owned(),
                "",
                401,
                "authentication_required",
            ),
            (
                "POST",
                "/api/v1/auth/login/start",
                String::new(),
                "https://other.example",
                403,
                "origin_mismatch",
            ),
            (
                "GET",
                "/api/v1/auth/me",
                "x".repeat(43),
                "",
                503,
                "storage_unavailable",
            ),
        ] {
            let response = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                listener.clone().oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .header("authorization", format!("Bearer {bearer}"))
                        .header("origin", origin)
                        .header("x-request-id", "auth-correlation")
                        .body(Body::empty())
                        .unwrap(),
                ),
            )
            .await
            .expect("authentication errors must not wait for a diagnostic write")
            .unwrap();
            assert_eq!(response.status().as_u16(), status);
            assert_eq!(response.headers()["x-request-id"], "auth-correlation");
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let error: piqueld_core::api::ErrorBody = serde_json::from_slice(&body).unwrap();
            assert_eq!(error.code, code);
            assert_eq!(error.request_id, "auth-correlation");
            if status == 503 {
                let id = error.details["diagnostic_id"].as_str().unwrap();
                assert!(id.starts_with("diagnostic-"));
                assert!(matches!(
                    store.diagnostic(id).await,
                    Err(piqueld::store::StoreError::NotFound)
                ));
            } else {
                assert!(error.details.get("diagnostic_id").is_none());
            }
        }
    }
    transaction.rollback().await.unwrap();
}

/// Secret lifecycle history names secrets and journals cleanup without values.
async fn assert_secret_history(client: &Client, application_id: &str) {
    let history = client
        .events(None, Some(application_id), None, 100)
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
    let removal = history
        .iter()
        .find(|event| {
            event.kind == "action_succeeded" && event.phase.as_deref() == Some("remove_secrets")
        })
        .expect("runtime cleanup is journaled");
    assert_eq!(removal.resource.as_deref(), Some("token"));
    assert!(removal.operation_id.is_none());
    assert!(
        !serde_json::to_string(&history)
            .unwrap()
            .contains("private-token-value")
    );
}

#[tokio::test]
async fn secret_api_is_application_scoped_write_only_and_versioned() {
    use piqueld_client::edit::{ApplicationEdit, EditOptions, ServiceEdit};
    let temp = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let api = router(state(&temp).await, FakeAuth);
    let server = tokio::spawn(serve(listener, api.clone()).into_future());
    let client = Client::tcp(&format!("http://{address}/")).unwrap();
    let app = create_and_inspect(&client, &manifest()).await;
    let response = api
        .oneshot(
            Request::builder()
                .method("PUT")
                .uri(format!(
                    "/api/v1/environments/{}/secrets/token",
                    app.environment_id
                ))
                .header("host", "localhost")
                .header("content-type", "application/octet-stream")
                .header("x-expected-generation", "0")
                .body(Body::from("private-token-value"))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let raw = std::str::from_utf8(&bytes).unwrap();
    assert!(!raw.contains("private-token-value"));
    let body: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["data"]["generation"], 1);
    assert_eq!(client.secrets(&app.environment_id).await.unwrap().len(), 1);
    let reference = |secrets| ApplicationEdit::Service {
        name: "web".into(),
        edit: ServiceEdit::Secrets(secrets),
    };
    let mount = piqueld_client::SecretMount {
        name: "token".into(),
        target: "/run/secrets/token".into(),
    };
    let saved = client
        .edit_application(
            &app.environment_id,
            &reference(vec![mount.clone()]),
            &EditOptions {
                expected_generation: Some(app.generation),
                ..EditOptions::default()
            },
        )
        .await
        .unwrap();
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
    assert!(matches!(
        client.delete_secret(&app.environment_id, "token", 1).await,
        Err(piqueld_client::ClientError::Api { status, .. }) if status.as_u16() == 409
    ));
    client
        .edit_application(
            &app.environment_id,
            &reference(Vec::new()),
            &EditOptions {
                expected_generation: Some(saved.generation),
                ..EditOptions::default()
            },
        )
        .await
        .unwrap();
    assert!(
        matches!(client.put_secret(&app.environment_id,"token",0,b"stale".to_vec()).await.unwrap_err(),piqueld_client::ClientError::Api{status,..} if status.as_u16()==409)
    );
    assert!(
        matches!(client.secrets("app-absent").await.unwrap_err(),piqueld_client::ClientError::Api{status,..} if status.as_u16()==404)
    );
    client
        .delete_secret(&app.environment_id, "token", 1)
        .await
        .unwrap();
    assert!(
        client
            .secrets(&app.environment_id)
            .await
            .unwrap()
            .is_empty()
    );
    assert_secret_history(&client, &app.environment_id).await;
    server.abort();
}

#[tokio::test]
async fn missing_secret_key_is_a_persisted_daemon_diagnostic() {
    let temp = tempfile::tempdir().unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(serve(listener, router(state(&temp).await, FakeAuth)).into_future());
    let client = Client::tcp(&format!("http://{address}/")).unwrap();
    let app = create_and_inspect(&client, &manifest()).await;
    client
        .put_secret(&app.environment_id, "token", 0, b"value".to_vec())
        .await
        .unwrap();
    std::fs::remove_file(temp.path().join("secrets.key")).unwrap();
    let error = client
        .put_secret(&app.environment_id, "token", 1, b"replacement".to_vec())
        .await
        .unwrap_err();
    let piqueld_client::ClientError::Api { status, error, .. } = error else {
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
    server.abort();
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
        instance: InstanceId::parse(store.instance_id()).unwrap(),
        unavailable: std::sync::atomic::AtomicBool::new(true),
    });
    let service = ApiState::new(store.clone(), runtime.clone());
    service
        .put_secret(Daemon, &environment, "token", 0, b"value".to_vec())
        .await
        .unwrap();
    let cleanup = {
        let service = service.clone();
        let id = environment.clone();
        tokio::spawn(async move { service.delete_secret(Daemon, &id, "token", 1).await })
    };
    tokio::time::timeout(std::time::Duration::from_secs(2), gate.started.notified())
        .await
        .unwrap();
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        service.put_secret(Daemon, &environment, "other", 0, b"unrelated".to_vec()),
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
    runtime
        .unavailable
        .store(false, std::sync::atomic::Ordering::Relaxed);
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
        .put_secret(
            Daemon,
            &piqueld_core::EnvironmentId::default_for(application.id()),
            "token",
            0,
            b"private-value".to_vec(),
        )
        .await
        .unwrap();
    let api = router(
        ApiState::new(
            store.clone(),
            Arc::new(FakeRuntime {
                cleanup_gate: None,
                instance: InstanceId::parse(store.instance_id()).unwrap(),
                unavailable: std::sync::atomic::AtomicBool::new(false),
            }),
        ),
        FakeAuth,
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(serve(listener, api).into_future());
    let client = Client::tcp(&format!("http://{address}")).unwrap();
    assert!(
        matches!(client.recover_secret_key().await, Err(piqueld_client::ClientError::Api { status, .. }) if status.as_u16() == 409)
    );
    assert!(!client.secrets(application.id().as_str()).await.unwrap()[0].unavailable);

    std::fs::remove_file(temp.path().join("secrets.key")).unwrap();
    let recovery = client.recover_secret_key().await.unwrap();
    assert_eq!(recovery.discarded_versions, 1);
    assert!(client.secrets(application.id().as_str()).await.unwrap()[0].unavailable);
    client
        .put_secret(application.id().as_str(), "token", 1, b"new-value".to_vec())
        .await
        .unwrap();
    assert!(!client.secrets(application.id().as_str()).await.unwrap()[0].unavailable);
    server.abort();
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
        (Method::PUT, "/api/v1/environments/app-test/secrets/%FF"),
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
        let mut request = request(path);
        *request.method_mut() = method;
        let response = application.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(response.headers()["content-type"], "application/json");
        let request_id = response.headers()["x-request-id"]
            .to_str()
            .unwrap()
            .to_owned();
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let error: piqueld_core::api::ErrorBody = serde_json::from_slice(&body).unwrap();
        assert_eq!(error.code, "path_invalid");
        assert_eq!(error.request_id, request_id);
    }
}

/// Saves the fixture application and adds a `staging` environment. The
/// returned generation includes the bump from creating `staging`.
async fn two_environments(
    service: &ApiState,
) -> (
    piqueld_core::api::SavedApplication,
    piqueld_core::api::EnvironmentView,
) {
    use piqueld::api::{ApplicationError, Mutation, MutationResponse};
    use piqueld::store::StoreError;
    let MutationResponse::Saved(mut saved) = service
        .accept(
            Daemon,
            Mutation::save(manifest().validate_template().unwrap(), None, false),
            Some(0),
            false,
            None,
        )
        .await
        .unwrap()
    else {
        panic!("saved")
    };
    let application = piqueld_core::ApplicationId::parse(&saved.application_id).unwrap();
    let create = |name: &str| Mutation::CreateEnvironment {
        application: application.clone(),
        name: piqueld_core::EnvironmentName::parse(name).unwrap(),
        branch: None,
    };
    let MutationResponse::Environment(staging) = service
        .accept(
            Daemon,
            create("staging"),
            Some(saved.generation),
            false,
            None,
        )
        .await
        .unwrap()
    else {
        panic!("environment")
    };
    assert_ne!(staging.id.as_str(), application.as_str());
    saved.generation += 1;
    assert!(matches!(
        service
            .accept(
                Daemon,
                create("staging"),
                Some(saved.generation),
                false,
                None
            )
            .await,
        Err(ApplicationError::Store(StoreError::AlreadyExists))
    ));
    (saved, staging)
}

#[tokio::test]
async fn environments_deploy_independently_and_runtime_commands_never_pick_one() {
    use piqueld::api::{ApplicationError, Mutation, MutationResponse};
    use piqueld::store::StoreError;
    use piqueld_core::EnvironmentName;
    let temp = tempfile::tempdir().unwrap();
    let service = state(&temp).await;
    let (saved, staging) = two_environments(&service).await;
    let application = piqueld_core::ApplicationId::parse(&saved.application_id).unwrap();
    let view = service.application(&application).await.unwrap();
    assert_eq!(view.generation, saved.generation);
    let production = view.environment("production").unwrap().id.clone();
    assert_eq!(production.as_str(), application.as_str());
    assert_eq!(view.sole_environment().unwrap_err().len(), 2);

    // Saving with a deployment never picks one of several environments.
    assert!(matches!(
        service
            .accept(Daemon,
                Mutation::save(
                    manifest().validate_template().unwrap(),
                    Some(saved.application_id.clone()),
                    true,
                ),
                Some(saved.generation),
                false,
                None,
            )
            .await,
        Err(ApplicationError::Store(StoreError::EnvironmentRequired { environments }))
            if environments.len() == 2
    ));

    let MutationResponse::Operation(deployed) = service
        .accept(
            Daemon,
            Mutation::deploy(staging.id.clone()),
            Some(saved.generation),
            false,
            None,
        )
        .await
        .unwrap()
    else {
        panic!("deployment")
    };
    assert_eq!(deployed.environment_id, staging.id.as_str());
    assert!(
        service
            .environment_detail(&production)
            .await
            .unwrap()
            .latest_operation
            .is_none()
    );
    assert_eq!(
        service
            .environment_detail(&staging.id)
            .await
            .unwrap()
            .latest_operation
            .unwrap()
            .id,
        deployed.operation_id
    );

    // Deleting the application names every environment.
    let delete = |names: &[&str]| Mutation::DeleteApplication {
        id: application.clone(),
        environments: names
            .iter()
            .map(|name| EnvironmentName::parse(*name).unwrap())
            .collect(),
    };
    assert!(matches!(
        service
            .accept(
                Daemon,
                delete(&["production"]),
                Some(saved.generation),
                false,
                None
            )
            .await,
        Err(ApplicationError::Store(
            StoreError::ConfirmationRequired { .. }
        ))
    ));
    let MutationResponse::Deleted(deleted) = service
        .accept(
            Daemon,
            delete(&["staging", "production"]),
            Some(saved.generation),
            false,
            None,
        )
        .await
        .unwrap()
    else {
        panic!("deleted")
    };
    assert_eq!(deleted.operations.len(), 2);
    assert_eq!(deleted.generation, saved.generation + 1);
}

#[tokio::test]
async fn application_history_spans_its_environments() {
    use piqueld::api::Mutation;
    use piqueld_core::observability::EventFilter;
    let temp = tempfile::tempdir().unwrap();
    let service = state(&temp).await;
    let (saved, staging) = two_environments(&service).await;
    let application = piqueld_core::ApplicationId::parse(&saved.application_id).unwrap();
    let production = piqueld_core::EnvironmentId::default_for(&application);
    service
        .accept(
            Daemon,
            Mutation::Rename {
                id: application.clone(),
                name: "renamed".into(),
            },
            Some(saved.generation),
            false,
            None,
        )
        .await
        .unwrap();
    service
        .accept(
            Daemon,
            Mutation::deploy(production.clone()),
            Some(saved.generation + 1),
            false,
            None,
        )
        .await
        .unwrap();
    let events = |filter: EventFilter| {
        let service = service.clone();
        async move {
            service
                .filtered_events(&filter, &piqueld::store::Visibility::ALL, None, 100)
                .await
                .unwrap()
                .items
                .into_iter()
                .map(|event| (event.kind, event.environment_id))
                .collect::<Vec<_>>()
        }
    };

    // Application-wide events are recorded once, without an environment, and
    // the application's history includes every environment's events.
    let history = events(EventFilter {
        application_id: Some(application.to_string()),
        ..EventFilter::default()
    })
    .await;
    assert!(history.contains(&("environment_created".into(), Some(staging.id.clone()))));
    assert_eq!(
        history
            .iter()
            .filter(|(kind, _)| kind == "application_renamed")
            .collect::<Vec<_>>(),
        [&("application_renamed".to_owned(), None)]
    );
    let production_history = events(EventFilter {
        environment_id: Some(production.to_string()),
        ..EventFilter::default()
    })
    .await;
    let in_production = |(_, environment): &(String, Option<piqueld_core::EnvironmentId>)| {
        environment.as_ref() == Some(&production)
    };
    assert!(production_history.iter().any(in_production));
    assert!(production_history.iter().all(in_production));
}

#[tokio::test]
async fn previews_compare_with_the_selected_environment() {
    use piqueld::api::{ApplicationError, Mutation};
    use piqueld::store::StoreError;
    let temp = tempfile::tempdir().unwrap();
    let service = state(&temp).await;
    let (saved, staging) = two_environments(&service).await;
    let production = piqueld_core::EnvironmentId::parse(&saved.application_id).unwrap();
    service
        .accept(
            Daemon,
            Mutation::deploy(staging.id.clone()),
            Some(saved.generation),
            false,
            None,
        )
        .await
        .unwrap();
    let plan = |environment: Option<piqueld_core::EnvironmentId>| {
        let service = service.clone();
        async move {
            service
                .plan(
                    &piqueld_core::access::Grants::admin(),
                    manifest().validate_template().unwrap(),
                    None,
                    None,
                    environment.as_ref(),
                )
                .await
        }
    };

    // Without a selection, several environments leave no runtime baseline.
    let unselected = plan(None).await.unwrap();
    assert!(unselected.operation.is_none() && unselected.plan.actions.is_empty());

    let deployed = plan(Some(staging.id.clone())).await.unwrap();
    assert!(deployed.identical);
    assert!(deployed.operation.is_some());
    let resolves_image = |plan: &piqueld_core::api::PlanView| {
        plan.plan
            .actions
            .iter()
            .any(|action| matches!(action.kind, ActionKind::ResolveImage { .. }))
    };
    assert!(resolves_image(&deployed));

    let undeployed = plan(Some(production)).await.unwrap();
    assert!(!undeployed.identical && undeployed.operation.is_none());
    assert!(!undeployed.changes.is_empty() && resolves_image(&undeployed));

    // An environment of another application is never used as a baseline.
    let mut other = manifest();
    other.metadata.name = "other".into();
    service
        .accept(
            Daemon,
            Mutation::save(other.validate_template().unwrap(), None, false),
            Some(0),
            false,
            None,
        )
        .await
        .unwrap();
    let foreign = service
        .applications(&piqueld_core::access::Scope::All, None, None)
        .await
        .unwrap()
        .items;
    let foreign = foreign
        .iter()
        .find(|application| application.name == "other")
        .unwrap()
        .environments[0]
        .id
        .clone();
    assert!(matches!(
        plan(Some(foreign)).await,
        Err(ApplicationError::Store(StoreError::NotFound))
    ));
}

#[tokio::test]
async fn previews_render_variables_for_the_selected_environment() {
    use piqueld::api::ApplicationError;
    use piqueld::store::StoreError;
    use piqueld_core::manifest::{EnvironmentConfig, Variable, VariableValue};
    let temp = tempfile::tempdir().unwrap();
    let service = state(&temp).await;
    let (saved, staging) = two_environments(&service).await;
    let production = piqueld_core::EnvironmentId::parse(&saved.application_id).unwrap();
    let level = |value: &str| EnvironmentConfig {
        variables: [("level".into(), Variable::String(value.into()))].into(),
    };
    let mut manifest = manifest();
    manifest.spec.services[0]
        .environment
        .insert("LEVEL".into(), "${{ vars.level }}".into());
    manifest.spec.environments.extend([
        ("staging".into(), level("debug")),
        ("preview".into(), level("trace")),
    ]);
    let plan = |environment: piqueld_core::EnvironmentId| {
        let (service, manifest) = (service.clone(), manifest.clone());
        async move {
            service
                .plan(
                    &piqueld_core::access::Grants::admin(),
                    manifest.validate_template().unwrap(),
                    None,
                    None,
                    Some(&environment),
                )
                .await
        }
    };

    let rendered = plan(staging.id.clone()).await.unwrap();
    assert_eq!(
        rendered.variables["vars.level"],
        VariableValue::String("debug".into())
    );
    assert_eq!(
        rendered.variables["env.name"],
        VariableValue::String("staging".into())
    );
    // A block for an environment that does not exist yet is only a warning.
    let unknown = rendered
        .plan
        .diagnostics
        .iter()
        .find(|diagnostic| diagnostic.code == piqueld_core::codes::ENVIRONMENT_BLOCK_UNKNOWN)
        .unwrap();
    assert_eq!(unknown.resource, "spec.environments.preview");
    assert!(!rendered.plan.is_blocked());

    // Production has no value for `level`.
    let Err(ApplicationError::Store(StoreError::Validation(errors))) = plan(production).await
    else {
        panic!("missing value expected")
    };
    assert_eq!(
        (errors.0[0].code.as_str(), errors.0[0].path.as_str()),
        (
            piqueld_core::codes::VARIABLE_VALUE_MISSING,
            "spec.services[0].environment.LEVEL"
        )
    );
}

/// Login starts echo the grants they will be limited to, so clients can tell
/// the limit applies, and are kept in memory, so large bodies are refused.
#[tokio::test]
async fn device_starts_echo_their_limits_and_refuse_large_bodies() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp).await;
    let store = Store::open(temp.path().join("state.db")).await.unwrap();
    let auth = piqueld::auth::Auth::new(&store, "https://piqueld.example").unwrap();
    let router = api_router(state, auth);
    let start = |body: String| {
        let request = Request::builder()
            .method("POST")
            .uri("/api/v1/auth/device/start")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        router.clone().oneshot(request)
    };
    let limited = serde_json::json!({"grants": [{"permission": "apps:read"}]});
    let response = start(limited.to_string()).await.unwrap();
    assert_eq!(response.status(), 200);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let started: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(started["grants"], limited["grants"]);
    let large = format!(r#"{{"grants":[],"padding":"{}"}}"#, "x".repeat(16 * 1024));
    let response = start(large).await.unwrap();
    assert_eq!(response.status(), 413);
    // The limit applies while reading, also without a `Content-Length`.
    let chunks = (0..64).map(|_| Ok::<_, std::io::Error>(vec![b' '; 1024]));
    let streamed = Request::builder()
        .method("POST")
        .uri("/api/v1/auth/device/start")
        .header("content-type", "application/json")
        .body(Body::from_stream(futures_util::stream::iter(chunks)))
        .unwrap();
    let response = router.clone().oneshot(streamed).await.unwrap();
    assert_eq!(response.status(), 413);
}
