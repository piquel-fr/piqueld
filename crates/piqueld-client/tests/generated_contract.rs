//! Wire-level checks for the generated bindings and shared transport adapter.
#![cfg(not(target_arch = "wasm32"))]

use axum::{
    Router,
    body::Bytes,
    http::{HeaderMap, Uri},
};
use piqueld_client::system::{DependencyStatus, ReadinessStatus};
use piqueld_client::{Client, ClientError, RenameApplicationRequest};
use tokio::{net::TcpListener, sync::mpsc, task::JoinHandle};

struct Request {
    uri: String,
    headers: HeaderMap,
    body: Bytes,
}

struct Server {
    client: Client,
    requests: mpsc::Receiver<Request>,
    task: JoinHandle<()>,
}

impl Server {
    async fn start(status: http::StatusCode, response: impl Into<Bytes>) -> Self {
        let response = response.into();
        let (tx, requests) = mpsc::channel(4);
        let app = Router::new().fallback(move |uri: Uri, headers: HeaderMap, body: Bytes| {
            let tx = tx.clone();
            let response = response.clone();
            async move {
                tx.send(Request {
                    uri: uri.to_string(),
                    headers,
                    body,
                })
                .await
                .unwrap();
                (status, response)
            }
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = Client::tcp(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            client,
            requests,
            task,
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

#[tokio::test]
async fn log_parameters_are_encoded_and_readiness_keeps_its_503_payload() {
    let mut server = Server::start(
        http::StatusCode::BAD_REQUEST,
        r#"{"code":"invalid_id","message":"invalid application"}"#,
    )
    .await;
    assert!(matches!(
        server
            .client
            .application_logs("a/b", Some("web & worker"), 20, 90)
            .await,
        Err(ClientError::Api { .. })
    ));
    let request = server.requests.recv().await.unwrap();
    assert_eq!(
        request.uri,
        "/api/v1/applications/a%2Fb/logs?service=web+%26+worker&since_seconds=90&tail=20"
    );

    let readiness = ReadinessStatus {
        ready: false,
        database: DependencyStatus::Ready,
        docker: DependencyStatus::Failed {
            message: "unavailable".into(),
        },
        swarm: DependencyStatus::Failed {
            message: "unavailable".into(),
        },
    };
    let body = serde_json::to_vec(&piqueld_client::Envelope { data: readiness }).unwrap();
    let server = Server::start(http::StatusCode::SERVICE_UNAVAILABLE, body).await;
    let readiness = server.client.system_readiness().await.unwrap();
    assert!(!readiness.ready);
    assert!(
        matches!(readiness.docker, DependencyStatus::Failed { message } if message == "unavailable")
    );
}

#[tokio::test]
async fn toml_adapter_preserves_headers_and_unsigned_revisions() {
    let mut server = Server::start(
        http::StatusCode::OK,
        r#"{"data":{"application_id":"app","generation":1,"operation_id":null}}"#,
    )
    .await;
    let client = server.client.clone().with_request_id("fallback-key");
    let manifest = "name = 'café'\n";
    let saved = client
        .apply_application_toml_with_preconditions(
            manifest,
            Some(u64::MAX),
            Some("inspected-app"),
            false,
            true,
        )
        .await
        .unwrap();
    assert_eq!(saved.application_id, "app");
    let request = server.requests.recv().await.unwrap();
    assert_eq!(request.uri, "/api/v1/applications/apply?deploy=true");
    assert_eq!(request.headers["content-type"], "application/toml");
    assert_eq!(
        request.headers["x-expected-generation"],
        u64::MAX.to_string()
    );
    assert_eq!(
        request.headers["x-expected-application-id"],
        "inspected-app"
    );
    assert_eq!(request.headers.get_all("idempotency-key").iter().count(), 1);
    assert_eq!(request.headers["idempotency-key"], "fallback-key");
    assert_eq!(request.body, manifest.as_bytes());
}

#[tokio::test]
async fn generated_json_mutation_encodes_paths_and_decodes_api_errors() {
    let mut server = Server::start(http::StatusCode::CONFLICT, r#"{"code":"generation_conflict","message":"stale revision","details":{"current":7},"request_id":"trace"}"#).await;
    let body = RenameApplicationRequest {
        name: "new-name".into(),
        expected_generation: Some(6),
    };
    let error = server
        .client
        .clone()
        .with_request_id("command-key")
        .rename_application("a/b ?é", &body)
        .await
        .unwrap_err();
    let ClientError::Api { status, error } = error else {
        panic!("expected structured API error")
    };
    assert_eq!(status, http::StatusCode::CONFLICT);
    assert_eq!(error.code, "generation_conflict");
    assert_eq!(error.details["current"], 7);
    assert_eq!(error.request_id, "trace");
    let request = server.requests.recv().await.unwrap();
    assert_eq!(request.uri, "/api/v1/applications/a%2Fb%20%3F%C3%A9/rename");
    assert_eq!(request.headers["content-type"], "application/json");
    assert_eq!(request.headers["idempotency-key"], "command-key");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&request.body).unwrap(),
        serde_json::to_value(body).unwrap()
    );
}

#[tokio::test]
async fn manifest_download_decodes_text_and_rejects_invalid_utf8() {
    let manifest = "name = 'café'\n";
    let mut server = Server::start(http::StatusCode::OK, manifest).await;
    assert_eq!(
        server.client.application_manifest("a/b").await.unwrap(),
        manifest
    );
    assert_eq!(
        server.requests.recv().await.unwrap().uri,
        "/api/v1/applications/a%2Fb/manifest"
    );
    let server = Server::start(http::StatusCode::OK, vec![0xff]).await;
    assert!(matches!(
        server.client.application_manifest("a/b").await,
        Err(ClientError::TextDecode { .. })
    ));
}
