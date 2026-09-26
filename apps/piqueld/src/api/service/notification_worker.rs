//! Background observation and webhook delivery. Neither executes infrastructure mutations.
use super::ApplicationService;
use crate::{
    config::WebhookKind,
    store::{StoreError, now_ms},
};
use piqueld_core::observability::DeliveryState;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

impl ApplicationService {
    pub(super) async fn observe_notifications(
        &self,
        scan_seconds: u64,
        cancellation: CancellationToken,
    ) {
        let mut tick = tokio::time::interval(Duration::from_secs(15));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {()=cancellation.cancelled()=>return,_=tick.tick()=>{}}
            let result = async {
                let readiness = self.readiness().await;
                let now = now_ms();
                let docker_ready =
                    matches!(readiness.docker, piqueld_core::api::DependencyStatus::Ready);
                for (key, failed) in [
                    ("docker_unavailable", !docker_ready),
                    (
                        "swarm_manager_unavailable",
                        !matches!(readiness.swarm, piqueld_core::api::DependencyStatus::Ready),
                    ),
                ] {
                    // Swarm cannot be observed while its Engine is unreachable.
                    if key == "swarm_manager_unavailable" && !docker_ready {
                        continue;
                    }
                    self.store
                        .observe_condition(key, None, failed, now, 45_000)
                        .await?;
                }
                self.store
                    .observe_services(
                        i64::try_from(scan_seconds.max(15).saturating_mul(3000))
                            .unwrap_or(i64::MAX),
                    )
                    .await?;
                self.store.daemon_stats().await?;
                Ok::<_, StoreError>(())
            }
            .await;
            if let Err(error) = result {
                tracing::error!(error=?error,"observability sampling failed");
            }
        }
    }
    pub(super) async fn deliver_notifications(
        &self,
        client: reqwest::Client,
        cancellation: CancellationToken,
    ) {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            let result = tokio::select! {
                () = cancellation.cancelled() => return,
                _ = tick.tick() => tokio::select! {
                    () = cancellation.cancelled() => return,
                    result = self.deliver_next_notification(&client) => result,
                },
            };
            if let Err(error) = result {
                tracing::error!(error=?error, "notification journal processing failed");
            }
        }
    }

    async fn deliver_next_notification(&self, client: &reqwest::Client) -> Result<(), StoreError> {
        self.store.process_notifications().await?;
        let Some((delivery, destination)) = self.store.claim_delivery().await? else {
            return Ok(());
        };
        let event = match self.store.event(delivery.event_id).await {
            Ok(event) => event,
            Err(StoreError::NotFound) => return Ok(()), // Application deletion removed the outbox row too.
            Err(error) => return Err(error),
        };
        let payload = match destination.kind {
            WebhookKind::Json => serde_json::json!({
                "version": 1,
                "instance_id": self.store.instance_id(),
                "delivery_id": delivery.id,
                "category": delivery.category,
                "event": event,
            }),
            WebhookKind::Discord => {
                let summary = event.message.as_deref().unwrap_or(&event.kind);
                let application = event
                    .application_id
                    .as_ref()
                    .map_or_else(|| "daemon".to_owned(), ToString::to_string);
                let diagnostic = event.diagnostic.as_ref().map_or_else(String::new, |d| {
                    format!("\nDiagnostic: {}\n{}", d.id, d.next_action)
                });
                let content = format!(
                    "piqueld · {}\n{summary}\nApplication: {application}\nEvent: {} · Delivery: {}{diagnostic}",
                    delivery.category, event.id, delivery.id,
                );
                serde_json::json!({
                    "content": content.chars().take(1900).collect::<String>(),
                    "allowed_mentions": {"parse": []},
                })
            }
        };
        let result = client
            .post(&destination.url)
            .header("Idempotency-Key", &delivery.id)
            .json(&payload)
            .send()
            .await;
        let exponent = u32::try_from(delivery.attempts.saturating_sub(1).min(10)).unwrap_or(10);
        let delay = 5_u64.saturating_mul(1_u64 << exponent).min(3600);
        let (state, message, delay) = match result {
            Ok(response) if response.status().is_success() => (DeliveryState::Delivered, None, 0),
            Ok(response) => {
                let status = response.status();
                let retry = status.is_server_error()
                    || status == reqwest::StatusCode::TOO_MANY_REQUESTS
                    || status == reqwest::StatusCode::REQUEST_TIMEOUT;
                let delay = response
                    .headers()
                    .get("retry-after")
                    .and_then(|value| value.to_str().ok())
                    .and_then(|value| value.parse::<u64>().ok())
                    .map_or(delay, |value| value.clamp(1, 3600));
                let state = if retry {
                    DeliveryState::Pending
                } else {
                    DeliveryState::Failed
                };
                (
                    state,
                    Some(format!("Receiver returned HTTP {status}")),
                    delay,
                )
            }
            Err(error) => {
                let message = if error.is_timeout() {
                    "Webhook request timed out"
                } else if error.is_connect() {
                    "Could not connect to webhook receiver"
                } else {
                    "Webhook transport failed"
                };
                (DeliveryState::Pending, Some(message.to_owned()), delay)
            }
        };
        self.store
            .complete_delivery(&delivery.id, state, message.as_deref(), delay)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{DaemonConfig, WebhookDestination, WebhookKind},
        docker::BollardDocker,
        reconcile::Controller,
        store::Store,
    };
    use std::{
        future::IntoFuture,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };

    #[tokio::test]
    async fn webhook_retries_preserve_identity_and_hide_receiver_details() {
        let requests = Arc::new(tokio::sync::Mutex::new(Vec::new()));
        let count = Arc::new(AtomicUsize::new(0));
        let seen = requests.clone();
        let receiver = axum::Router::new().route(
            "/secret",
            axum::routing::post(
                move |headers: axum::http::HeaderMap,
                      axum::Json(body): axum::Json<serde_json::Value>| {
                    let requests = seen.clone();
                    let count = count.clone();
                    async move {
                        requests.lock().await.push((
                            headers["idempotency-key"].to_str().unwrap().to_owned(),
                            body,
                        ));
                        let status = if count.fetch_add(1, Ordering::SeqCst) == 0 {
                            axum::http::StatusCode::SERVICE_UNAVAILABLE
                        } else {
                            axum::http::StatusCode::OK
                        };
                        (status, [("retry-after", "1")], "private-receiver-detail")
                    }
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(axum::serve(listener, receiver).into_future());
        let temp = tempfile::tempdir().unwrap();
        let mut config = DaemonConfig::default();
        config.notifications.enabled = true;
        config.notifications.destinations.push(WebhookDestination {
            name: "test".into(),
            url: format!("http://{address}/secret"),
            enabled: true,
            kind: WebhookKind::Json,
        });
        let store = Arc::new(
            Store::open(temp.path().join("db"))
                .await
                .unwrap()
                .with_observability(&config),
        );
        store.configure_deliveries().await.unwrap();
        store
            .record_diagnostic(
                &piqueld_core::observability::Diagnostic::new(
                    "diagnostic-test".into(),
                    "internal_error",
                    "Test failure".into(),
                ),
                None,
                None,
            )
            .await
            .unwrap();
        // The sender never calls this runtime; only the socket existence check is needed.
        let _socket = tokio::net::UnixListener::bind(temp.path().join("unused.sock")).unwrap();
        let runtime = Controller::new(
            Arc::new(BollardDocker::connect(&temp.path().join("unused.sock")).unwrap()),
            store.clone(),
        )
        .runtime(Arc::new(tokio::sync::Notify::new()));
        let service = ApplicationService::new(store.clone(), runtime);
        let cancel = CancellationToken::new();
        let worker_cancel = cancel.clone();
        let worker = tokio::spawn(async move {
            service
                .deliver_notifications(reqwest::Client::new(), worker_cancel)
                .await;
        });
        tokio::time::timeout(Duration::from_secs(8), async {
            loop {
                let deliveries = store.deliveries(None, 100).await.unwrap();
                if deliveries
                    .items
                    .first()
                    .is_some_and(|d| d.state == DeliveryState::Delivered)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
        })
        .await
        .unwrap();
        cancel.cancel();
        worker.await.unwrap();
        server.abort();
        let requests = requests.lock().await;
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0], requests[1]);
        assert_eq!(requests[0].1["version"], 1);
        assert_eq!(requests[0].1["delivery_id"], requests[0].0);
        let delivery = store.deliveries(None, 100).await.unwrap().items.remove(0);
        assert_eq!(delivery.attempts, 2);
        assert!(delivery.last_error.is_none());
        assert!(!format!("{delivery:?}").contains("private-receiver-detail"));
    }
}
