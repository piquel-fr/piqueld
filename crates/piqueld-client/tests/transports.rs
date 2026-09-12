//! Transport integration tests for the typed client.

#![cfg(not(target_arch = "wasm32"))]

use axum::{
    Json, Router,
    extract::Path,
    http::HeaderMap,
    routing::{get, put},
};
use piqueld_client::{
    AcceptedOperation, Client, ClientError, Envelope, ListApplicationsOptions, SystemStatus,
};
use std::{path::PathBuf, sync::Mutex, time::Duration};
use tempfile::TempDir;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, UnixListener},
    sync::mpsc,
    time::timeout,
};

async fn status() -> Json<Envelope<SystemStatus>> {
    Json(Envelope {
        data: SystemStatus {
            status: "running".into(),
            api_version: "v1".into(),
            daemon_version: "0.1.0".into(),
            instance_id: "instance-test".into(),
        },
    })
}

#[derive(Clone, Debug)]
struct CapturedRequest {
    id: String,
    content_type: Option<String>,
    expected_generation: Option<String>,
    idempotency_key: Option<String>,
    body: String,
}

static CAPTURED: Mutex<Option<CapturedRequest>> = Mutex::new(None);

fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

async fn capture_toml_replace(
    Path(id): Path<String>,
    headers: HeaderMap,
    body: String,
) -> Json<Envelope<AcceptedOperation>> {
    *CAPTURED.lock().unwrap() = Some(CapturedRequest {
        id,
        content_type: header_value(&headers, "content-type"),
        expected_generation: header_value(&headers, "x-expected-generation"),
        idempotency_key: header_value(&headers, "idempotency-key"),
        body,
    });
    Json(Envelope {
        data: AcceptedOperation {
            operation_id: "op-1".into(),
            application_id: "app-1".into(),
            generation: 7,
        },
    })
}

fn app() -> Router {
    Router::new()
        .route("/api/v1/system/status", get(status))
        .route("/api/v1/applications/{id}", put(capture_toml_replace))
}

/// Answers one connection with a hand-written HTTP response.
async fn serve_raw(listener: TcpListener, response: Vec<u8>) {
    let (mut socket, _) = listener.accept().await.unwrap();
    let mut buffer = [0u8; 2048];
    let _ = socket.read(&mut buffer).await;
    let _ = socket.write_all(&response).await;
}

fn raw_response(status_line: &str, content_type: &str, body: &str) -> Vec<u8> {
    format!(
        "{status_line}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
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
    assert_eq!(response.status, "running");
    assert_eq!(response.api_version, "v1");
    assert_eq!(response.daemon_version, "0.1.0");
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
    assert_eq!(response.daemon_version, "0.1.0");
    server.abort();
}

#[tokio::test]
async fn requests_use_origin_form_with_an_explicit_host_header() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (wire_tx, mut wire_rx) = mpsc::channel(1);
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = Vec::new();
        loop {
            let mut chunk = [0u8; 1024];
            let read = socket.read(&mut chunk).await.unwrap();
            assert_ne!(read, 0, "client closed before sending request headers");
            buffer.extend_from_slice(&chunk[..read]);
            if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        wire_tx
            .send(String::from_utf8_lossy(&buffer).into_owned())
            .await
            .unwrap();
        let body = r#"{"data":{"status":"running","api_version":"v1","daemon_version":"0.1.0","instance_id":"i"}}"#;
        let _ = socket
            .write_all(&raw_response("HTTP/1.1 200 OK", "application/json", body))
            .await;
    });
    let response = Client::tcp(&format!("http://{address}/"))
        .unwrap()
        .system_status()
        .await
        .unwrap();
    server.await.unwrap();
    let wire = wire_rx.recv().await.unwrap();
    assert!(wire.starts_with("GET /api/v1/system/status HTTP/1.1\r\n"));
    let host = wire
        .lines()
        .filter_map(|line| line.split_once(':'))
        .find_map(|(name, value)| name.eq_ignore_ascii_case("host").then(|| value.trim()));
    let expected_host = address.to_string();
    assert_eq!(host, Some(expected_host.as_str()));
    assert!(!wire.contains("http://"), "absolute-form leaked: {wire}");
    assert_eq!(response.daemon_version, "0.1.0");
}

