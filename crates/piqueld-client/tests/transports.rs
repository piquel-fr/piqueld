#![allow(missing_docs)]

use axum::{
    Json, Router,
    extract::State,
    response::{Sse, sse::Event},
    routing::get,
};
use futures_util::stream;
use piqueld_client::{Client, Envelope, SystemStatus};
use std::{convert::Infallible, sync::Arc, time::Duration};
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

struct DisconnectGuard(Arc<std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>>);
impl Drop for DisconnectGuard {
    fn drop(&mut self) {
        if let Some(sender) = self.0.lock().unwrap().take() {
            let _ = sender.send(());
        }
    }
}

async fn events(
    State(signal): State<Arc<std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>>>,
) -> Sse<impl futures_util::Stream<Item = Result<Event, Infallible>>> {
    let stream = stream::unfold(
        (DisconnectGuard(signal), true),
        |(guard, first)| async move {
            if !first {
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
            Some((
                Ok(Event::default()
                    .id("current:one")
                    .event("operation")
                    .data("{}")),
                (guard, false),
            ))
        },
    );
    Sse::new(stream)
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

#[tokio::test]
async fn dropping_event_receiver_closes_the_stream() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (disconnected_tx, disconnected_rx) = tokio::sync::oneshot::channel();
    let signal = Arc::new(std::sync::Mutex::new(Some(disconnected_tx)));
    let router = Router::new()
        .route("/api/v1/operations/{id}/events", get(events))
        .with_state(signal);
    let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    let client = Client::tcp(&format!("http://{address}/")).unwrap();
    let mut receiver = client.watch_operation("operation-test", None);
    receiver.recv().await.unwrap().unwrap();
    drop(receiver);
    tokio::time::timeout(Duration::from_secs(2), disconnected_rx)
        .await
        .expect("server stream was not dropped")
        .unwrap();
    server.abort();
}
