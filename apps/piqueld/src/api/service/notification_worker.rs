//! Background observation and webhook delivery, and test notifications.
//! Neither executes infrastructure mutations.
use super::{ApplicationError, ApplicationService};
use crate::store::{StoreError, now_ms};
use piqueld_core::{
    Event,
    observability::{
        DeliveryState, DestinationKind, DestinationName, DestinationTest, EventScope,
        NotificationCategory,
    },
};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Sends notifications. Requests time out after ten seconds and never follow
/// redirects, so a receiver cannot bounce them elsewhere.
#[derive(Clone)]
pub(super) struct Notifier(reqwest::Client);

impl Default for Notifier {
    fn default() -> Self {
        Self(
            reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none())
                .build()
                // As for `reqwest::Client::new`, only a missing TLS backend fails.
                .expect("webhook client initializes"),
        )
    }
}

/// The outcome of one notification request. Errors are safe summaries that
/// never include the receiver's response body.
enum Attempt {
    /// The receiver acknowledged it with a 2xx status.
    Delivered,
    /// It may succeed later: after a transport failure, or HTTP 408, 429 or
    /// 5xx, waiting `retry_after` seconds when the receiver says so.
    Retry {
        error: String,
        retry_after: Option<u64>,
    },
    /// The receiver refused it with another status.
    Rejected(String),
}

impl Notifier {
    /// Posts `payload` to `url`, with `idempotency_key` as `Idempotency-Key`
    /// so receivers can deduplicate retries. `Retry-After` is honoured as
    /// 1–3600 seconds.
    async fn post(&self, url: &str, idempotency_key: &str, payload: &serde_json::Value) -> Attempt {
        let response = self
            .0
            .post(url)
            .header("Idempotency-Key", idempotency_key)
            .json(payload)
            .send()
            .await;
        match response {
            Ok(response) if response.status().is_success() => Attempt::Delivered,
            Ok(response) => {
                let status = response.status();
                let error = format!("Receiver returned HTTP {status}");
                if status.is_server_error()
                    || status == reqwest::StatusCode::TOO_MANY_REQUESTS
                    || status == reqwest::StatusCode::REQUEST_TIMEOUT
                {
                    let retry_after = response
                        .headers()
                        .get("retry-after")
                        .and_then(|value| value.to_str().ok())
                        .and_then(|value| value.parse::<u64>().ok())
                        .map(|value| value.clamp(1, 3600));
                    Attempt::Retry { error, retry_after }
                } else {
                    Attempt::Rejected(error)
                }
            }
            Err(error) => {
                let error = if error.is_timeout() {
                    "Webhook request timed out"
                } else if error.is_connect() {
                    "Could not connect to webhook receiver"
                } else {
                    "Webhook transport failed"
                };
                Attempt::Retry {
                    error: error.to_owned(),
                    retry_after: None,
                }
            }
        }
    }
}