#[tokio::test]
async fn request_timeout_is_reported_by_the_client() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let client = Client::tcp(&format!("http://{address}/"))
        .unwrap()
        .with_timeout(Duration::from_millis(1));
    let result = client.system_status().await;
    assert!(matches!(
        result,
        Err(ClientError::Transport { message }) if message == "request timed out"
    ));
}

#[tokio::test]
async fn incomplete_response_bodies_time_out() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let (_, mut writer) = tokio::io::split(socket);
        writer
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 64\r\n\r\nshort")
            .await
            .unwrap();
        std::future::pending::<()>().await;
    });
    let result = Client::tcp(&format!("http://{address}/"))
        .unwrap()
        .with_timeout(Duration::from_millis(50))
        .system_status()
        .await;
    assert!(matches!(
        result,
        Err(ClientError::Transport { message }) if message == "request timed out"
    ));
    server.abort();
}

#[tokio::test]
async fn unix_socket_stalled_response_bodies_time_out() {
    let temp = TempDir::new().unwrap();
    let path = temp.path().join("stalled.sock");
    let listener = UnixListener::bind(&path).unwrap();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let (_, mut writer) = tokio::io::split(socket);
        writer
            .write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 64\r\n\r\nshort")
            .await
            .unwrap();
        std::future::pending::<()>().await;
    });
    let result = Client::unix(&path)
        .with_timeout(Duration::from_millis(50))
        .system_status()
        .await;
    assert!(matches!(
        result,
        Err(ClientError::Transport { message }) if message == "request timed out"
    ));
    server.abort();
}

#[tokio::test]
async fn oversized_response_bodies_are_rejected() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let total = 16 * 1024 * 1024 + 1;
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = [0u8; 2048];
        let _ = socket.read(&mut buffer).await;
        let head = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {total}\r\n\r\n"
        );
        if socket.write_all(head.as_bytes()).await.is_err() {
            return;
        }
        let chunk = vec![0u8; 64 * 1024];
        for _ in 0..=total / chunk.len() {
            if socket.write_all(&chunk).await.is_err() {
                return;
            }
        }
        std::future::pending::<()>().await;
    });
    let result = Client::tcp(&format!("http://{address}/"))
        .unwrap()
        .with_timeout(Duration::from_secs(15))
        .system_status()
        .await;
    server.abort();
    match result {
        Err(ClientError::Transport { message }) => {
            assert!(message.contains("exceeded"), "unexpected error: {message}");
        }
        other => panic!("expected a body-size transport error, got {other:?}"),
    }
}

#[tokio::test]
async fn malformed_success_envelopes_decode_as_errors() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(serve_raw(
        listener,
        raw_response(
            "HTTP/1.1 200 OK",
            "application/json",
            r#"{"unexpected":true}"#,
        ),
    ));
    let result = Client::tcp(&format!("http://{address}/"))
        .unwrap()
        .system_status()
        .await;
    server.await.unwrap();
    let Err(ClientError::Decode { source }) = result else {
        panic!("malformed success envelope must preserve its decoder error");
    };
    assert!(source.line() > 0);
    assert!(source.column() > 0);
}

#[tokio::test]
async fn unreadable_error_bodies_fall_back_to_invalid_error_response() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(serve_raw(
        listener,
        raw_response(
            "HTTP/1.1 500 Internal Server Error",
            "text/html",
            "<html>boom</html>",
        ),
    ));
    let result = Client::tcp(&format!("http://{address}/"))
        .unwrap()
        .system_status()
        .await;
    server.await.unwrap();
    match result {
        Err(ClientError::Api { status, error }) => {
            assert_eq!(status, http::StatusCode::INTERNAL_SERVER_ERROR);
            assert_eq!(error.code, "invalid_error_response");
        }
        other => panic!("expected an API error, got {other:?}"),
    }
}

#[tokio::test]
async fn structured_api_errors_expose_status_code_and_message() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let body = r#"{"code":"application_generation_conflict","message":"the application was modified by another client","details":{"current_generation":4},"request_id":"req-1"}"#;
    let server = tokio::spawn(serve_raw(
        listener,
        raw_response("HTTP/1.1 409 Conflict", "application/json", body),
    ));
    let result = Client::tcp(&format!("http://{address}/"))
        .unwrap()
        .system_status()
        .await;
    server.await.unwrap();
    match result {
        Err(ClientError::Api { status, error }) => {
            assert_eq!(status, http::StatusCode::CONFLICT);
            assert_eq!(error.code, "application_generation_conflict");
            assert_eq!(
                error.message,
                "the application was modified by another client"
            );
            assert_eq!(error.request_id, "req-1");
        }
        other => panic!("expected an API error, got {other:?}"),
    }
}

