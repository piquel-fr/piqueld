#![allow(missing_docs)]

use axum::{Json, Router, routing::get};
use piqueld_client::{Client, Envelope, SystemStatus};
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
    let path = temp.path().join("piqueld.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app()).await.unwrap() });
    let response = Client::unix(&path).system_status().await.unwrap();
    assert_eq!(response.status, "running");
    server.abort();
}
