//! Focused API/client coverage for the polling application lifecycle.

#[path = "support/poll.rs"]
mod poll;
#[path = "support/repository.rs"]
mod repository;
#[path = "support/sqlite.rs"]
mod sqlite;

use async_trait::async_trait;
use axum::{body::Body, serve};
use futures_util::StreamExt as _;
use http::{HeaderMap, Method, Request, StatusCode};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper_util::rt::TokioIo;
use piqueld::api::Actor::Daemon;
use piqueld::api::http::{
    ApiState, Authenticator, EmbeddedBundle, UiAssets, api_router, router, web_router,
};
use piqueld::application::{BoundaryError, RuntimeBoundary};
use piqueld::store::{Store, StoredEnvironment};
use piqueld_client::{
    AcceptedOperation, ApplyApplicationRequest, ApplyOptions, Client, ClientError,
};
use piqueld_core::api::SavedApplication;
use piqueld_core::{
    InstanceId, NormalizedApplication, ObservedApplication, ResolutionSet, compile_application,
    manifest::{ApplicationManifest, ApplicationTemplate, Source, ValidatedSource},
    planner::ActionKind,
    resource::{ResolvedSource, image_repository},
};
use poll::eventually;
use repository::Repository;
use std::{
    collections::BTreeMap,
    future::IntoFuture,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};
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
            credential: piqueld::auth::Credential::Account {
                user: piqueld_core::auth::User {
                    id: "contract-admin".into(),
                    username: "admin".into(),
                    display_name: String::new(),
                },
                id: "contract-admin".into(),
                kind: piqueld::store::CredentialKind::Token,
            },
            grants: piqueld_core::access::Grants::admin(),
            scoped: false,
            tailnet: None,
        }))
    }
}

/// The bearer token [`seed_account`] gives the administrator [`FakeAuth`]
/// signs in as.
fn admin_token() -> String {
    format!("{:x<43}", "contract-admin")
}

/// Holds secret removal once it starts, until released.
#[derive(Default)]
struct CleanupGate {
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

struct FakeRuntime {
    cleanup_gate: Option<Arc<CleanupGate>>,
    instance: InstanceId,
    unavailable: AtomicBool,
}

impl FakeRuntime {
    /// A reachable runtime owned by `store`'s installation.
    fn new(store: &Store) -> Self {
        Self {
            cleanup_gate: None,
            instance: store.instance_id().clone(),
            unavailable: AtomicBool::new(false),
        }
    }

    /// Makes Docker unreachable, or reachable again.
    fn set_unavailable(&self, unavailable: bool) {
        self.unavailable.store(unavailable, Ordering::Relaxed);
    }
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

