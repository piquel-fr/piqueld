//! Focused API/client coverage for the polling application lifecycle.

#[path = "../support/poll.rs"]
mod poll;
#[path = "../support/repository.rs"]
mod repository;
#[path = "../support/sqlite.rs"]
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
use piqueld_client::{ApplyApplicationRequest, ApplyOptions, Client, ClientError};
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

/// Holds a fake runtime call once it starts, until released.
#[derive(Default)]
struct Gate {
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

struct FakeRuntime {
    /// Holds secret removal, when given.
    cleanup_gate: Option<Arc<Gate>>,
    /// Holds creating a command named `held`.
    exec_gate: Gate,
    instance: InstanceId,
    unavailable: AtomicBool,
}

impl FakeRuntime {
    /// A reachable runtime owned by `store`'s installation.
    fn new(store: &Store) -> Self {
        Self {
            cleanup_gate: None,
            exec_gate: Gate::default(),
            instance: store.instance_id().clone(),
            unavailable: AtomicBool::new(false),
        }
    }

    /// Resolves every image of `application` to a digest of `a`s and
    /// compiles it for `environment`, as preparing it does.
    fn resolve(
        &self,
        environment: &piqueld_core::EnvironmentId,
        application: &NormalizedApplication,
    ) -> Result<piqueld_core::ResolvedApplication, Vec<piqueld_core::resource::CompileError>> {
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
        compile_application(
            application,
            environment,
            self.instance.clone(),
            &ResolutionSet {
                sources,
                secret_names: BTreeMap::default(),
            },
        )
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
    /// `held` is created once [`Self::exec_gate`] releases it.
    async fn create_exec(
        &self,
        _environment: &piqueld_core::EnvironmentId,
        request: &piqueld_core::exec::ExecRequest,
    ) -> Result<Option<piqueld::docker::Exec>, BoundaryError> {
        if request.command.as_slice()[0] == "held" {
            self.exec_gate.started.notify_one();
            self.exec_gate.release.notified().await;
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
        self.resolve(environment, application)
            .map_err(BoundaryError::Compilation)
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
    (Arc::clone(&runtime), ApiState::new(store, runtime).unwrap())
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

/// The ID of the `production` environment of application `application`.
async fn production(state: &ApiState, application: &str) -> piqueld_core::EnvironmentId {
    let application = piqueld_core::ApplicationId::parse(application).unwrap();
    let view = state.application(&application).await.unwrap();
    view.environment("production").unwrap().id.clone()
}

/// [`production`], read through `client`.
async fn production_through(client: &Client, application: &str) -> String {
    let view = client.application(application).await.unwrap();
    view.environment("production").unwrap().id.to_string()
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
            Endpoint::Unix(path) => Client::unix(path).unwrap(),
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
    async fn deploy(&self) -> Deployed {
        self.client
            .apply_and_deploy(&new_application())
            .await
            .unwrap()
    }

    /// The same daemon, routed in process.
    fn router(&self) -> axum::Router {
        router(
            ApiState::new(Arc::clone(&self.store), self.runtime.clone()).unwrap(),
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
            .deployments(environment, None, None)
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

// Existing lifecycle scenarios explicitly request deployment; save-only behavior
// is exercised separately below.
trait DeployFixture {
    async fn apply_and_deploy(
        &self,
        request: &ApplyApplicationRequest,
    ) -> Result<Deployed, ClientError>;
    async fn apply_and_deploy_with_force(
        &self,
        request: &ApplyApplicationRequest,
        force: bool,
    ) -> Result<Deployed, ClientError>;
}
impl DeployFixture for Client {
    async fn apply_and_deploy(
        &self,
        request: &ApplyApplicationRequest,
    ) -> Result<Deployed, ClientError> {
        self.apply_and_deploy_with_force(request, false).await
    }
    async fn apply_and_deploy_with_force(
        &self,
        request: &ApplyApplicationRequest,
        force: bool,
    ) -> Result<Deployed, ClientError> {
        let saved = self
            .apply_application_with_options(
                request,
                &ApplyOptions {
                    force,
                    deploy: true,
                },
            )
            .await?;
        let operation_id = saved.operation_id.expect("deployment requested");
        Ok(Deployed {
            environment_id: self
                .operation(&operation_id)
                .await?
                .environment_id
                .to_string(),
            application_id: saved.application_id,
            generation: saved.generation,
            operation_id,
        })
    }
}

/// What [`DeployFixture`] accepted: the operation deploying an application's
/// sole environment.
#[derive(Debug)]
struct Deployed {
    application_id: String,
    environment_id: String,
    operation_id: String,
    generation: u64,
}

/// Two applications, `blog` and `shop`, served by the real authenticator, with
/// a `deployer` holding `apps:deploy` on `blog`, a `creator` holding
/// `apps:create` plus `apps:write` on `blog`, a `lead` holding only
/// `accounts:manage`, and an `auditor` holding only `audit:read`.
struct GrantFixture {
    temp: TempDir,
    runtime: Arc<FakeRuntime>,
    state: ApiState,
    auth: piqueld::auth::Auth,
    router: axum::Router,
    blog: String,
    shop: String,
    /// The `production` environments of `blog` and `shop`.
    blog_production: String,
    shop_production: String,
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
        let (runtime, state) = contract_state(&temp, Arc::clone(&store)).await;
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
        let blog_production = production(&state, &blog).await.to_string();
        let shop_production = production(&state, &shop).await.to_string();
        Self {
            temp,
            router: api_router(state.clone(), auth.clone()),
            runtime,
            state,
            auth,
            blog,
            shop,
            blog_production,
            shop_production,
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

mod acceptance;
mod applications;
mod authentication;
mod authorization;
mod dashboard;
mod environments;
mod field_edits;
mod openapi;
mod previews;
mod secrets;
mod streams;
mod transport;
