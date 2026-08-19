//! Transport integration tests for the typed client.

use axum::{Json, Router, routing::get};
use piqueld_client::{Client, ClientError, Envelope, SystemStatus};
use std::{path::PathBuf, time::Duration};
use tempfile::TempDir;
use tokio::net::{TcpListener, UnixListener};

async fn status() -> Json<Envelope<SystemStatus>> {
    Json(Envelope {
        data: SystemStatus {
            status: "running".into(),
            api_version: "v1".into(),
            instance_id: "instance-test".into(),
        },
    })
}

fn app() -> Router {
    Router::new().route("/api/v1/system/status", get(status))
}

#[tokio::test]
async fn typed_client_uses_tcp() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app()).await.unwrap() });
    let response = Client::tcp(&format!("http://{address}/"))
        .unwrap()
        .system_status()
        .await
        .unwrap();
    assert_eq!(response.instance_id, "instance-test");
    server.abort();
}

#[tokio::test]
async fn typed_client_uses_unix_socket() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join(PathBuf::from("piqueld.sock"));
    let listener = UnixListener::bind(&path).unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app()).await.unwrap() });
    let response = Client::unix(&path).system_status().await.unwrap();
    assert_eq!(response.status, "running");
    server.abort();
}

#[tokio::test]
async fn request_timeout_is_reported_by_the_client() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let client = Client::tcp(&format!("http://{address}/"))
        .unwrap()
        .with_timeout(Duration::from_millis(1));
    assert!(matches!(
        client.system_status().await,
        Err(ClientError::Transport { .. })
    ));
}

#[test]
fn tcp_transport_rejects_non_loopback_endpoints() {
    assert!(matches!(
        Client::tcp("http://example.com:8080/"),
        Err(ClientError::Endpoint)
    ));
    assert!(Client::tcp("http://localhost:8080/").is_ok());
}
