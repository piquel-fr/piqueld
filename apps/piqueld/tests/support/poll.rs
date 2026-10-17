//! Waiting for work that finishes in the background.

use std::time::Duration;

/// Runs `check` every `interval` until it returns `Some`, giving up with
/// `None` once `timeout` passed.
pub async fn eventually<T>(
    timeout: Duration,
    interval: Duration,
    mut check: impl AsyncFnMut() -> Option<T>,
) -> Option<T> {
    tokio::time::timeout(timeout, async {
        loop {
            if let Some(value) = check().await {
                return value;
            }
            tokio::time::sleep(interval).await;
        }
    })
    .await
    .ok()
}
