//! Exercise real persistent TLS connections through Caddy while its config reloads.
use super::Scenario;
use axum::{body::Body, http::Request, response::Response};
use futures_util::stream;
use hyper::Method;
use hyper_util::rt::TokioIo;
use std::{convert::Infallible, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

impl Scenario {
    fn traffic_backend(&self) -> tokio::task::JoinSet<()> {
        let socket = self
            .directory
            .path()
            .join("ingress/control/test-backend.sock");
        let listener = tokio::net::UnixListener::bind(socket).unwrap();
        let mut servers = tokio::task::JoinSet::new();
        servers.spawn(async move {
            let mut connections = tokio::task::JoinSet::new();
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                connections.spawn(async move {
                    hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), hyper::service::service_fn(backend))
                        .with_upgrades()
                        .await
                        .unwrap();
                });
            }
        });
        servers
    }

    pub(super) async fn persistent_traffic_survives_reload(&self) {
        let _servers = self.traffic_backend();
        let original = self
            .gateway
            .caddy
            .json(Method::GET, "/config/", None)
            .await
            .unwrap();
        let mut configuration = original.clone();
        for route in configuration["apps"]["http"]["servers"]["https"]["routes"]
            .as_array_mut()
            .unwrap()
        {
            if route["handle"][0]["handler"] == "reverse_proxy" {
                route["handle"][0]["upstreams"][0]["dial"] =
                    "unix//control/test-backend.sock".into();
            }
        }
        self.gateway
            .caddy
            .json(Method::POST, "/load", Some(&configuration))
            .await
            .unwrap();
        let root = tokio::fs::read(
            self.directory
                .path()
                .join("ingress/data/caddy/pki/authorities/local/root.crt"),
        )
        .await
        .unwrap();
        let builder = || {
            reqwest::Client::builder()
                .no_proxy()
                .timeout(Duration::from_secs(15))
                .add_root_certificate(reqwest::Certificate::from_pem(&root).unwrap())
                .resolve("one.example.test", "127.0.0.1:443".parse().unwrap())
        };
        let http1 = builder().http1_only().build().unwrap();
        let http2 = builder().http2_prior_knowledge().build().unwrap();
        let url = self.url("one.example.test");
        let mut events = http2.get(format!("{url}events")).send().await.unwrap();
        assert_eq!(events.version(), reqwest::Version::HTTP_2);
        assert_eq!(events.headers()["content-type"], "text/event-stream");
        assert!(events.chunk().await.unwrap().is_some());
        let response = http1
            .get(format!("{url}websocket"))
            .header("Connection", "Upgrade")
            .header("Upgrade", "websocket")
            .header("Sec-WebSocket-Version", "13")
            .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==")
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), 101);
        assert_eq!(
            response.headers()["sec-websocket-accept"],
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
        let mut websocket = response.upgrade().await.unwrap();
        for revision in 0..3 {
            // Pooling stays enabled: these clients exercise idle HTTP/1 and HTTP/2
            // reuse, alongside an active HTTP/2 SSE stream and WebSocket.
            for (client, version) in [
                (&http1, reqwest::Version::HTTP_11),
                (&http2, reqwest::Version::HTTP_2),
            ] {
                let response = client.get(&url).send().await.unwrap();
                assert_eq!(response.version(), version);
                assert_eq!(response.text().await.unwrap(), "persistent backend");
            }
            configuration["apps"]["http"]["servers"]["http"]["routes"][0]["handle"][0]["headers"]
                ["X-Reload"] = serde_json::json!([revision.to_string()]);
            self.gateway
                .caddy
                .json(Method::POST, "/load", Some(&configuration))
                .await
                .unwrap();
            tokio::time::timeout(Duration::from_secs(3), async {
                // One masked, single-byte text frame is sufficient to prove the
                // same upgraded stream still carries traffic after each reload.
                websocket
                    .write_all(&[0x81, 0x81, 0, 0, 0, 0, b'x'])
                    .await
                    .unwrap();
                let mut echo = [0; 3];
                websocket.read_exact(&mut echo).await.unwrap();
                assert_eq!(echo, [0x81, 1, b'x']);
                assert!(events.chunk().await.unwrap().is_some());
            })
            .await
            .unwrap();
        }
        self.gateway
            .caddy
            .json(Method::POST, "/load", Some(&original))
            .await
            .unwrap();
    }
}

async fn backend(mut request: Request<hyper::body::Incoming>) -> Result<Response, Infallible> {
    let response = match request.uri().path() {
        "/events" => Response::builder()
            .header("Content-Type", "text/event-stream")
            .body(Body::from_stream(stream::unfold(
                0_u32,
                |sequence| async move {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    Some((
                        Ok::<_, Infallible>(format!("data: {sequence}\n\n")),
                        sequence + 1,
                    ))
                },
            )))
            .unwrap(),
        "/websocket" => {
            let upgrade = hyper::upgrade::on(&mut request);
            tokio::spawn(async move {
                let mut stream = TokioIo::new(upgrade.await.unwrap());
                let mut frame = [0; 7];
                while stream.read_exact(&mut frame).await.is_ok() {
                    assert_eq!(&frame[..6], &[0x81, 0x81, 0, 0, 0, 0]);
                    stream.write_all(&[0x81, 1, frame[6]]).await.unwrap();
                }
            });
            Response::builder()
                .status(101)
                .header("Connection", "Upgrade")
                .header("Upgrade", "websocket")
                .header("Sec-WebSocket-Accept", "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=")
                .body(Body::empty())
                .unwrap()
        }
        _ => Response::new(Body::from("persistent backend")),
    };
    Ok(response)
}