#[tokio::test]
async fn redirects_are_not_followed() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let decoy = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let redirect = format!(
        "HTTP/1.1 302 Found\r\nlocation: http://{}/api/v1/system/status\r\ncontent-length: 0\r\nconnection: close\r\n\r\n",
        decoy.local_addr().unwrap()
    )
    .into_bytes();
    let server = tokio::spawn(serve_raw(listener, redirect));
    let result = Client::tcp(&format!("http://{address}/"))
        .unwrap()
        .system_status()
        .await;
    server.await.unwrap();
    assert!(matches!(
        result,
        Err(ClientError::Api { status, .. }) if status == http::StatusCode::FOUND
    ));
    assert!(
        timeout(Duration::from_millis(100), decoy.accept())
            .await
            .is_err(),
        "the redirect target must never be contacted"
    );
}

#[tokio::test]
async fn toml_mutation_headers_are_forwarded() {
    *CAPTURED.lock().unwrap() = None;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app()).await.unwrap() });
    let manifest = "name = 'my-app'\n";
    let accepted = Client::tcp(&format!("http://{address}/"))
        .unwrap()
        .replace_application_toml_with_key("my-app", manifest, 7, Some("key-123"))
        .await
        .unwrap();
    server.abort();
    assert_eq!(accepted.generation, 7);
    let captured = CAPTURED
        .lock()
        .unwrap()
        .clone()
        .expect("the mutation request must be captured");
    assert_eq!(captured.id, "my-app");
    assert_eq!(captured.content_type.as_deref(), Some("application/toml"));
    assert_eq!(captured.expected_generation.as_deref(), Some("7"));
    assert_eq!(captured.idempotency_key.as_deref(), Some("key-123"));
    assert_eq!(captured.body, manifest);
}

#[tokio::test]
async fn application_queries_are_percent_encoded() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (target_tx, mut target_rx) = mpsc::channel(1);
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = Vec::new();
        loop {
            let mut chunk = [0u8; 1024];
            let read = socket.read(&mut chunk).await.unwrap();
            assert_ne!(read, 0, "client closed before sending request headers");
            buffer.extend_from_slice(&chunk[..read]);
            if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        let request = String::from_utf8_lossy(&buffer).into_owned();
        let target = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or_default()
            .to_owned();
        target_tx.send(target).await.unwrap();
        let _ = socket
            .write_all(&raw_response(
                "HTTP/1.1 200 OK",
                "application/json",
                r#"{"data":{"items":[],"next_cursor":null}}"#,
            ))
            .await;
    });
    let page = Client::tcp(&format!("http://{address}/"))
        .unwrap()
        .applications_with(&ListApplicationsOptions {
            cursor: Some("a&b c".into()),
            limit: Some(3),
        })
        .await
        .unwrap();
    server.await.unwrap();
    assert_eq!(
        target_rx.recv().await.unwrap(),
        "/api/v1/applications?cursor=a%26b+c&limit=3"
    );
    assert!(page.items.is_empty());
    assert!(page.next_cursor.is_none());
}

#[test]
fn tcp_transport_rejects_non_loopback_endpoints() {
    for rejected in [
        "http://example.com:8080/",
        "http://192.168.1.10:8080/",
        "http://10.0.0.1/",
        "http://0.0.0.0/",
        "http://[::ffff:127.0.0.1]:8080/",
        "https://127.0.0.1/",
        "http://user@localhost/",
        "http://:secret@localhost/",
        "http://:@localhost/",
        "http://localhost/app",
        "http://localhost/?x=1",
    ] {
        assert!(
            matches!(Client::tcp(rejected), Err(ClientError::Endpoint { .. })),
            "expected {rejected} to be rejected"
        );
    }
    for accepted in [
        "http://localhost:8080/",
        "http://LOCALHOST/",
        "http://127.0.0.1/",
        "http://127.1/",
        "http://[::1]:8080/",
    ] {
        assert!(
            Client::tcp(accepted).is_ok(),
            "expected {accepted} to be accepted"
        );
    }
}