impl ApplicationService {
    /// Samples dependency and service health every 15 seconds until cancelled.
    ///
    /// Each tick feeds Docker and Swarm readiness and per-application runtime
    /// health into the store's condition tracking (which emits failure and
    /// recovery events), then refreshes the cached daemon statistics. Sampling
    /// errors are logged and the loop continues.
    pub(super) async fn observe_notifications(
        &self,
        scan_seconds: u64,
        cancellation: CancellationToken,
    ) {
        let mut tick = tokio::time::interval(Duration::from_secs(15));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                () = cancellation.cancelled() => return,
                _ = tick.tick() => {}
            }
            let result = async {
                let readiness = self.readiness(&piqueld_core::access::Scope::All).await;
                let now = now_ms();
                let ready = |status| matches!(status, piqueld_core::api::DependencyStatus::Ready);
                let docker_ready = ready(readiness.docker);
                self.store
                    .observe_condition("docker_unavailable", None, !docker_ready, now, 45_000)
                    .await?;
                // Swarm cannot be observed while its Engine is unreachable.
                if docker_ready {
                    let failed = !ready(readiness.swarm);
                    self.store
                        .observe_condition("swarm_manager_unavailable", None, failed, now, 45_000)
                        .await?;
                }
                // Service health older than three scan intervals is treated as stale.
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
                tracing::error!(?error, "observability sampling failed");
            }
        }
    }
    /// Attempts webhook deliveries one at a time until cancelled, claiming
    /// the next as soon as one is attempted, and checking again every second
    /// while none is due. Errors are logged and never stop the worker.
    pub(super) async fn deliver_notifications(&self, cancellation: CancellationToken) {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut idle = true;
        loop {
            if idle {
                tokio::select! {
                    () = cancellation.cancelled() => return,
                    _ = tick.tick() => {}
                }
            }
            let result = tokio::select! {
                () = cancellation.cancelled() => return,
                result = self.deliver_next_notification() => result,
            };
            // A failure waits for the next tick, like an empty outbox.
            idle = !result.unwrap_or_else(|error| {
                tracing::error!(error=?error, "notification delivery failed");
                false
            });
        }
    }

    /// Ingests new events into the outbox, then claims and sends one due
    /// delivery. Returns whether one was due.
    ///
    /// A delivery that can be retried stays pending, and is retried after
    /// the receiver's `Retry-After`, or with exponential backoff (5 s
    /// doubling, at most an hour); one the receiver rejects fails for good.
    async fn deliver_next_notification(&self) -> Result<bool, StoreError> {
        // Queued deliveries, retries and recoveries must not wait on new-event ingestion.
        if let Err(error) = self.store.process_notifications().await {
            tracing::error!(error=?error, "notification event processing failed");
        }
        let Some((delivery, destination)) = self.store.claim_delivery().await? else {
            return Ok(false);
        };
        let event = match self.store.event(delivery.event_id).await {
            Ok(event) => event,
            Err(StoreError::NotFound) => return Ok(true), // Application deletion removed the outbox row too.
            Err(error) => return Err(error),
        };
        // Backoff: 5 s, 10 s, 20 s, ... capped at one hour.
        let exponent = u32::try_from(delivery.attempts.saturating_sub(1).min(10)).unwrap_or(10);
        let backoff = 5_u64.saturating_mul(1_u64 << exponent).min(3600);
        let url = match self.store.destination_url(&destination) {
            Ok(url) => url,
            Err(error) => {
                let message = "The destination URL could not be decrypted";
                self.store
                    .complete_delivery(&delivery.id, DeliveryState::Pending, Some(message), backoff)
                    .await?;
                return Err(error);
            }
        };
        let payload = self.payload(
            destination.kind,
            &delivery.id,
            Some(delivery.category),
            &event,
            destination.message.as_deref(),
        );
        let (state, message, delay) = match self.notifier.post(&url, &delivery.id, &payload).await {
            Attempt::Delivered => (DeliveryState::Delivered, None, 0),
            Attempt::Retry { error, retry_after } => (
                DeliveryState::Pending,
                Some(error),
                retry_after.unwrap_or(backoff),
            ),
            Attempt::Rejected(error) => (DeliveryState::Failed, Some(error), backoff),
        };
        self.store
            .complete_delivery(&delivery.id, state, message.as_deref(), delay)
            .await?;
        Ok(true)
    }

    /// Sends a test notification to the destination `name` right away,
    /// outside the outbox, and reports whether its receiver acknowledged it.
    ///
    /// # Errors
    /// Returns absence or secret storage errors.
    pub async fn test_notification_destination(
        &self,
        name: &DestinationName,
    ) -> Result<DestinationTest, ApplicationError> {
        let destination = self.store.notification_destination(name).await?;
        let url = self.store.destination_url(&destination)?;
        let event = Event {
            kind: "notification_test".into(),
            scope: EventScope::Daemon,
            message: Some(format!("Test notification for destination {name}")),
            created_at_ms: now_ms(),
            ..Event::default()
        };
        let id = format!("test-{}", uuid::Uuid::now_v7().simple());
        let payload = self.payload(destination.kind, &id, None, &event, None);
        Ok(match self.notifier.post(&url, &id, &payload).await {
            Attempt::Delivered => DestinationTest {
                delivered: true,
                error: None,
            },
            Attempt::Retry { error, .. } | Attempt::Rejected(error) => DestinationTest {
                delivered: false,
                error: Some(error),
            },
        })
    }

    /// What a destination of `kind` receives for `event`, with its
    /// category's `message`: the versioned JSON envelope, or a Discord message
    /// whose content is `message` and whose embed describes the event. Test
    /// notifications have no category.
    fn payload(
        &self,
        kind: DestinationKind,
        delivery: &str,
        category: Option<NotificationCategory>,
        event: &Event,
        message: Option<&str>,
    ) -> serde_json::Value {
        match kind {
            DestinationKind::Json => serde_json::json!({
                "version": 1,
                "instance_id": self.store.instance_id(),
                "delivery_id": delivery,
                "category": category,
                "message": message,
                "event": event,
            }),
            DestinationKind::Discord => {
                let summary = event.message.as_deref().unwrap_or(&event.kind);
                let application = event
                    .environment_id
                    .as_ref()
                    .map_or_else(|| "daemon".to_owned(), ToString::to_string);
                let diagnostic = event.diagnostic.as_ref().map_or_else(String::new, |d| {
                    format!("\nDiagnostic: {}\n{}", d.id, d.next_action)
                });
                let category = category.map_or("test", NotificationCategory::as_str);
                let text = format!(
                    "piqueld · {category}\n{summary}\nApplication: {application}\nEvent: {} · Delivery: {delivery}{diagnostic}",
                    event.id,
                );
                // Only the content pings: it is the category message alone,
                // the operator's own text. Mentions in embeds never notify.
                serde_json::json!({
                    "content": message.unwrap_or_default(),
                    "embeds": [{"description": text.chars().take(4000).collect::<String>()}],
                    "allowed_mentions": {"parse": ["users", "roles", "everyone"]},
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use piqueld_core::observability::DestinationSettings;
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
        let store = Arc::new(Store::open(temp.path().join("db")).await.unwrap());
        let url = format!("http://{address}/secret");
        store
            .add_destination(
                "test",
                &url,
                DestinationSettings::new(DestinationKind::Json),
            )
            .await;
        store
            .record_diagnostic(
                &piqueld_core::observability::Diagnostic::new(
                    "diagnostic-test".into(),
                    piqueld_core::observability::DiagnosticCode::InternalError,
                    "Test failure".into(),
                ),
                None,
                None,
                crate::store::Attribution::default(),
            )
            .await
            .unwrap();
        let service = ApplicationService::unconnected(store.clone(), temp.path());
        let cancel = CancellationToken::new();
        let worker_cancel = cancel.clone();
        let worker = tokio::spawn(async move {
            service.deliver_notifications(worker_cancel).await;
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