    /// Only `web` has a running task. A command named `block` never reads
    /// its input, one named `hang` echoes it but never stops or exits, and one named
    /// `slow` takes two seconds to create.
    async fn create_exec(
        &self,
        _environment: &piqueld_core::EnvironmentId,
        request: &piqueld_core::exec::ExecRequest,
    ) -> Result<Option<piqueld::docker::Exec>, BoundaryError> {
        if request.command.as_slice()[0] == "slow" {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        let id = match request.command.as_slice()[0].as_str() {
            "block" => "exec-blocked",
            "hang" => "exec-hang",
            _ => "exec-1",
        };
        Ok(
            (request.service.as_str() == "web").then(|| piqueld::docker::Exec {
                id: id.into(),
                task: "task-1".into(),
                tty: request.tty.is_some(),
            }),
        )
    }

    /// Echoes standard input, then reports how many bytes it read as the exit
    /// code, until the transport stops it.
    async fn run_exec(
        &self,
        exec: &piqueld::docker::Exec,
        io: piqueld::docker::ExecIo,
    ) -> Result<i64, BoundaryError> {
        use piqueld_core::exec::{ExecInput, ExecOutput};
        let piqueld::docker::ExecIo {
            mut input,
            output,
            stop,
        } = io;
        let echo = async {
            if exec.id == "exec-blocked" {
                return std::future::pending().await;
            }
            let mut read = 0;
            while let Some(ExecInput::Stdin(data)) = input.recv().await {
                read += data.len();
                output.send(ExecOutput::Stdout(data)).await.unwrap();
            }
            if exec.id == "exec-hang" {
                return std::future::pending().await;
            }
            output
                .send(ExecOutput::Stderr(b"closed".to_vec()))
                .await
                .unwrap();
            Ok(i64::try_from(read).unwrap())
        };
        let stopped = async {
            if exec.id == "exec-hang" {
                std::future::pending::<()>().await;
            }
            stop.cancelled().await;
        };
        tokio::select! {
            result = echo => result,
            () = stopped => Err(piqueld::docker::DockerError::Request("stopped").into()),
        }
    }

    fn trigger_reconciliation(&self) {}

    async fn readiness(&self) -> (bool, bool) {
        (true, !self.unavailable.load(Ordering::Relaxed))
    }

    /// Nothing else mutates the fake runtime.
    async fn lock_mutations(&self) -> tokio::sync::OwnedMutexGuard<()> {
        Arc::new(tokio::sync::Mutex::new(())).lock_owned().await
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

    /// Image availability is unknown: no engine.
    async fn local_images(&self) -> Result<piqueld_core::LocalImages, BoundaryError> {
        Err(piqueld::docker::DockerError::Unavailable("list images").into())
    }

    async fn reuse_images(
        &self,
        _: &BTreeMap<piqueld_core::ServiceName, piqueld_core::ResolvedSource>,
    ) -> Result<piqueld::reconcile::ImagesInUse, BoundaryError> {
        Err(piqueld::docker::DockerError::Unavailable("list images").into())
    }

    async fn check_available(&self) -> Result<(), BoundaryError> {
        if self.unavailable.load(Ordering::Relaxed) {
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
    /// The fake runtime runs no containers.
    async fn containers(&self) -> Result<Vec<piqueld::docker::ManagedContainer>, BoundaryError> {
        Ok(Vec::new())
    }
    async fn container_usage(
        &self,
        _: &str,
    ) -> Result<Option<piqueld::docker::ContainerUsage>, BoundaryError> {
        Ok(None)
    }
    fn container_logs(&self, _: &str, _: i32) -> piqueld::docker::ContainerLogs {
        futures_util::stream::empty().boxed()
    }
    /// No container ever starts.
    fn container_starts(&self, _: i64) -> piqueld::docker::ContainerStarts {
        futures_util::stream::pending().boxed()
    }
}

/// The contract manifest: `notes`, with one `web` service running an image.
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

/// Applies the contract manifest as a new application.
fn new_application() -> ApplyApplicationRequest {
    ApplyApplicationRequest {
        manifest: manifest(),
        expected_generation: Some(0),
        expected_application_id: None,
    }
}

/// The contract daemon's database in `temp`.
fn database(temp: &TempDir) -> PathBuf {
    temp.path().join("state.db")
}

/// The contract daemon's state in `temp`, over [`FakeRuntime`].
async fn state(temp: &TempDir) -> ApiState {
    state_with(temp, None).await
}

/// [`state`] with the daemon's `[snapshots] max_upload_bytes`, if given.
async fn state_with(temp: &TempDir, max_upload_bytes: Option<std::num::NonZeroU64>) -> ApiState {
    let store = Store::open(database(temp))
        .await
        .expect("fresh database opens");
    let store = match max_upload_bytes {
        Some(limit) => store.with_snapshot_uploads(limit),
        None => store,
    };
    contract_state(temp, Arc::new(store)).await.1
}

/// The runtime and state of a contract daemon over `store`, opened in
/// `temp`.
async fn contract_state(temp: &TempDir, store: Arc<Store>) -> (Arc<FakeRuntime>, ApiState) {
    // `FakeAuth` signs requests in with this administrator's credential,
    // which mutations re-read from the database.
    seed_account(&database(temp), "contract-admin", &[("admin", None)]).await;
    let runtime = Arc::new(FakeRuntime::new(&store));
    (Arc::clone(&runtime), ApiState::new(store, runtime))
}

/// The real authenticator of the contract daemon in `temp`.
async fn auth(temp: &TempDir) -> piqueld::auth::Auth {
    let store = Store::open(database(temp)).await.unwrap();
    piqueld::auth::Auth::new(&store, "https://piqueld.example").unwrap()
}

/// Saves `manifest` as a new application, without deploying it.
async fn save(state: &ApiState, manifest: ApplicationManifest) -> SavedApplication {
    let mutation = piqueld::api::Mutation::save(manifest.validate_template().unwrap(), None, false);
    let Ok(piqueld::api::MutationResponse::Saved(saved)) =
        state.accept(Daemon, mutation, Some(0), false, None).await
    else {
        panic!("save response");
    };
    saved
}

/// Seeds an account holding `grants` (permission, optional application ID)
/// with a non-expiring token, returning the token. The account and credential
/// share `name` as their ID; seeding an existing account again adds grants.
async fn seed_account(database: &Path, name: &str, grants: &[(&str, Option<&str>)]) -> String {
    use base64::Engine as _;
    use sha2::Digest as _;
    let token = format!("{name:x<43}");
    let hash = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(token.as_bytes()));
    let mut connection = sqlite::connect(database).await;
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

/// Runs `sql` against the database at `database`.
async fn execute(database: &Path, sql: &str) {
    sqlx::query(sql)
        .execute(&mut sqlite::connect(database).await)
        .await
        .unwrap();
}

/// The status and code of an API error response.
#[derive(Debug, PartialEq)]
struct ApiError {
    status: u16,
    code: String,
}

impl ApiError {
    fn new(status: u16, code: &str) -> Self {
        Self {
            status,
            code: code.to_owned(),
        }
    }
}

/// The status and code of `error`, which must be an API error response.
fn api_error(error: ClientError) -> ApiError {
    match error {
        ClientError::Api { status, error } => ApiError {
            status: status.as_u16(),
            code: error.code,
        },
        other => panic!("API error expected, got {other:?}"),
    }
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

/// A `GET` of `uri` from a local client.
fn request(uri: &str) -> Request<Body> {
    request_as(Method::GET, uri)
}

/// A bodiless `method` request of `uri` from a local client.
fn request_as(method: Method, uri: &str) -> Request<Body> {
    Request::builder()
        .method(method)
        .header("host", "localhost")
        .uri(uri)
        .body(Body::empty())
        .expect("request is valid")
}

/// A response read to its end.
struct Received {
    status: StatusCode,
    headers: HeaderMap,
    body: Bytes,
}

impl Received {
    async fn read<B>(response: http::Response<B>) -> Self
    where
        B: hyper::body::Body,
        B::Error: std::fmt::Debug,
    {
        let (parts, body) = response.into_parts();
        Self {
            status: parts.status,
            headers: parts.headers,
            body: body.collect().await.expect("body is readable").to_bytes(),
        }
    }

    fn text(&self) -> &str {
        std::str::from_utf8(&self.body).expect("response is UTF-8")
    }

    /// The JSON body, or null when there is none.
    fn json(&self) -> serde_json::Value {
        serde_json::from_slice(&self.body).unwrap_or_default()
    }

    fn header(&self, name: impl http::header::AsHeaderName) -> Option<&str> {
        self.headers
            .get(name)
            .map(|value| value.to_str().expect("header is text"))
    }

    /// The error code of the JSON body.
    fn code(&self) -> String {
        self.json()["code"].as_str().unwrap_or_default().to_owned()
    }

    fn assert_error(&self, status: StatusCode, code: &str) {
        assert_eq!(self.status, status);
        assert_eq!(self.code(), code);
    }

    /// Asserts the response's status and media type are documented for
    /// `method` `path` in `document`, and that errors are correlated bodies.
    fn assert_documented(&self, document: &serde_json::Value, method: &Method, path: &str) {
        let status = self.status.as_u16().to_string();
        let operation = &document["paths"][path][method.as_str().to_lowercase()];
        let response = &operation["responses"][&status];
        assert!(
            !response.is_null(),
            "undocumented {method} {path} -> {status}"
        );
        let media_type = self
            .header(http::header::CONTENT_TYPE)
            .unwrap()
            .split(';')
            .next()
            .unwrap();
        assert!(
            !response["content"][media_type].is_null(),
            "undocumented response media type for {method} {path}: {media_type}"
        );
        if self.status.is_client_error() || self.status.is_server_error() {
            let error: piqueld_core::api::ErrorBody = serde_json::from_slice(&self.body).unwrap();
            assert_ne!(error.code, "");
            assert_ne!(error.message, "");
            let header_id = self.header("x-request-id").expect("request ID header");
            assert_ne!(header_id, "");
            assert_eq!(error.request_id, header_id);
        }
    }
}

/// In-process requests to a router.
trait InProcess {
    /// Sends `request` and returns the response, whose body may stream.
    async fn open(&self, request: Request<Body>) -> axum::response::Response;

    /// Sends `request` and reads the whole response.
    async fn send(&self, request: Request<Body>) -> Received;

    /// Sends a JSON request as bearer `token`, returning the status and the
    /// JSON body.
    async fn call(
        &self,
        token: &str,
        method: &str,
        uri: &str,
        body: serde_json::Value,
    ) -> (u16, serde_json::Value);

    /// Sends a bodiless request as bearer `token`, returning the status.
    async fn status(&self, token: &str, method: &str, uri: &str) -> u16 {
        self.call(token, method, uri, serde_json::Value::Null)
            .await
            .0
    }
}

impl InProcess for axum::Router {
    async fn open(&self, request: Request<Body>) -> axum::response::Response {
        self.clone()
            .oneshot(request)
            .await
            .expect("router responds")
    }

    async fn send(&self, request: Request<Body>) -> Received {
        Received::read(self.open(request).await).await
    }

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
        let response = self.send(request).await;
        (response.status.as_u16(), response.json())
    }
}

/// Where a [`Server`] listens.
enum Endpoint {
    Tcp(std::net::SocketAddr),
    Unix(PathBuf),
}

/// A router served on a local socket until dropped.
struct Server {
    endpoint: Endpoint,
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Server {
    async fn tcp(router: axum::Router) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("TCP binds");
        let endpoint = Endpoint::Tcp(listener.local_addr().expect("TCP address"));
        let task = tokio::spawn(serve(listener, router).into_future());
        Self { endpoint, task }
    }

    /// [`Self::tcp`], telling handlers each client's address, as tailnet
    /// listeners do.
    async fn tcp_with_peers(router: axum::Router) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("TCP binds");
        let endpoint = Endpoint::Tcp(listener.local_addr().expect("TCP address"));
        let service = router.into_make_service_with_connect_info::<std::net::SocketAddr>();
        let task = tokio::spawn(serve(listener, service).into_future());
        Self { endpoint, task }
    }

    /// Serves `router` on a Unix socket at `path`, which must be short.
    fn unix(router: axum::Router, path: &Path) -> Self {
        let listener = tokio::net::UnixListener::bind(path).expect("Unix socket binds");
        let task = tokio::spawn(serve(listener, router).into_future());
        Self {
            endpoint: Endpoint::Unix(path.to_owned()),
            task,
        }
    }

    /// The TCP address served.
    fn address(&self) -> std::net::SocketAddr {
        match &self.endpoint {
            Endpoint::Tcp(address) => *address,
            Endpoint::Unix(path) => panic!("{} is a Unix socket", path.display()),
        }
    }

    fn client(&self) -> Client {
        match &self.endpoint {
            Endpoint::Tcp(address) => Client::tcp(&format!("http://{address}/")).unwrap(),
            Endpoint::Unix(path) => Client::unix(path),
        }
    }

    /// Sends one raw HTTP/1.1 request, from `localhost` unless `headers` name
    /// another host, and reads the whole response.
    async fn send(
        &self,
        method: Method,
        path: &str,
        headers: &[(&str, &str)],
        body: Vec<u8>,
    ) -> Received {
        let mut builder = Request::builder()
            .method(method)
            .uri(path)
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
        match &self.endpoint {
            Endpoint::Tcp(address) => {
                let stream = tokio::net::TcpStream::connect(address).await.unwrap();
                Self::exchange(stream, request).await
            }
            Endpoint::Unix(path) => {
                let stream = tokio::net::UnixStream::connect(path).await.unwrap();
                Self::exchange(stream, request).await
            }
        }
    }

    /// Sends `request` over a fresh HTTP/1.1 connection on `stream`.
    async fn exchange<S>(stream: S, request: Request<Full<Bytes>>) -> Received
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(stream))
            .await
            .expect("handshake");
        tokio::spawn(async move {
            let _ = connection.with_upgrades().await;
        });
        Received::read(sender.send_request(request).await.expect("response")).await
    }

    /// Asserts the server is still serving, rather than having failed.
    fn assert_running(&self) {
        assert!(!self.task.is_finished(), "the server stopped");
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
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
    let shell = application.send(request("/dashboard/")).await;
    assert_eq!(shell.status, StatusCode::OK);
    let csp = shell.header("content-security-policy").expect("CSP");
    let html = shell.text();
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
        let asset = application
            .send(request(&format!("/dashboard/{name}")))
            .await;
        assert_eq!(asset.status, StatusCode::OK, "{name}");
        assert_eq!(asset.body.as_ref(), *expected, "{name}");
    }
}

async fn assert_dashboard_routes(application: &axum::Router) {
    for uri in ["/", "/dashboard"] {
        let redirect = application.send(request(uri)).await;
        assert_eq!(redirect.status, StatusCode::PERMANENT_REDIRECT, "{uri}");
        assert_eq!(
            redirect.header(http::header::LOCATION),
            Some("/dashboard/"),
            "{uri}"
        );
    }
    for uri in ["/dashboard/", "/dashboard/applications/notes"] {
        let shell = application.send(request(uri)).await;
        assert_eq!(shell.status, StatusCode::OK, "{uri}");
        assert!(shell.text().contains("dashboard-shell"), "{uri}");
    }
    let asset = application.send(request("/dashboard/app.js")).await;
    assert_eq!(asset.status, StatusCode::OK);
    assert!(asset.text().contains("console.log"));
    let hashed = application
        .send(request("/dashboard/piqueld-ui-a1b2c3d4_bg.wasm"))
        .await;
    assert_eq!(hashed.status, StatusCode::OK);
    assert!(hashed.body.starts_with(b"\0"));
    for uri in ["/dashboard/missing.js", "/api/v1/unknown", "/unknown"] {
        let missing = application.send(request(uri)).await;
        assert_eq!(missing.status, StatusCode::NOT_FOUND, "{uri}");
        assert!(!missing.text().contains("dashboard-shell"), "{uri}");
        if uri.starts_with("/api/") {
            assert!(missing.text().contains("endpoint_not_found"), "{uri}");
        }
    }
}

async fn assert_api_routes(application: &axum::Router) {
    let health = application.send(request("/health")).await;
    assert_eq!(health.status, StatusCode::OK);
    assert_eq!(health.text(), r#"{"status":"ok"}"#);

    let openapi = application.send(request("/api/v1/openapi.json")).await;
    assert_eq!(openapi.status, StatusCode::OK);
    let openapi = openapi.json();
    assert_eq!(openapi["openapi"], "3.0.3");
    assert!(openapi["paths"]["/api/v1/environments/{id}/detail"].is_object());
}

async fn assert_api_only_and_ui_modes(temp: &TempDir) {
    let api_only = api_router(state(temp).await, FakeAuth);
    let root = api_only.send(request("/")).await;
    assert_eq!(root.status, StatusCode::NOT_FOUND);
    assert!(!root.text().contains("endpoint_not_found"));
    let health = api_only.send(request("/health")).await;
    assert_eq!(health.status, StatusCode::NOT_FOUND);
    let unknown = api_only.send(request("/api/v1/unknown")).await;
    assert_eq!(unknown.status, StatusCode::NOT_FOUND);
    assert!(unknown.text().contains("endpoint_not_found"));

    let disabled = web_router(state(temp).await, UiAssets::Disabled, FakeAuth);
    for uri in ["/", "/dashboard/"] {
        let response = disabled.send(request(uri)).await;
        assert_eq!(response.status, StatusCode::NOT_FOUND, "{uri}");
    }
}

#[tokio::test]
async fn dashboard_responses_carry_security_headers() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let application = web_router(
        state(&temp).await,
        UiAssets::Embedded(TEST_BUNDLE),
        FakeAuth,
    );
    let shell = application.send(request("/dashboard/")).await;
    assert_eq!(shell.status, StatusCode::OK);
    let csp = shell
        .header("content-security-policy")
        .expect("dashboard carries a content security policy");
    assert!(csp.contains("default-src 'self'"), "{csp}");
    assert!(csp.contains("wasm-unsafe-eval"), "{csp}");
    assert!(csp.contains("frame-ancestors 'none'"), "{csp}");
    assert_eq!(shell.header("x-content-type-options"), Some("nosniff"));
    assert_eq!(shell.header("referrer-policy"), Some("no-referrer"));

    let head = application
        .send(request_as(Method::HEAD, "/dashboard/"))
        .await;
    assert_eq!(head.status, StatusCode::OK);
    assert!(head.body.is_empty());

    let post = application
        .send(request_as(Method::POST, "/dashboard/"))
        .await;
    assert_eq!(post.status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(post.header(http::header::ALLOW), Some("GET, HEAD"));
    for (method, path, status) in [
        (Method::GET, "/", StatusCode::PERMANENT_REDIRECT),
        (Method::GET, "/dashboard/missing.js", StatusCode::NOT_FOUND),
        (Method::POST, "/dashboard", StatusCode::METHOD_NOT_ALLOWED),
    ] {
        let response = application.send(request_as(method, path)).await;
        assert_eq!(response.status, status);
        for name in [
            "content-security-policy",
            "x-content-type-options",
            "referrer-policy",
        ] {
            assert_eq!(
                response.headers.get_all(name).iter().count(),
                1,
                "{path}: {name}"
            );
            assert_eq!(response.header(name), shell.header(name));
        }
    }
    for path in ["/health", "/api/v1/missing"] {
        let response = application.send(request(path)).await;
        assert!(
            !response.headers.contains_key("content-security-policy"),
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
        .send(request("/dashboard/piqueld-ui-a1b2c3d4_bg.wasm"))
        .await;
    assert_eq!(hashed.header("content-type"), Some("application/wasm"));
    assert_eq!(
        hashed.header("cache-control"),
        Some("public, max-age=31536000, immutable")
    );

    let unhashed = application.send(request("/dashboard/app.js")).await;
    assert_eq!(
        unhashed.header("content-type"),
        Some("text/javascript; charset=utf-8")
    );
    assert_eq!(unhashed.header("cache-control"), Some("no-cache"));

    // A short all-hex stem is an ordinary name, not a content digest.
    let hex_stemmed = application.send(request("/dashboard/added.css")).await;
    assert_eq!(hex_stemmed.header("cache-control"), Some("no-cache"));

    // The shell, also served for deep links, is never cached.
    for path in ["/dashboard/", "/dashboard/applications/notes"] {
        let shell = application.send(request(path)).await;
        assert_eq!(shell.header("cache-control"), Some("no-store"), "{path}");
    }
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
async fn served_openapi_document_matches_the_generated_snapshot_and_resolves_refs() {
    let temp = tempfile::tempdir().expect("temporary directory");
    let server = Server::tcp(router(state(&temp).await, FakeAuth)).await;

    let response = server
        .send(Method::GET, "/api/v1/openapi.json", &[], Vec::new())
        .await;
    assert_eq!(response.status, StatusCode::OK);
    let generated =
        serde_json::to_value(piqueld::api::http::openapi_document()).expect("document serializes");
    assert_eq!(response.json(), generated);
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
    collect_unresolved_refs(&generated, &generated, &mut unresolved);
    assert!(unresolved.is_empty(), "unresolved refs: {unresolved:?}");
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

/// Collects into `out` every `$ref` in `value` that resolves to nothing in
/// `document`.
fn collect_unresolved_refs(
    value: &serde_json::Value,
    document: &serde_json::Value,
    out: &mut Vec<String>,
) {
    match value {
        serde_json::Value::Object(map) => {
            if let Some(reference) = map.get("$ref").and_then(serde_json::Value::as_str) {
                let pointer = reference.strip_prefix("#").unwrap_or(reference);
                if document.pointer(pointer).is_none() {
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
    let temp = tempfile::tempdir().expect("temporary directory");
    let socket = temp.path().join("contract.sock");
    let server = Server::unix(router(state(&temp).await, FakeAuth), &socket);
    let client = server.client();

    let status = client.system_status().await.expect("status over unix");
    assert_eq!(status.api_version, "v1");
    assert_lifecycle(&client).await;
}

/// Environments refuse an invalid branch or commit like previews do, as a
/// malformed request field rather than a manifest validation failure.
#[tokio::test]
async fn environment_branches_are_validated_like_preview_branches() {
    let temp = tempfile::tempdir().unwrap();
    let server = Server::tcp(router(state(&temp).await, FakeAuth)).await;
    let saved = server
        .client()
        .apply_and_deploy(&new_application())
        .await
        .unwrap();
    let id = &saved.environment_id;
    let json = [("content-type", "application/json")];
    for (method, path, body, code) in [
        (
            Method::POST,
            format!("/api/v1/applications/{id}/environments"),
            r#"{"name":"staging","branch":"bad..branch","expected_generation":1}"#,
            "git_branch_invalid",
        ),
        (
            Method::PUT,
            format!("/api/v1/environments/{id}/branch"),
            r#"{"branch":"main","commit":"abc","expected_generation":1}"#,
            "git_commit_invalid",
        ),
    ] {
        let response = server.send(method, &path, &json, body.into()).await;
        response.assert_error(StatusCode::BAD_REQUEST, code);
    }
}

#[tokio::test]
async fn generations_deploy_reconcile_and_event_pagination_share_the_http_contract() {
    let temp = tempfile::tempdir().unwrap();
    let server = Server::tcp(router(state(&temp).await, FakeAuth)).await;
    let client = server.client();
    let mut request = new_application();
    let first = client.apply_and_deploy(&request).await.unwrap();
    assert_eq!(first.generation, 1);
    let stale = client.apply_and_deploy(&request).await.unwrap_err();
    assert_eq!(api_error(stale).status, 409);
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
        matches!(unbacked, ClientError::Api { error, .. } if error.details.to_string().contains("manifest_repository_required"))
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
}

/// The contract daemon served over TCP, with a client, until dropped.
struct AcceptanceApi {
    client: Client,
    runtime: Arc<FakeRuntime>,
    store: Arc<Store>,
    server: Server,
}

impl AcceptanceApi {
    async fn start(temp: &TempDir) -> Self {
        let store = Arc::new(Store::open(database(temp)).await.unwrap());
        let (runtime, state) = contract_state(temp, Arc::clone(&store)).await;
        let server = Server::tcp(router(state, FakeAuth)).await;
        Self {
            client: server.client(),
            runtime,
            store,
            server,
        }
    }

    /// Saves the contract manifest as a new application.
    async fn save(&self) -> SavedApplication {
        self.client
            .apply_application(&new_application())
            .await
            .unwrap()
    }

    /// Saves and deploys the contract manifest as a new application.
    async fn deploy(&self) -> AcceptedOperation {
        self.client
            .apply_and_deploy(&new_application())
            .await
            .unwrap()
    }

    /// The same daemon, routed in process.
    fn router(&self) -> axum::Router {
        router(
            ApiState::new(Arc::clone(&self.store), self.runtime.clone()),
            FakeAuth,
        )
    }

    /// Runs operation `id` to success, or to failure with `error`.
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

    /// Applies `edit` at the current generation, returning the saved manifest.
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
        self.manifest(id).await
    }

    /// Applies `edit` to `web`, returning the saved service.
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

    /// Application `id`'s saved manifest.
    async fn manifest(&self, id: &str) -> ApplicationManifest {
        self.client
            .application(id)
            .await
            .unwrap()
            .application
            .to_manifest()
    }
}

#[tokio::test]
async fn acceptance_receipts_survive_restart_and_supersession() {
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let request = new_application();
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
    assert_eq!(api_error(error).code, "request_id_conflict");
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
    let accepted = api.deploy().await;
    let request = piqueld_client::RenameApplicationRequest {
        name: "renamed".into(),
        expected_generation: Some(1),
    };
    let keyed = api.client.clone().with_request_id("rename-command");
    let error = keyed
        .rename_application(&accepted.environment_id, &request)
        .await
        .unwrap_err();
    assert_eq!(api_error(error).code, "application_busy");
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
    let mut identical = new_application();
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
    let old_name = api.deploy().await;
    let mut stale = new_application();
    stale.expected_generation = Some(1);
    stale.expected_application_id = Some(accepted.environment_id);
    stale.manifest.spec.services[0].replicas = 3.into();
    let error = api.client.apply_and_deploy(&stale).await.unwrap_err();
    assert_eq!(api_error(error).code, "identity_conflict");
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
    let temp = tempfile::tempdir().unwrap();
    let database = database(&temp);
    let api = AcceptanceApi::start(&temp).await;
    execute(&database, "CREATE TRIGGER reject_receipt BEFORE INSERT ON request_receipts BEGIN SELECT RAISE(ABORT,'injected receipt failure'); END").await;
    let keyed = api.client.clone().with_request_id("atomic-command");
    let request = new_application();
    assert!(keyed.apply_and_deploy(&request).await.is_err());
    assert_eq!(
        api.store.list(None, 50).await.unwrap().items,
        [] as [StoredEnvironment; 0]
    );
    execute(&database, "DROP TRIGGER reject_receipt").await;
    let accepted = keyed.apply_and_deploy(&request).await.unwrap();
    execute(&database, "UPDATE request_receipts SET expires_at_ms=0").await;
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
    let mut request = new_application();
    request.manifest.spec.services[0]
        .environment
        .insert("TOKEN".into(), "old-private-value".into());
    let normalized = request
        .manifest
        .clone()
        .validate()
        .unwrap()
        .normalize(piqueld_core::ApplicationId::parse("app-preview-01").unwrap());
    let target = api
        .runtime
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

#[tokio::test]
async fn mutations_require_preconditions_but_reconcile_uses_current_intent() {
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let mut request = new_application();
    request.expected_generation = None;
    assert_eq!(
        api_error(api.client.apply_and_deploy(&request).await.unwrap_err()).code,
        "precondition_required"
    );
    request.expected_generation = Some(0);
    let accepted = api.client.apply_and_deploy(&request).await.unwrap();
    // Creating it again finds its name taken, like a rename would.
    assert_eq!(
        api_error(api.client.apply_and_deploy(&request).await.unwrap_err()).code,
        "application_name_collision"
    );
    request.expected_generation = Some(1);
    assert_eq!(
        api_error(api.client.apply_and_deploy(&request).await.unwrap_err()).code,
        "precondition_required"
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
    assert_eq!(
        api_error(first.err().or(second.err()).unwrap()).code,
        "generation_conflict"
    );
    assert_eq!(
        api_error(
            api.client
                .delete_application_with_generation(&accepted.environment_id, None)
                .await
                .unwrap_err()
        )
        .code,
        "precondition_required"
    );
    assert_eq!(
        api_error(
            api.client
                .delete_application_with_generation(&accepted.environment_id, Some(1))
                .await
                .unwrap_err()
        )
        .code,
        "generation_conflict"
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
    let mut request = new_application();
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
    let replacement = api.deploy().await;
    assert_ne!(replacement.environment_id, original.environment_id);
    request.expected_generation = Some(1);
    request.expected_application_id = Some(original.environment_id);
    request.manifest.spec.services[0].replicas = 4.into();
    assert_eq!(
        api_error(api.client.apply_and_deploy(&request).await.unwrap_err()).code,
        "identity_conflict"
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
    let request = new_application();
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
    assert_eq!(
        api_error(keyed.apply_and_deploy(&request).await.unwrap_err()).code,
        "request_id_conflict"
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
    let accepted = api.deploy().await;
    let mut rename = piqueld_client::RenameApplicationRequest {
        name: "renamed".into(),
        expected_generation: None,
    };
    assert_eq!(
        api_error(
            api.client
                .rename_application(&accepted.environment_id, &rename)
                .await
                .unwrap_err()
        )
        .code,
        "precondition_required"
    );
    assert_eq!(
        api_error(
            api.client
                .rename_application_with_force(&accepted.environment_id, &rename, true)
                .await
                .unwrap_err()
        )
        .code,
        "application_busy"
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
    api.deploy().await;
    rename.name = "notes".into();
    assert_eq!(
        api_error(
            api.client
                .rename_application_with_force(&accepted.environment_id, &rename, true)
                .await
                .unwrap_err()
        )
        .code,
        "application_name_collision"
    );
}

#[tokio::test]
async fn docker_outage_allows_acceptance_and_preserves_receipt_replay() {
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let request = new_application();
    let keyed = api.client.clone().with_request_id("outage-replay");
    let accepted = keyed.apply_and_deploy(&request).await.unwrap();
    api.runtime.set_unavailable(true);
    assert_eq!(
        keyed.apply_and_deploy(&request).await.unwrap().operation_id,
        accepted.operation_id
    );
    let mut changed = request.clone();
    changed.expected_generation = Some(accepted.generation);
    changed.expected_application_id = Some(accepted.environment_id.clone());
    changed.manifest.spec.services[0].replicas = 2.into();
    let unavailable = api.client.plan_application(&changed, None).await;
    assert_eq!(api_error(unavailable.unwrap_err()).status, 503);
    let next = api.client.apply_and_deploy(&changed).await.unwrap();
    assert_ne!(next.operation_id, accepted.operation_id);
    assert_eq!(
        api_error(keyed.apply_and_deploy(&changed).await.unwrap_err()).code,
        "request_id_conflict"
    );
}

// Existing lifecycle scenarios explicitly request deployment; save-only behavior
// is exercised separately below.
trait DeployFixture {
    async fn apply_and_deploy(
        &self,
        request: &ApplyApplicationRequest,
    ) -> Result<AcceptedOperation, ClientError>;
    async fn apply_and_deploy_with_force(
        &self,
        request: &ApplyApplicationRequest,
        force: bool,
    ) -> Result<AcceptedOperation, ClientError>;
}
impl DeployFixture for Client {
    async fn apply_and_deploy(
        &self,
        request: &ApplyApplicationRequest,
    ) -> Result<AcceptedOperation, ClientError> {
        self.apply_and_deploy_with_force(request, false).await
    }
    async fn apply_and_deploy_with_force(
        &self,
        request: &ApplyApplicationRequest,
        force: bool,
    ) -> Result<AcceptedOperation, ClientError> {
        let saved = self
            .apply_application_with_options(
                request,
                &ApplyOptions {
                    force,
                    deploy: true,
                },
            )
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
    let server = Server::tcp(router(state(&temp).await, FakeAuth)).await;
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
        let response = server
            .send(
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

/// Exec rechecks the caller's grants when the command starts, so access lost
/// after the connection was authorized cannot start one. History names who
/// started a command.
#[tokio::test]
async fn exec_rechecks_grants_when_the_command_starts() {
    use piqueld::store::{Caller, StoreError};
    use piqueld_core::access::Denied;
    let temp = tempfile::tempdir().unwrap();
    let state = state(&temp).await;
    let saved = save(&state, manifest()).await;
    let database = database(&temp);
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
    let session = state
        .exec(caller, &environment, &request, "operator")
        .await
        .unwrap();
    // The start is recorded once the command runs; without input it exits.
    let (_, input) = tokio::sync::mpsc::channel(1);
    let (output, _output) = tokio::sync::mpsc::channel(1);
    let io = piqueld::docker::ExecIo {
        input,
        output,
        stop: tokio_util::sync::CancellationToken::new(),
    };
    assert_eq!(session.run(io).await.unwrap(), 0);
    let filter = piqueld_core::observability::EventFilter {
        kind: Some("command_started".into()),
        ..Default::default()
    };
    let started = state
        .filtered_events(&filter, &piqueld::store::Visibility::ALL, None, 10)
        .await
        .unwrap()
        .items;
    assert_eq!(
        started[0].actor,
        Some(piqueld_core::EventActor::Account {
            user_id: "operator".into(),
            credential_id: Some("operator".into()),
        })
    );
    execute(
        &database,
        "DELETE FROM auth_grants WHERE user_id='operator'",
    )
    .await;
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
    let database = database(&temp);
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
    let unix = Server::unix(
        api_router(state.clone(), FakeAuth),
        &temp.path().join("exec.sock"),
    );
    let tcp = Server::tcp(router(state, FakeAuth)).await;
    let request = |service: &str| ExecRequest {
        service: service.parse().unwrap(),
        command: ExecCommand::parse(vec!["invite".into(), "create".into()]).unwrap(),
        stdin: true,
        tty: None,
    };
    let app = create_and_inspect(&unix.client(), &manifest()).await;
    for client in [unix.client(), tcp.client()] {
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
    let commands = unix
        .client()
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
    let url = format!(
        "ws://{}/api/v1/environments/{}/exec",
        tcp.address(),
        app.environment_id
    );
    let (mut socket, _) = connect_exec(tcp.address(), url, None).await;
    let start = serde_json::to_string(&request("web")).unwrap();
    socket.send(Message::text(start)).await.unwrap();
    socket.close(None).await.unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(5), socket.next()).await;
    assert!(
        matches!(reply, Ok(Some(Ok(Message::Close(_))))),
        "{reply:?}"
    );
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
    let service = ApplicationService::new(api.store.clone(), api.runtime.clone());
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
        run: JobRun::BeforeRollout,
        timeout_seconds: 60,
        volume_access: JobVolumeAccess::ReadOnly,
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

/// Creating an application under the name of a repository-managed one is a
/// name collision, not an edit refused for being repository-managed.
#[tokio::test]
async fn creating_over_a_repository_managed_name_collides() {
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
    // A runtime change, which the repository-managed check would refuse.
    let mut request = new_application();
    request.manifest.spec.services[0].replicas = 2.into();
    let error = api.client.apply_application(&request).await.unwrap_err();
    assert_eq!(api_error(error).code, "application_name_collision");
}

#[tokio::test]
async fn field_edits_validate_atomically_and_preserve_git_ownership() {
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
    let blocked = ApplicationEdit::Service {
        name: "web".into(),
        edit: ServiceEdit::Replicas(2.into()),
    };
    let error = api
        .client
        .edit_application(id, &blocked, &options)
        .await
        .unwrap_err();
    assert_eq!(api_error(error).code, "repository_managed");
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

#[tokio::test]
async fn event_stream_replays_after_cursor_and_rejects_pruned_history() {
    let temp = tempfile::tempdir().unwrap();
    let api = AcceptanceApi::start(&temp).await;
    let app = api.deploy().await;
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
    let router = api.router();
    for limit in [0, 101] {
        let response = router
            .send(request(&format!("/api/v1/events/stream?limit={limit}")))
            .await;
        assert_eq!(response.status, StatusCode::BAD_REQUEST);
    }
    let response = router
        .open(
            Request::builder()
                .uri("/api/v1/events/stream?errors_only=true&limit=1")
                .header("host", "localhost")
                .header("last-event-id", format!("v1:{after}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    let mut body = response.into_body();
    let frame = tokio::time::timeout(Duration::from_secs(2), async {
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
        .send(request(&format!("/api/v1/events/stream?cursor=v1:{after}")))
        .await;
    assert_eq!(response.status, StatusCode::GONE);
}

/// Every documented operation except the public sign-in endpoints must reject
/// anonymous callers on both the website and Unix-socket routers.
#[tokio::test]
async fn every_documented_operation_requires_authentication() {
    const PUBLIC: &[&str] = &[
        "/api/v1/auth/status",
        "/api/v1/auth/setup-link",
        "/api/v1/auth/recovery",
        "/api/v1/auth/sign-in-link",
        "/api/v1/auth/login/operator",
        "/api/v1/auth/register/start",
        "/api/v1/auth/register/finish",
        "/api/v1/auth/login/start",
        "/api/v1/auth/login/finish",
        "/api/v1/auth/device/start",
        "/api/v1/auth/device/poll",
    ];
    let temp = TempDir::new().unwrap();
    let store = Arc::new(Store::open(database(&temp)).await.unwrap());
    let auth = piqueld::auth::Auth::new(&store, "https://piqueld.example").unwrap();
    let runtime = Arc::new(FakeRuntime::new(&store));
    let state = ApiState::new(store, runtime);
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
                    .send(
                        Request::builder()
                            .method(method.as_str())
                            .uri(&uri)
                            .header("host", "localhost")
                            .header("content-type", "application/json")
                            .header("x-request-id", "anonymous-contract")
                            .body(Body::from("{}"))
                            .unwrap(),
                    )
                    .await;
                assert_eq!(
                    response.status,
                    StatusCode::UNAUTHORIZED,
                    "{method} {path} must require authentication"
                );
                // The daemon replaces a client-supplied request ID.
                let request_id = response.header("x-request-id").unwrap();
                assert_ne!(request_id, "anonymous-contract");
                let error: piqueld_core::api::ErrorBody =
                    serde_json::from_slice(&response.body).unwrap();
                assert_eq!(error.request_id, request_id);
                checked += 1;
            }
        }
    }
    assert!(checked > 60, "only {checked} operations were checked");
}

/// Two applications, `blog` and `shop`, served by the real authenticator, with
/// a `deployer` holding `apps:deploy` on `blog`, a `creator` holding
/// `apps:create` plus `apps:write` on `blog`, a `lead` holding only
/// `accounts:manage`, and an `auditor` holding only `audit:read`.
struct GrantFixture {
    temp: TempDir,
    state: ApiState,
    auth: piqueld::auth::Auth,
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
        let database = database(&temp);
        // Authentication shares the store, which tells connections that
        // access changed.
        let store = Arc::new(Store::open(&database).await.unwrap());
        let (_, state) = contract_state(&temp, Arc::clone(&store)).await;
        let mut ids = Vec::new();
        for name in ["blog", "shop"] {
            let mut input = manifest();
            input.metadata.name = name.into();
            ids.push(save(&state, input).await.application_id);
        }
        let blog = Some(ids[0].as_str());
        let deployer = seed_account(&database, "deployer", &[("apps:deploy", blog)]).await;
        let creator = [("apps:create", None), ("apps:write", blog)];
        let creator = seed_account(&database, "creator", &creator).await;
        let lead = seed_account(&database, "lead", &[("accounts:manage", None)]).await;
        let auditor = seed_account(&database, "auditor", &[("audit:read", None)]).await;
        let auth = piqueld::auth::Auth::new(&store, "https://piqueld.example").unwrap();
        let [blog, shop] = <[String; 2]>::try_from(ids).unwrap();
        Self {
            temp,
            router: api_router(state.clone(), auth.clone()),
            state,
            auth,
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
            source: None,
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
        // Allow for a loaded machine.
        eventually(
            Duration::from_secs(10),
            Duration::from_millis(20),
            async || {
                let (status, body) = self
                    .router
                    .call(token, "GET", uri, serde_json::Value::Null)
                    .await;
                assert_eq!(status, 200, "{body}");
                (body["data"]["items"].as_array().unwrap().len() >= count).then_some(body)
            },
        )
        .await
        .unwrap_or_else(|| panic!("audit trail at {uri} never held {count} records"))
    }

    /// Serves the fixture's router over TCP.
    async fn serve(&self) -> Server {
        Server::tcp(self.router.clone()).await
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
        let (status, body) = f.router.call(&f.lead, "GET", uri, none.clone()).await;
        assert_eq!(status, 200, "{uri}");
        assert_eq!(body["data"]["items"], serde_json::json!([]), "{uri}");
    }
    let (status, body) = f
        .router
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
            f.router.status(&f.deployer, "GET", &uri).await,
            404,
            "{uri}"
        );
    }
    let deploy = format!("/api/v1/environments/{}/deploy?force=true", f.blog);
    assert_eq!(f.router.status(&f.deployer, "POST", &deploy).await, 202);
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
        let (status, body) = f.router.call(&f.deployer, method, &uri, body).await;
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
        assert_eq!(f.router.status(&f.deployer, method, &uri).await, status);
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
    assert_eq!(f.router.status(&f.deployer, "GET", others).await, 403);
    // A cross-site write is refused, yet still names its valid caller.
    let foreign = Request::post("/api/v1/applications")
        .header("authorization", format!("Bearer {}", f.deployer))
        .header("origin", "https://attacker.example")
        .body(Body::empty())
        .unwrap();
    assert_eq!(f.router.send(foreign).await.status, 403);
    // So is a throttled one, once the peer's sign-in start allowance is spent.
    let mut status = 0;
    for _ in 0..31 {
        let start = serde_json::json!({});
        status = f
            .router
            .call(&f.deployer, "POST", "/api/v1/auth/device/start", start)
            .await
            .0;
    }
    assert_eq!(status, 429);
    let unknown = "pqd_0000000000000000000000000000000000000000000";
    let (status, _) = f
        .router
        .call(unknown, "GET", "/api/v1/applications", none)
        .await;
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
}

/// Callers read their own trail, so it reveals nothing they could not see:
/// a hidden environment's application is not recorded, and an API token
/// reads only its own requests, not everything its account did.
#[tokio::test]
async fn own_audit_trails_reveal_nothing_hidden() {
    let f = GrantFixture::new().await;
    let hidden = format!("/api/v1/environments/{}", f.shop);
    assert_eq!(f.router.status(&f.deployer, "GET", &hidden).await, 404);
    // Refusals before authorization, like a cross-site request, are no hint.
    let foreign = Request::delete(&hidden)
        .header("authorization", format!("Bearer {}", f.deployer))
        .header("origin", "https://attacker.example")
        .body(Body::empty())
        .unwrap();
    assert_eq!(f.router.send(foreign).await.status, 403);
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
        .router
        .call(&f.deployer, "POST", "/api/v1/auth/manage", create)
        .await;
    assert_eq!(status, 200, "{created}");
    let token = created["token"].as_str().unwrap();
    let manifest = format!("/api/v1/applications/{}/manifest", f.blog);
    assert_eq!(f.router.status(token, "GET", &manifest).await, 200);
    let body = f.audit(token, "/api/v1/audit", 1).await;
    let items = body["data"]["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "{body}");
    assert_eq!(items[0]["action"], "GET /api/v1/applications/{id}/manifest");
    let account = "/api/v1/audit?credential_id=deployer";
    assert_eq!(f.router.status(token, "GET", account).await, 403);
}

/// The trail's hash chain verifies intact, and checking it needs `audit:read`.
#[tokio::test]
async fn verifying_the_audit_chain_needs_audit_read() {
    let f = GrantFixture::new().await;
    let none = serde_json::Value::Null;
    let deploy = format!("/api/v1/environments/{}/deploy?force=true", f.blog);
    assert_eq!(f.router.status(&f.deployer, "POST", &deploy).await, 202);
    f.audit(&f.auditor, "/api/v1/audit", 1).await;
    let verify = "/api/v1/audit/verify";
    let (status, body) = f.router.call(&f.auditor, "GET", verify, none.clone()).await;
    assert_eq!(
        (status, &body["data"]["broken_at"]),
        (200, &serde_json::Value::Null)
    );
    assert_eq!(body["data"]["checked"], 1);
    assert_eq!(f.router.status(&f.deployer, "GET", verify).await, 403);
}

/// A command refused when it starts, after its connection was authorized
/// and audited as allowed, is audited and counted as a refusal too.
#[tokio::test]
async fn exec_refusals_after_the_upgrade_are_audited() {
    use futures_util::{SinkExt, StreamExt};
    use tokio_tungstenite::tungstenite::Message;
    let f = GrantFixture::new().await;
    let grants = [
        ("apps:exec", Some(f.blog.as_str())),
        ("apps:read", Some(f.blog.as_str())),
    ];
    let operator = seed_account(&database(&f.temp), "operator", &grants).await;
    let server = f.serve().await;
    let url = format!(
        "ws://{}/api/v1/environments/{}/exec",
        server.address(),
        f.blog
    );
    let (mut socket, _) = connect_exec(server.address(), url, Some(&operator)).await;
    execute(
        &database(&f.temp),
        "DELETE FROM auth_grants WHERE user_id='operator' AND permission='apps:exec'",
    )
    .await;
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
}

/// Connects an exec WebSocket to `url` at `address`, as `token` when given.
async fn connect_exec(
    address: std::net::SocketAddr,
    url: String,
    token: Option<&str>,
) -> (
    tokio_tungstenite::WebSocketStream<tokio::net::TcpStream>,
    tokio_tungstenite::tungstenite::handshake::client::Response,
) {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let mut request = url.into_client_request().unwrap();
    if let Some(token) = token {
        let bearer = format!("Bearer {token}").parse().unwrap();
        request.headers_mut().insert("authorization", bearer);
    }
    let stream = tokio::net::TcpStream::connect(address).await.unwrap();
    tokio_tungstenite::client_async(request, stream)
        .await
        .unwrap()
}

/// Opens an exec WebSocket on `environment` at `address` as `token` and
/// starts `command` with standard input attached.
async fn open_exec(
    address: std::net::SocketAddr,
    environment: &str,
    token: Option<&str>,
    command: &str,
) -> tokio_tungstenite::WebSocketStream<tokio::net::TcpStream> {
    use futures_util::SinkExt;
    use tokio_tungstenite::tungstenite::Message;
    let url = format!("ws://{address}/api/v1/environments/{environment}/exec");
    let (mut socket, _) = connect_exec(address, url, token).await;
    let start = serde_json::json!({"service": "web", "command": [command], "stdin": true});
    socket.send(Message::text(start.to_string())).await.unwrap();
    socket
}

/// A running command ends with the refusal an equivalent request would get
/// once its caller's credential is revoked, its account deleted, or its
/// grants no longer include `apps:exec`, even if the runtime never stops it.
#[tokio::test]
async fn exec_sessions_end_once_access_is_lost() {
    use futures_util::{SinkExt, StreamExt};
    use piqueld_core::exec::{ExecFrame, ExecInput, ExecOutput};
    use serde_json::json;
    use tokio_tungstenite::tungstenite::Message;
    let f = GrantFixture::new().await;
    let database = database(&f.temp);
    let server = f.serve().await;
    let admin = admin_token();
    // Deleting an account needs another administrator able to sign in.
    execute(&database, "INSERT INTO auth_passkeys(id,user_id,name,credential,created_at) VALUES('key','contract-admin','key','{}',1)").await;
    let reader = json!([{"permission": "apps:read", "applications": [f.blog]}]);
    let changes = [
        (
            "revoked",
            "hang",
            json!({"action": "revoke_credential", "id": "revoked"}),
            401,
        ),
        (
            "deleted",
            "cat",
            json!({"action": "delete_user", "user_id": "deleted"}),
            401,
        ),
        (
            "demoted",
            "cat",
            json!({"action": "set_grants", "user_id": "demoted", "grants": reader}),
            403,
        ),
    ];
    let grants = [
        ("apps:exec", Some(f.blog.as_str())),
        ("apps:read", Some(f.blog.as_str())),
    ];
    for (name, command, change, status) in changes {
        let token = seed_account(&database, name, &grants).await;
        let mut socket = open_exec(server.address(), &f.blog, Some(&token), command).await;
        let echo = ExecInput::Stdin(b"hi".to_vec()).encode();
        socket.send(Message::binary(echo)).await.unwrap();
        let mut next = async || loop {
            match socket.next().await {
                Some(Ok(Message::Binary(data))) => return Some(ExecOutput::decode(&data).unwrap()),
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                _ => return None,
            }
        };
        assert!(matches!(next().await, Some(ExecOutput::Stdout(data)) if data == b"hi"));
        let manage = "/api/v1/auth/manage";
        assert_eq!(
            f.router.call(&admin, "POST", manage, change).await.0,
            200,
            "{name}"
        );
        // A runtime that never stops gets five seconds to wind down.
        let last = tokio::time::timeout(Duration::from_secs(10), next())
            .await
            .expect("the session ends promptly");
        assert!(
            matches!(&last, Some(ExecOutput::Failed { status: got, .. }) if *got == status),
            "{name}: {last:?}"
        );
    }
}

/// Access lost while a command is being created is noticed before it runs
/// or is recorded as started, even if it would finish at once.
#[tokio::test]
async fn exec_rechecks_access_once_the_command_is_created() {
    use futures_util::{SinkExt, StreamExt};
    use piqueld_core::exec::{ExecFrame, ExecInput, ExecOutput};
    use tokio_tungstenite::tungstenite::Message;
    let f = GrantFixture::new().await;
    let grants = [("apps:exec", Some(f.blog.as_str()))];
    let token = seed_account(&database(&f.temp), "operator", &grants).await;
    let server = f.serve().await;
    let mut socket = open_exec(server.address(), &f.blog, Some(&token), "slow").await;
    // Input that would let the command finish as soon as it runs.
    for input in [ExecInput::Stdin(b"hi".to_vec()), ExecInput::CloseStdin] {
        socket.send(Message::binary(input.encode())).await.unwrap();
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    let revoke = serde_json::json!({"action": "revoke_credential", "id": "operator"});
    let manage = "/api/v1/auth/manage";
    assert_eq!(f.router.call(&token, "POST", manage, revoke).await.0, 200);
    let mut outputs = Vec::new();
    while let Some(Ok(message)) = socket.next().await {
        if let Message::Binary(data) = message {
            outputs.push(ExecOutput::decode(&data).unwrap());
        }
    }
    assert!(
        matches!(&outputs[..], [ExecOutput::Failed { status: 401, .. }]),
        "{outputs:?}"
    );
    // History does not claim the command started.
    let filter = piqueld_core::observability::EventFilter {
        kind: Some("command_started".into()),
        ..Default::default()
    };
    let started = f
        .state
        .filtered_events(&filter, &piqueld::store::Visibility::ALL, None, 10)
        .await
        .unwrap();
    assert!(started.items.is_empty());
}

/// A client that leaves while the command is not reading its input, so
/// that input is no longer read from the connection either, still ends the
/// session.
#[tokio::test]
async fn exec_notices_a_client_that_left_behind_unread_input() {
    use futures_util::SinkExt;
    use piqueld_core::exec::{ExecFrame, ExecInput};
    use tokio_tungstenite::tungstenite::Message;
    let temp = tempfile::tempdir().unwrap();
    let state = state(&temp).await;
    let app = save(&state, manifest()).await.application_id;
    let server = Server::tcp(router(state.clone(), FakeAuth)).await;
    let mut socket = open_exec(server.address(), app.as_str(), None, "block").await;
    // More input than the relay buffers, so it stops reading the connection.
    for _ in 0..32 {
        let input = ExecInput::Stdin(vec![0; 1024]).encode();
        socket.send(Message::binary(input)).await.unwrap();
    }
    drop(socket);
    let filter = piqueld_core::observability::EventFilter {
        kind: Some("command_finished".into()),
        ..Default::default()
    };
    let finished = eventually(
        Duration::from_secs(20),
        Duration::from_millis(100),
        async || {
            let events = state
                .filtered_events(&filter, &piqueld::store::Visibility::ALL, None, 10)
                .await
                .unwrap()
                .items;
            (!events.is_empty()).then_some(events)
        },
    )
    .await
    .expect("the session ends once the client is gone");
    assert_eq!(
        finished[0].message.as_deref(),
        Some("Command stream in task task-1 failed")
    );
}

/// Reads an event stream until it ends, or until a message contains
/// `needle` when given, returning what was read.
async fn read_stream(body: &mut Body, needle: Option<&str>) -> String {
    let mut text = String::new();
    while let Some(frame) = body.frame().await {
        if let Ok(data) = frame.unwrap().into_data() {
            text.push_str(std::str::from_utf8(&data).unwrap());
            if needle.is_some_and(|needle| text.contains(needle)) {
                break;
            }
        }
    }
    text
}

/// An event stream ends with an `access_ended` message once its caller's
/// credential is revoked, without sending events it read before, and
/// without a message when the daemon shuts down.
#[tokio::test]
async fn event_streams_end_once_access_is_lost_or_the_daemon_stops() {
    let f = GrantFixture::new().await;
    let reader = seed_account(&database(&f.temp), "reader", &[("events:read", None)]).await;
    let shutdown = tokio_util::sync::CancellationToken::new();
    let router = api_router(
        f.state.clone().with_shutdown(shutdown.clone()),
        f.auth.clone(),
    );
    let open = async |token: &str| {
        let request = Request::builder()
            .uri("/api/v1/events/stream")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        let response = router.open(request).await;
        assert_eq!(response.status(), 200);
        response.into_body()
    };
    let mut revoked = open(&reader).await;
    let mut stopped = open(&admin_token()).await;
    // The first event leaves the rest of the batch read but not yet sent.
    read_stream(&mut revoked, Some("event: event")).await;
    let revoke = serde_json::json!({"action": "revoke_credential", "id": "reader"});
    let manage = "/api/v1/auth/manage";
    assert_eq!(f.router.call(&reader, "POST", manage, revoke).await.0, 200);
    let text = tokio::time::timeout(Duration::from_secs(5), read_stream(&mut revoked, None))
        .await
        .expect("the stream ends promptly");
    assert!(
        text.trim_start().starts_with("event: access_ended"),
        "{text}"
    );
    assert!(text.contains("authentication_required"), "{text}");
    assert!(!text.contains("event: event"), "{text}");

    shutdown.cancel();
    let text = tokio::time::timeout(Duration::from_secs(5), read_stream(&mut stopped, None))
        .await
        .expect("the stream ends on shutdown");
    assert!(!text.contains("access_ended"), "{text}");
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
        let (status, body) = f.router.call(&f.deployer, "POST", start, json!({})).await;
        assert_eq!(status, 200, "{body}");
        codes.push((body["device_code"].clone(), body["user_code"].clone()));
    }
    let [(pending, _), (approved, user_code)] = <[_; 2]>::try_from(codes).unwrap();
    let (_, body) = f
        .router
        .call(&f.deployer, "POST", poll, json!({"device_code": pending}))
        .await;
    assert_eq!(body["status"], "authorization_pending");
    let approve = json!({"user_code": user_code});
    let (status, _) = f
        .router
        .call(&f.deployer, "POST", "/api/v1/auth/device/approve", approve)
        .await;
    assert_eq!(status, 200);
    let (_, body) = f
        .router
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
        .router
        .call(&f.creator, "POST", "/api/v1/applications", name("notes"))
        .await;
    assert_eq!(status, 200, "{body}");
    let created = format!(
        "/api/v1/applications/{}",
        body["data"]["application_id"].as_str().unwrap()
    );
    assert_eq!(f.router.status(&f.creator, "GET", &created).await, 200);
    let mut input = manifest();
    input.metadata.name = "shop".into();
    let apply = serde_json::json!({"manifest": input});
    let (status, body) = f
        .router
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
        .router
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
    let auth = auth(&temp).await;
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
        let response = listener.send(request("/api/v1/auth/setup-link")).await;
        assert_eq!(response.status, status);
        if status == 200 {
            let setup: piqueld_core::auth::SetupLink =
                serde_json::from_slice(&response.body).unwrap();
            assert_eq!(setup.url, link.trim());
        }
    }
    // The refused remote request is audited in the background.
    let event = refusals(&state, |events| match events.as_slice() {
        [event] => Some(event.clone()),
        _ => None,
    })
    .await
    .expect("the refused setup link request was not audited");
    assert_eq!(
        (event.action.as_str(), event.status),
        ("GET /api/v1/auth/setup-link", 404)
    );
}

/// Waits for the refusals audited in the background until `check` finds
/// what it expects in them.
async fn refusals<T>(
    state: &ApiState,
    check: impl Fn(Vec<piqueld_core::audit::AuditEvent>) -> Option<T>,
) -> Option<T> {
    let denied = piqueld_core::audit::AuditFilter {
        outcome: Some(piqueld_core::audit::AuditOutcome::Denied),
        ..Default::default()
    };
    // Allow for a loaded machine.
    eventually(
        Duration::from_secs(10),
        Duration::from_millis(20),
        async || check(state.audit_events(&denied, None, 10).await.unwrap().items),
    )
    .await
}

/// Sends `request` to `router` as if from a Unix socket peer running as
/// `uid` (TCP routers ignore it), with an optional JSON `body`. Returns the
/// status, the JSON response body, and any `Set-Cookie` value.
async fn as_peer(
    router: &axum::Router,
    request: axum::http::request::Builder,
    uid: Option<u32>,
    body: Option<serde_json::Value>,
) -> (u16, serde_json::Value, Option<String>) {
    use axum::http::header;
    let request = request.header(header::HOST, "localhost");
    let mut request = match body {
        Some(body) => request
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body.to_string())),
        None => request.body(Body::empty()),
    }
    .unwrap();
    request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(piqueld::api::http::UnixPeer {
            uid,
        }));
    let response = router.send(request).await;
    let cookie = response.header(header::SET_COOKIE).map(str::to_owned);
    (response.status.as_u16(), response.json(), cookie)
}

/// Admin recovery is served only over the Unix socket, only to root or the
/// daemon's own user, and only once setup has completed.
#[tokio::test]
async fn admin_recovery_requires_the_host_operator_over_the_unix_socket() {
    let temp = TempDir::new().unwrap();
    let state = state(&temp).await;
    let database = database(&temp);
    let auth = auth(&temp).await;
    let operator = rustix::process::geteuid().as_raw();
    let stranger = if operator == 0 { 1 } else { operator + 1 };
    let unix = api_router(state.clone(), auth.clone());
    let web = web_router(state.clone(), UiAssets::Embedded(TEST_BUNDLE), auth);
    let recover = async |router: &axum::Router, uid: Option<u32>| {
        let (status, body, _) =
            as_peer(router, Request::post("/api/v1/auth/recovery"), uid, None).await;
        (status, body)
    };
    assert_eq!(
        recover(&unix, Some(operator)).await.0,
        409,
        "setup is still open"
    );
    seed_account(&database, "admin", &[("admin", None)]).await;
    execute(&database, "UPDATE auth_setup SET initialized=1").await;
    for (router, uid) in [
        (&unix, None),
        (&unix, Some(stranger)),
        (&web, Some(operator)),
    ] {
        assert_eq!(recover(router, uid).await.0, 404, "{uid:?}");
    }
    let (status, body) = recover(&unix, Some(operator)).await;
    assert_eq!(status, 200, "{body}");
    assert!(
        body["url"]
            .as_str()
            .unwrap()
            .contains("/dashboard/auth#invite=")
    );
}

/// Over the Unix socket, root and the daemon's own user act as the host
/// operator without a token; anyone else, like a member of the socket's
/// group, still needs one, and a presented token keeps its meaning. Only the
/// operator gets a sign-in link, which signs one browser in, once. The audit
/// trail names the operator.
#[tokio::test]
async fn the_host_operator_acts_without_a_token_over_the_unix_socket() {
    use axum::http::header;
    use piqueld_core::{audit::AuditFilter, auth::HostOperator};
    let temp = TempDir::new().unwrap();
    let state = state(&temp).await;
    let auth = auth(&temp).await;
    let operator = rustix::process::geteuid().as_raw();
    let stranger = if operator == 0 { 1 } else { operator + 1 };
    let unix = api_router(state.clone(), auth.clone());
    let web = web_router(state.clone(), UiAssets::Embedded(TEST_BUNDLE), auth);
    let me = "/api/v1/auth/me";
    for uid in [operator, 0] {
        let (status, body, _) = as_peer(&unix, Request::get(me), Some(uid), None).await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["operator"]["uid"].as_u64(), Some(u64::from(uid)));
        assert_eq!(body["grants"][0]["permission"], "admin");
    }
    for (router, uid) in [
        (&unix, Some(stranger)),
        (&unix, None),
        (&web, Some(operator)),
    ] {
        let status = as_peer(router, Request::get(me), uid, None).await.0;
        assert_eq!(status, 401, "{uid:?}");
    }
    let issue = "/api/v1/auth/sign-in-link";
    for (router, uid) in [(&unix, Some(stranger)), (&web, Some(operator))] {
        assert_eq!(
            as_peer(router, Request::post(issue), uid, None).await.0,
            404
        );
    }
    let (status, link, _) = as_peer(&unix, Request::post(issue), Some(operator), None).await;
    assert_eq!(status, 200, "{link}");
    let secret = link["url"]
        .as_str()
        .unwrap()
        .split_once("#operator=")
        .unwrap()
        .1;
    let login = || {
        Request::post("/api/v1/auth/login/operator")
            .header(header::ORIGIN, "https://piqueld.example")
    };
    let redeem = Some(serde_json::json!({ "secret": secret }));
    let (status, body, cookie) = as_peer(&web, login(), None, redeem.clone()).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["operator"]["uid"].as_u64(), Some(u64::from(operator)));
    let cookie = cookie.unwrap();
    assert!(cookie.contains("Max-Age=43200"), "{cookie}");
    assert_eq!(
        as_peer(&web, login(), None, redeem).await.0,
        401,
        "single use"
    );
    let session = cookie.split(';').next().unwrap();
    let request = Request::get(me).header(header::COOKIE, session);
    let (status, body, _) = as_peer(&web, request, None, None).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["operator"]["uid"].as_u64(), Some(u64::from(operator)));

    let foreign = Request::post(issue).header(header::ORIGIN, "https://attacker.example");
    assert_eq!(as_peer(&unix, foreign, Some(operator), None).await.0, 403);

    // Issuing the link and signing in with it are audited, in the background,
    // as the operator.
    let operator = Some(HostOperator { uid: operator });
    eventually(
        Duration::from_secs(10),
        Duration::from_millis(20),
        async || {
            let page = state
                .audit_events(&AuditFilter::default(), None, 50)
                .await
                .unwrap();
            let audited = |action: &str, status: u16| {
                page.items.iter().any(|event| {
                    event.action == action && event.status == status && event.operator == operator
                })
            };
            // Refusals before authentication, like a cross-site request, too.
            (audited("POST /api/v1/auth/sign-in-link", 200)
                && audited("POST /api/v1/auth/login/operator", 200)
                && audited("POST /api/v1/auth/sign-in-link", 403))
            .then_some(())
        },
    )
    .await
    .expect("the host operator's requests were not audited as the operator");
}

/// Over the Unix socket, a credential the host operator presents keeps its
/// meaning: the scheme is case-insensitive, an unusable credential never
/// falls back to the operator, and only the token-less operator gets a
/// sign-in link.
#[tokio::test]
async fn credentials_presented_by_the_host_operator_keep_their_meaning() {
    use axum::http::header;
    let temp = TempDir::new().unwrap();
    let state = state(&temp).await;
    let auth = auth(&temp).await;
    let operator = rustix::process::geteuid().as_raw();
    let unix = api_router(state, auth);
    let me = "/api/v1/auth/me";
    let token = admin_token();
    for scheme in ["Bearer", "bearer"] {
        let bearer = format!("{scheme} {token}");
        let request = Request::get(me).header(header::AUTHORIZATION, bearer);
        let (_, body, _) = as_peer(&unix, request, Some(operator), None).await;
        assert_eq!(body["user"]["username"], "contract-admin", "{scheme}");
    }
    for unusable in ["Basic dXNlcjpwYXNz", "Bearer pqd_unknown"] {
        let request = Request::get(me).header(header::AUTHORIZATION, unusable);
        let status = as_peer(&unix, request, Some(operator), None).await.0;
        assert_eq!(status, 401, "{unusable}");
    }
    // So does a session cookie, even in a later `Cookie` header.
    let cookies = |request: axum::http::request::Builder| {
        request
            .header(header::COOKIE, "other=one")
            .header(header::COOKIE, "__Host-piqueld_session=pqd_unknown")
    };
    let status = as_peer(&unix, cookies(Request::get(me)), Some(operator), None)
        .await
        .0;
    assert_eq!(status, 401);
    // Even one that cannot be parsed.
    let raw = header::HeaderValue::from_bytes(b"other=\xff; __Host-piqueld_session=x").unwrap();
    let request = Request::get(me).header(header::COOKIE, raw);
    assert_eq!(as_peer(&unix, request, Some(operator), None).await.0, 401);
    let issue = Request::post("/api/v1/auth/sign-in-link")
        .header(header::AUTHORIZATION, format!("Bearer {token}"));
    assert_eq!(as_peer(&unix, issue, Some(operator), None).await.0, 404);
    // A cookie-authenticated write without the site's origin is refused
    // outright, rather than issuing a link.
    let issue = cookies(Request::post("/api/v1/auth/sign-in-link"));
    assert_eq!(as_peer(&unix, issue, Some(operator), None).await.0, 403);
}

/// A fixed tailnet: `100.64.0.1` is Alice's laptop, `100.64.0.2` a CI runner,
/// `100.64.0.3` a runner that just lost `tag:ci`, which only a cached (not
/// fresh) lookup still reports, `100.64.0.4` a runner that just left the
/// tailnet, which only a cached lookup still finds, and `100.64.0.5` a runner
/// whose fresh lookups time out.
struct FakeTailnet;

#[async_trait]
impl piqueld::tailnet::TailnetLookup for FakeTailnet {
    async fn whois(
        &self,
        peer: std::net::SocketAddr,
        fresh: bool,
    ) -> Result<Option<piqueld_core::tailnet::TailnetPeer>, piqueld::tailnet::LookupTimedOut> {
        let (login, node, tags) = match peer.ip().to_string().as_str() {
            "100.64.0.1" => (Some("alice@example.com".into()), "laptop", Vec::new()),
            "100.64.0.2" => (None, "runner", vec!["tag:ci".to_owned()]),
            "100.64.0.3" if fresh => (None, "retired", vec!["tag:old".to_owned()]),
            "100.64.0.3" => (None, "retired", vec!["tag:ci".to_owned()]),
            "100.64.0.4" if fresh => return Ok(None),
            "100.64.0.4" => (None, "vanished", vec!["tag:ci".to_owned()]),
            "100.64.0.5" if fresh => return Err(piqueld::tailnet::LookupTimedOut),
            "100.64.0.5" => (None, "busy", vec!["tag:ci".to_owned()]),
            _ => return Ok(None),
        };
        Ok(Some(piqueld_core::tailnet::TailnetPeer {
            login,
            node: node.into(),
            tags,
        }))
    }
}

/// A daemon whose administrator created a token bound to `tag:ci`: its
/// state, authentication, and the token.
async fn tailnet_bound_token(temp: &TempDir) -> (ApiState, piqueld::auth::Auth, String) {
    use piqueld::auth::{Auth, TokenPolicy};
    let state = state(temp).await;
    let database = database(temp);
    let admin = seed_account(&database, "admin", &[("admin", None)]).await;
    let store = Store::open(&database).await.unwrap();
    let tokens = TokenPolicy {
        max_days: None,
        tailnet: true,
    };
    let auth = Auth::configured(&store, "https://piqueld.example", tokens).unwrap();
    let create = piqueld_core::auth::Manage::CreateToken {
        grants: piqueld_core::access::Grants::admin(),
        name: "ci".into(),
        days: Some(1),
        tailnet: Some(piqueld_core::tailnet::TailnetBinding::parse("tag:ci").unwrap()),
    };
    let unix = api_router(state.clone(), auth.clone());
    let (_, managed) = unix
        .call(
            &admin,
            "POST",
            "/api/v1/auth/manage",
            serde_json::to_value(&create).unwrap(),
        )
        .await;
    let token = managed["token"].as_str().expect("token").to_owned();
    (state, auth, token)
}

/// A token bound to a tailnet tag works only through the tailnet node from a
/// device carrying that tag right now; refusals name the tailnet peer in the
/// audit trail.
#[tokio::test]
async fn tailnet_bound_tokens_work_only_from_matching_devices() {
    let temp = TempDir::new().unwrap();
    let (state, auth, token) = tailnet_bound_token(&temp).await;
    let unix = api_router(state.clone(), auth.clone());
    let lookup: Arc<dyn piqueld::tailnet::TailnetLookup> = Arc::new(FakeTailnet);
    let tailnet = web_router(state.clone(), UiAssets::Embedded(TEST_BUNDLE), auth)
        .layer(axum::Extension(lookup));
    let me = async |router: &axum::Router, peer: Option<[u8; 4]>| {
        let mut request = Request::get("/api/v1/auth/me")
            .header("host", "localhost")
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap();
        if let Some(ip) = peer {
            let address = std::net::SocketAddr::from((ip, 40_000));
            request
                .extensions_mut()
                .insert(axum::extract::ConnectInfo(address));
        }
        router.send(request).await.status.as_u16()
    };
    assert_eq!(
        me(&tailnet, Some([100, 64, 0, 2])).await,
        200,
        "the CI runner"
    );
    assert_eq!(
        me(&tailnet, Some([100, 64, 0, 1])).await,
        401,
        "Alice's laptop"
    );
    let retagged = me(&tailnet, Some([100, 64, 0, 3])).await;
    assert_eq!(retagged, 401, "a cached identity never authorizes");
    let vanished = me(&tailnet, Some([100, 64, 0, 4])).await;
    assert_eq!(vanished, 401, "nor stands in for a failed lookup");
    let busy = me(&tailnet, Some([100, 64, 0, 5])).await;
    assert_eq!(busy, 503, "a lookup that timed out is no mismatch");
    assert_eq!(me(&unix, None).await, 401, "outside the tailnet");
    let from = refusals(&state, |events| {
        (events.len() >= 4).then(|| {
            events
                .into_iter()
                .map(|event| event.tailnet)
                .collect::<std::collections::BTreeSet<_>>()
        })
    })
    .await
    .expect("the refused requests were not audited");
    // The vanished runner's refusal names no peer, not its cached one.
    let described = ["alice@example.com on laptop", "tag:old on retired"];
    let expected = std::iter::once(None)
        .chain(described.map(|who| Some(who.to_owned())))
        .collect();
    assert_eq!(from, expected);
}

/// Behind the gateway's dashboard hostname, one connection carries many
/// tailnet clients, each named by the `X-Forwarded-For` header the gateway
/// sets, so tailnet-bound tokens work from matching devices there too.
#[tokio::test]
async fn the_dashboard_socket_names_clients_by_the_gateways_header() {
    let temp = TempDir::new().unwrap();
    let (state, auth, token) = tailnet_bound_token(&temp).await;
    let lookup: Arc<dyn piqueld::tailnet::TailnetLookup> = Arc::new(FakeTailnet);
    let gateway = piqueld::api::http::gateway_router(
        web_router(state.clone(), UiAssets::Embedded(TEST_BUNDLE), auth)
            .layer(axum::Extension(lookup)),
    );
    let through_gateway = async |client: Option<&str>| {
        let mut request = Request::get("/api/v1/auth/me")
            .header("host", "localhost")
            .header("authorization", format!("Bearer {token}"));
        if let Some(client) = client {
            request = request.header("x-forwarded-for", client);
        }
        let request = request.body(Body::empty()).unwrap();
        gateway.send(request).await.status.as_u16()
    };
    assert_eq!(
        through_gateway(Some("100.64.0.2")).await,
        200,
        "the CI runner"
    );
    assert_eq!(
        through_gateway(Some("100.64.0.1")).await,
        401,
        "Alice's laptop"
    );
    assert_eq!(through_gateway(None).await, 403, "not through the gateway");
}

/// A runner whose tag is `tag:ci` on the first lookup, and `tag:old` on every
/// later one, so authorizing a connection must look it up only once.
struct Retagged(std::sync::atomic::AtomicUsize);

#[async_trait]
impl piqueld::tailnet::TailnetLookup for Retagged {
    async fn whois(
        &self,
        _: std::net::SocketAddr,
        _: bool,
    ) -> Result<Option<piqueld_core::tailnet::TailnetPeer>, piqueld::tailnet::LookupTimedOut> {
        let lookups = self.0.fetch_add(1, Ordering::SeqCst);
        let tag = if lookups == 0 { "tag:ci" } else { "tag:old" };
        Ok(Some(piqueld_core::tailnet::TailnetPeer {
            login: None,
            node: "runner".into(),
            tags: vec![tag.to_owned()],
        }))
    }
}

/// A bound token's binding is rechecked when a command arrives: a device
/// retagged since the connection was authorized cannot start one, and the
/// refusal is audited with the device as it is now.
#[tokio::test]
async fn tailnet_bindings_are_rechecked_when_a_command_starts() {
    use piqueld_client::exec::{ExecCommand, ExecOutput, ExecRequest};
    let temp = TempDir::new().unwrap();
    let (state, auth, token) = tailnet_bound_token(&temp).await;
    let saved = save(&state, manifest()).await;
    let lookup: Arc<dyn piqueld::tailnet::TailnetLookup> =
        Arc::new(Retagged(std::sync::atomic::AtomicUsize::new(0)));
    let tailnet = web_router(state.clone(), UiAssets::Embedded(TEST_BUNDLE), auth)
        .layer(axum::Extension(lookup));
    let server = Server::tcp_with_peers(tailnet).await;
    let client = server.client().with_bearer(&token).unwrap();
    let request = ExecRequest {
        service: "web".parse().unwrap(),
        command: ExecCommand::parse(vec!["true".into()]).unwrap(),
        stdin: false,
        tty: None,
    };
    // The default environment shares its application's ID.
    let environment = saved.application_id.as_str();
    let (mut output, _input) = client.exec(environment, &request).await.unwrap();
    assert!(matches!(
        output.next().await,
        Ok(Some(ExecOutput::Failed { status: 401, error }))
            if error.code == "tailnet_binding_mismatch"
    ));
    let event = refusals(&state, |events| match events.as_slice() {
        [event] => Some(event.clone()),
        _ => None,
    })
    .await
    .expect("the refused command was not audited");
    assert_eq!(event.tailnet.as_deref(), Some("tag:old on runner"));
}

/// Authentication short circuits still use the common error correlation layers,
/// including storage failures that must not attempt another database write.
#[tokio::test]
async fn authentication_errors_preserve_request_and_diagnostic_ids() {
    use sqlx::Connection as _;

    let temp = TempDir::new().unwrap();
    let state = state(&temp).await;
    let store = Store::open(database(&temp)).await.unwrap();
    let mut connection = sqlite::connect(&database(&temp)).await;
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
            // A bearer that could not be checked is no reason to require
            // the ceremony's Origin.
            (
                "POST",
                "/api/v1/auth/register/start",
                "x".repeat(43),
                "",
                503,
                "storage_unavailable",
            ),
        ] {
            let mut request = Request::builder()
                .method(method)
                .uri(path)
                .header("host", "localhost")
                .header("authorization", format!("Bearer {bearer}"))
                .header("x-request-id", "auth-correlation");
            if !origin.is_empty() {
                request = request.header("origin", origin);
            }
            let response = tokio::time::timeout(
                Duration::from_secs(2),
                listener.send(request.body(Body::empty()).unwrap()),
            )
            .await
            .expect("authentication errors must not wait for a diagnostic write");
            assert_eq!(response.status.as_u16(), status);
            let request_id = response.header("x-request-id").unwrap();
            assert_ne!(request_id, "auth-correlation");
            let error: piqueld_core::api::ErrorBody =
                serde_json::from_slice(&response.body).unwrap();
            assert_eq!(error.code, code);
            assert_eq!(error.request_id, request_id);
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

/// Authentication endpoints answer undecodable bodies, and bodies over the
/// small limit of public sign-in endpoints, with the usual JSON error, code,
/// and request ID.
#[tokio::test]
async fn authentication_bodies_fail_with_structured_errors() {
    let temp = TempDir::new().unwrap();
    let store = Store::open(database(&temp)).await.unwrap();
    let auth = piqueld::auth::Auth::new(&store, "https://piqueld.example").unwrap();
    let unix = api_router(state(&temp).await, auth);
    let oversized = format!(r#"{{"device_code":"{}"}}"#, "x".repeat(64 * 1024));
    for (content_type, body, status, code) in [
        ("application/json", "{", 400, "json_malformed"),
        ("text/plain", "{}", 415, "content_type_unsupported"),
        (
            "application/json",
            oversized.as_str(),
            413,
            "request_body_too_large",
        ),
    ] {
        let request = Request::post("/api/v1/auth/device/poll")
            .header("content-type", content_type)
            .body(Body::from(body.to_owned()))
            .unwrap();
        let response = unix.send(request).await;
        assert_eq!(response.status.as_u16(), status);
        let error: piqueld_core::api::ErrorBody = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(error.code, code);
        assert_eq!(
            response.header("x-request-id"),
            Some(error.request_id.as_str())
        );
    }
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
    let service = ApiState::new(store.clone(), runtime.clone());
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
    let server = Server::tcp(router(ApiState::new(store, runtime), FakeAuth)).await;
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
    let mut saved = save(service, manifest()).await;
    let application = piqueld_core::ApplicationId::parse(&saved.application_id).unwrap();
    let create = |name: &str| Mutation::CreateEnvironment {
        application: application.clone(),
        name: piqueld_core::EnvironmentName::parse(name).unwrap(),
        source: None,
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
            .environment_detail(&production, &piqueld_core::access::Scope::All)
            .await
            .unwrap()
            .latest_operation
            .is_none()
    );
    assert_eq!(
        service
            .environment_detail(&staging.id, &piqueld_core::access::Scope::All)
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
    save(&service, other).await;
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
        ..EnvironmentConfig::default()
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
    // Blocks declare environments: saving it would create `preview`, and
    // `production` would no longer be declared.
    let name = |name: &str| piqueld_core::EnvironmentName::parse(name).unwrap();
    assert_eq!(
        rendered.environments,
        [
            piqueld_core::manifest::EnvironmentChange::Create {
                environment: name("preview"),
                source: piqueld_core::manifest::DeclaredSource::Saved,
                sync: false,
            },
            piqueld_core::manifest::EnvironmentChange::Undeclared {
                environment: name("production"),
            },
        ]
    );
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
    let router = api_router(state, auth(&temp).await);
    let start = |body: String| {
        let request = Request::builder()
            .method("POST")
            .uri("/api/v1/auth/device/start")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap();
        router.send(request)
    };
    let limited = serde_json::json!({"grants": [{"permission": "apps:read"}]});
    let response = start(limited.to_string()).await;
    assert_eq!(response.status, StatusCode::OK);
    assert_eq!(response.json()["grants"], limited["grants"]);
    let large = format!(r#"{{"grants":[],"padding":"{}"}}"#, "x".repeat(16 * 1024));
    let response = start(large).await;
    assert_eq!(response.status, StatusCode::PAYLOAD_TOO_LARGE);
    // The limit applies while reading, also without a `Content-Length`.
    let chunks = (0..64).map(|_| Ok::<_, std::io::Error>(vec![b' '; 1024]));
    let streamed = Request::builder()
        .method("POST")
        .uri("/api/v1/auth/device/start")
        .header("content-type", "application/json")
        .body(Body::from_stream(futures_util::stream::iter(chunks)))
        .unwrap();
    let response = router.send(streamed).await;
    assert_eq!(response.status, StatusCode::PAYLOAD_TOO_LARGE);
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

/// A Git repository with one empty commit on `main` and on each of `branches`.
fn repository_with_branches(branches: &[&str]) -> Repository {
    let repository = Repository::new();
    repository.commit(&[]);
    for branch in branches {
        repository.git(&["branch", branch]);
    }
    repository
}

/// Pruning deletes only the previews whose branch the repository confirms is
/// gone, and nothing at all while the repository cannot be read.
#[tokio::test]
async fn pruning_deletes_previews_only_when_their_branch_is_confirmed_gone() {
    use piqueld_client::{BranchState, CreatePreviewRequest, PrunePreviewsRequest};
    let temp = tempfile::tempdir().unwrap();
    let repository = repository_with_branches(&["feat/gone", "feat/kept"]);
    let (server, application) = connected_application(&temp, &repository, None).await;
    let client = server.client();
    let application = application.as_str();
    let mut previews = Vec::new();
    for branch in ["feat/gone", "feat/kept"] {
        let request = CreatePreviewRequest {
            branch: branch.into(),
            slot: None,
        };
        previews.push(
            client
                .create_preview(application, &request)
                .await
                .unwrap()
                .preview
                .id,
        );
    }
    let prune = PrunePreviewsRequest {
        previews: previews.clone(),
    };
    let deleting = async || {
        let mut deleting = Vec::new();
        for preview in &previews {
            deleting.push(
                client
                    .preview(preview.as_str())
                    .await
                    .unwrap()
                    .preview
                    .delete_intent,
            );
        }
        deleting
    };
    repository.git(&["branch", "-D", "feat/gone"]);
    let states = async || {
        client
            .previews(application)
            .await
            .unwrap()
            .into_iter()
            .map(|view| view.branch)
            .collect::<Vec<_>>()
    };
    assert!(matches!(
        states().await.as_slice(),
        [BranchState::Gone, BranchState::Exists { .. }]
    ));

    // An unreadable repository leaves every branch unknown and deletes nothing.
    let moved = temp.path().join("moved");
    std::fs::rename(repository.path(), &moved).unwrap();
    assert!(matches!(
        states().await.as_slice(),
        [BranchState::Unknown { .. }, BranchState::Unknown { .. }]
    ));
    let error = client
        .prune_previews(application, &prune)
        .await
        .unwrap_err();
    assert_eq!(api_error(error).status, 502);
    assert_eq!(deleting().await, [false, false]);

    std::fs::rename(&moved, repository.path()).unwrap();
    let deleted = client.prune_previews(application, &prune).await.unwrap();
    assert_eq!(deleted.len(), 1);
    assert_eq!(deleted[0].preview.id, previews[0]);
    assert_eq!(deleting().await, [true, false]);
}

/// Serves the contract API over TCP, with `max_upload_bytes` if given, and
/// saves the contract manifest connected to `repository`. Returns the
/// server and the application ID.
async fn connected_application(
    temp: &TempDir,
    repository: &Repository,
    max_upload_bytes: Option<std::num::NonZeroU64>,
) -> (Server, String) {
    let server = Server::tcp(router(state_with(temp, max_upload_bytes).await, FakeAuth)).await;
    let saved = server
        .client()
        .apply_application(&ApplyApplicationRequest {
            manifest: connected_manifest(repository),
            ..new_application()
        })
        .await
        .unwrap();
    (server, saved.application_id)
}

/// The contract manifest, connected to `app.toml` on `repository`'s `main`.
fn connected_manifest(repository: &Repository) -> ApplicationManifest {
    let mut manifest = manifest();
    manifest.spec.manifest = Some(
        serde_json::from_value(serde_json::json!({
            "repository": {"url": repository.path().display().to_string(), "branch": "main"},
            "path": "app.toml",
        }))
        .unwrap(),
    );
    manifest
}

/// A snapshot of a never-pushed branch is uploaded thin against the base the
/// repository has, deploys to a new preview whose branch state is `local`,
/// and is never pruned; oversized and invalid bundles get their own errors.
#[tokio::test]
async fn local_snapshots_upload_within_limits_and_are_never_pruned() {
    use piqueld_client::{BranchState, CreatePreviewRequest, PrunePreviewsRequest};
    let temp = tempfile::tempdir().unwrap();
    let repository = repository_with_branches(&["feat/gone"]);
    let limit = std::num::NonZeroU64::new(8192);
    let (server, application) = connected_application(&temp, &repository, limit).await;
    let client = server.client();
    let application = application.as_str();
    let request = CreatePreviewRequest {
        branch: "feat/gone".into(),
        slot: None,
    };
    let gone = client
        .create_preview(application, &request)
        .await
        .unwrap()
        .preview
        .id;
    repository.git(&["branch", "-D", "feat/gone"]);

    // A working copy with a commit on a branch the repository never had.
    let work = repository.working_copy();
    work.git(&["switch", "-qc", "feat/local"]);
    work.commit(&[("app.toml", b"local")]);
    let base = work.git(&["rev-parse", "origin/main"]);
    let upload = async |bytes: Vec<u8>| {
        let length = bytes.len() as u64;
        client
            .deploy_local_preview(application, ("feat/local", None), (bytes, length))
            .await
    };
    let created = upload(work.bundle("feat/local", Some(&base)))
        .await
        .unwrap();
    assert!(created.created);
    let local = created.preview.id;
    let deployments = client.deployments(local.as_str(), None).await.unwrap();
    assert!(matches!(
        &deployments.items[0].origin,
        piqueld_client::DeploymentOrigin::Local(snapshot) if snapshot.base.as_deref() == Some(base.as_str())
    ));

    // The never-pushed branch reads as `local`, not gone, and is kept.
    let states = client.previews(application).await.unwrap();
    let states = states
        .into_iter()
        .map(|view| view.branch)
        .collect::<Vec<_>>();
    assert_eq!(
        states,
        [BranchState::Gone, BranchState::Local { head: None }]
    );
    let prune = PrunePreviewsRequest {
        previews: vec![gone.clone(), local.clone()],
    };
    let deleted = client.prune_previews(application, &prune).await.unwrap();
    assert_eq!(deleted.len(), 1);
    assert_eq!(deleted[0].preview.id, gone);
    assert!(
        !client
            .preview(local.as_str())
            .await
            .unwrap()
            .preview
            .delete_intent
    );

    // Oversized and invalid uploads are refused before deploying anything.
    // The noise is a xorshift sequence, so it doesn't compress.
    let mut state = 0x9e37_79b9_7f4a_7c15_u64;
    let noise = (0..65_536)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state.to_le_bytes()[0]
        })
        .collect::<Vec<_>>();
    work.commit(&[("noise.bin", &noise)]);
    let large = upload(work.bundle("feat/local", None)).await.unwrap_err();
    assert_eq!(api_error(large), ApiError::new(413, "snapshot_too_large"));
    let invalid = upload(b"# v2 git bundle\nnot a bundle\n\n".to_vec()).await;
    assert_eq!(
        api_error(invalid.unwrap_err()),
        ApiError::new(422, "snapshot_invalid")
    );
    let deployments = client.deployments(local.as_str(), None).await.unwrap();
    assert_eq!(deployments.items.len(), 1);
}

/// An application `blog` with a manifest repository and a production
/// environment, an `owner` holding application permissions on it,
/// an `agent` token the owner limited to previews permissions, and a
/// `limited` account that holds only `previews:deploy` on blog.
struct PreviewFixture {
    _temp: TempDir,
    _repository: Repository,
    database: PathBuf,
    router: axum::Router,
    blog: String,
    production: String,
    owner: String,
    limited: String,
    agent: String,
}

impl PreviewFixture {
    /// The previews permissions `agent` holds.
    const AGENT: [&str; 2] = ["previews:deploy", "previews:delete"];
    /// Every permission the owner holds.
    const PERMISSIONS: [&str; 6] = [
        "apps:deploy",
        "apps:delete",
        "logs:read",
        "events:read",
        "apps:write",
        "secrets:write",
    ];

    async fn new() -> Self {
        let temp = TempDir::new().unwrap();
        let database = database(&temp);
        let repository = repository_with_branches(&["feat/one", "feat/two"]);
        let state = state(&temp).await;
        let blog = save(&state, connected_manifest(&repository))
            .await
            .application_id;
        let blog_id = piqueld_core::ApplicationId::parse(blog.as_str()).unwrap();
        // The application's only long-lived environment stands for production.
        let production = state.application(&blog_id).await.unwrap().environments[0]
            .id
            .to_string();
        let app = Some(blog.as_str());
        let owner = Self::PERMISSIONS.map(|permission| (permission, app));
        let owner = seed_account(&database, "owner", &owner).await;
        let limited = [("previews:deploy", app), ("accounts:manage", None)];
        let limited = seed_account(&database, "limited", &limited).await;
        let mut fixture = Self {
            router: api_router(state, auth(&temp).await),
            _temp: temp,
            _repository: repository,
            database,
            blog,
            production,
            owner,
            limited,
            agent: String::new(),
        };
        let request = fixture.token(&Self::AGENT);
        let (status, created) = fixture
            .router
            .call(&fixture.owner, "POST", "/api/v1/auth/manage", request)
            .await;
        assert_eq!(status, 200, "{created}");
        created["token"]
            .as_str()
            .unwrap()
            .clone_into(&mut fixture.agent);
        fixture
    }

    /// Grants of `permissions` on blog.
    fn grants(&self, permissions: &[&str]) -> serde_json::Value {
        permissions
            .iter()
            .map(|permission| {
                serde_json::json!({
                    "permission": permission,
                    "applications": [self.blog],
                })
            })
            .collect()
    }

    /// A request creating a token with [`Self::grants`].
    fn token(&self, permissions: &[&str]) -> serde_json::Value {
        let grants = self.grants(permissions);
        serde_json::json!({"action": "create_token", "grants": grants, "name": "agent", "days": 1})
    }

    /// Records a finished build of `environment`, returning its ID.
    async fn build(&self, environment: &str) -> i64 {
        sqlx::query_scalar("INSERT INTO builds(environment_id,operation_id,service,source_json,state,started_at_ms) VALUES(?1,'operation','web','{}','succeeded',1) RETURNING id")
            .bind(environment)
            .fetch_one(&mut sqlite::connect(&self.database).await)
            .await
            .unwrap()
    }

    /// Creates a preview of `branch` as the agent, returning its ID.
    async fn preview(&self, branch: &str) -> String {
        let uri = format!("/api/v1/applications/{}/previews", self.blog);
        let request = serde_json::json!({"branch": branch});
        let (status, body) = self.router.call(&self.agent, "POST", &uri, request).await;
        assert!(status < 300, "{status} {body}");
        body["data"]["preview"]["id"].as_str().unwrap().to_owned()
    }
}

/// A token with previews permissions creates, deploys, reads, reads the logs
/// of, and deletes previews.
#[tokio::test]
async fn previews_permissions_manage_previews() {
    let f = PreviewFixture::new().await;
    let none = serde_json::Value::Null;
    let blog = &f.blog;
    let preview = f.preview("feat/one").await;
    for (method, uri) in [
        ("GET", format!("/api/v1/previews/{preview}")),
        ("GET", format!("/api/v1/environments/{preview}/deployments")),
        ("POST", format!("/api/v1/previews/{preview}/deploy")),
        ("GET", format!("/api/v1/applications/{blog}/previews")),
        ("GET", format!("/api/v1/applications/{blog}")),
    ] {
        let (status, body) = f.router.call(&f.agent, method, &uri, none.clone()).await;
        assert!(status < 300, "{method} {uri}: {status} {body}");
    }
    let logs = format!("/api/v1/environments/{preview}/logs?service=web&tail=5&since_seconds=60");
    let (status, body) = f.router.call(&f.agent, "GET", &logs, none.clone()).await;
    assert_eq!(status, 200, "{body}");
    let build = format!("/api/v1/builds/{}/logs", f.build(&preview).await);
    let (status, body) = f.router.call(&f.agent, "GET", &build, none.clone()).await;
    assert_eq!(status, 200, "{body}");
    let delete = format!("/api/v1/previews/{preview}");
    assert_eq!(f.router.status(&f.agent, "DELETE", &delete).await, 202);
}

/// A token with previews permissions changes no long-lived environment, reads
/// none of its logs, changes no shared configuration, and cannot delete the
/// application or run commands.
#[tokio::test]
async fn previews_permissions_reach_no_production_or_shared_state() {
    let f = PreviewFixture::new().await;
    let preview = f.preview("feat/one").await;
    let (blog, production) = (&f.blog, format!("/api/v1/environments/{}", f.production));
    let build = f.build(&f.production).await;
    for (method, uri, permission) in [
        ("GET", format!("/api/v1/builds/{build}/logs"), "logs:read"),
        ("GET", format!("{production}/logs"), "logs:read"),
        (
            "POST",
            format!("{production}/deploy?force=true"),
            "apps:deploy",
        ),
        ("DELETE", format!("{production}?force=true"), "apps:delete"),
        (
            "GET",
            format!("/api/v1/environments/{preview}/exec"),
            "apps:exec",
        ),
        (
            "DELETE",
            format!("/api/v1/applications/{blog}?force=true"),
            "apps:delete",
        ),
        (
            "PUT",
            format!("/api/v1/applications/{blog}/name?force=true"),
            "apps:write",
        ),
        (
            "PUT",
            format!("/api/v1/applications/{blog}/repository/sync?force=true"),
            "apps:write",
        ),
        (
            "POST",
            format!("/api/v1/applications/{blog}/environments?force=true"),
            "apps:write",
        ),
        (
            "PUT",
            format!("/api/v1/applications/{blog}/secrets/token/access"),
            "secrets:write",
        ),
    ] {
        let (status, body) = f
            .router
            .call(&f.agent, method, &uri, serde_json::json!({}))
            .await;
        assert_eq!(status, 403, "{method} {uri}: {body}");
        assert_eq!(body["details"]["permission"], permission, "{method} {uri}");
    }
}

/// Previews permissions are never handed on as whole-application access, and
/// a token holding them follows its owner's current access and its own
/// revocation.
#[tokio::test]
async fn previews_permissions_never_widen() {
    let f = PreviewFixture::new().await;
    let none = serde_json::Value::Null;
    let manage = "/api/v1/auth/manage";
    let invite =
        serde_json::json!({"action": "create_invitation", "grants": f.grants(&["apps:deploy"])});
    for wider in [f.token(&["apps:deploy"]), invite] {
        let (status, body) = f.router.call(&f.limited, "POST", manage, wider).await;
        assert_eq!(
            (status, body["code"].as_str()),
            (403, Some("permission_exceeded"))
        );
    }
    let narrower = f.token(&["previews:deploy"]);
    assert_eq!(
        f.router.call(&f.limited, "POST", manage, narrower).await.0,
        200
    );

    let preview = format!("/api/v1/previews/{}", f.preview("feat/two").await);
    execute(
        &f.database,
        "DELETE FROM auth_grants WHERE user_id='owner' AND permission='apps:delete'",
    )
    .await;
    let (status, body) = f
        .router
        .call(&f.agent, "DELETE", &preview, none.clone())
        .await;
    assert_eq!(
        (status, body["details"]["permission"].as_str()),
        (403, Some("previews:delete"))
    );

    let (_, directory) = f
        .router
        .call(&f.owner, "GET", "/api/v1/auth/directory", none.clone())
        .await;
    let id = directory["credentials"]
        .as_array()
        .unwrap()
        .iter()
        .find(|credential| credential["name"] == "agent" && credential["user_id"] == "owner")
        .unwrap()["id"]
        .clone();
    let revoke = serde_json::json!({"action": "revoke_credential", "id": id});
    assert_eq!(f.router.call(&f.owner, "POST", manage, revoke).await.0, 200);
    assert_eq!(f.router.status(&f.agent, "GET", &preview).await, 401);
}

/// Granting telemetry takes `telemetry:grant`, which `apps:write` does not
/// include, and reading the application, which stays hidden otherwise.
#[tokio::test]
async fn telemetry_grants_need_the_telemetry_grant_permission() {
    use piqueld::api::{Actor, ApplicationError, Mutation, MutationResponse};
    use piqueld::store::{Caller, StoreError};
    use piqueld_core::access::Denied;
    let temp = tempfile::tempdir().unwrap();
    let state = state(&temp).await;
    let database = database(&temp);
    let saved = save(&state, manifest()).await;
    let id = piqueld_core::EnvironmentId::parse(saved.application_id).unwrap();
    let account = |name| {
        Actor::Account(Caller {
            credential_id: name,
            user_id: name,
        })
    };
    seed_account(&database, "writer", &[("apps:write", None)]).await;
    seed_account(&database, "blind", &[("telemetry:grant", None)]).await;
    let grants = [("telemetry:grant", None), ("apps:read", None)];
    seed_account(&database, "granter", &grants).await;
    let grant = |actor| {
        let mutation = Mutation::SetTelemetry {
            id: id.clone(),
            granted: true,
        };
        state.accept(actor, mutation, None, false, None)
    };
    assert!(matches!(
        grant(account("writer")).await,
        Err(ApplicationError::Store(StoreError::Denied(
            Denied::Missing(_)
        )))
    ));
    assert!(matches!(
        grant(account("blind")).await,
        Err(ApplicationError::Store(StoreError::Denied(Denied::Hidden)))
    ));
    let Ok(MutationResponse::Environment(environment)) = grant(account("granter")).await else {
        panic!("granted");
    };
    assert!(environment.telemetry);
}
