//! Diagnostic history, daemon measurements and notification delivery controls.
use crate::{
    Client, ClientError, Event, Page,
    client::{generated_result, invalid_request},
};
pub use piqueld_core::observability::*;
impl Client {
    /// Reads a filtered page of durable history.
    /// # Errors
    /// Returns invalid page size, transport, decoding or API errors.
    pub async fn filtered_events(
        &self,
        filter: &EventFilter,
        cursor: Option<&str>,
        limit: u16,
    ) -> Result<Page<Event>, ClientError> {
        if !(1..=100).contains(&limit) {
            return Err(invalid_request("event limit must be between 1 and 100"));
        }
        generated_result(
            self.generated
                .list_events(
                    filter.action_id.as_deref(),
                    filter.application_id.as_deref(),
                    filter.attempt,
                    cursor,
                    filter.descending.then_some(true),
                    filter.error_code.as_deref(),
                    filter.errors_only.then_some(true),
                    filter.kind.as_deref(),
                    Some(i64::from(limit)),
                    filter.operation_id.as_deref(),
                    filter.scope.as_ref(),
                    filter.since_ms,
                    filter.until_ms,
                )
                .await,
        )
        .await
        .map(|r| r.data)
    }
    /// Finds the contextual event for a diagnostic occurrence.
    /// # Errors
    /// Returns transport, decoding, absence or API errors.
    pub async fn diagnostic(&self, id: &str) -> Result<Event, ClientError> {
        generated_result(self.generated.get_diagnostic(id).await)
            .await
            .map(|r| r.data)
    }
    /// Gets cached daemon resource usage and queue measurements.
    /// # Errors
    /// Returns transport, decoding or API errors.
    pub async fn daemon_stats(&self) -> Result<DaemonStats, ClientError> {
        generated_result(self.generated.system_resources().await)
            .await
            .map(|r| r.data)
    }
    /// Gets deployment statistics for a selected time interval.
    /// # Errors
    /// Returns transport, decoding or API errors.
    pub async fn deployment_analytics(
        &self,
        application: Option<&str>,
        since: Option<i64>,
        until: Option<i64>,
    ) -> Result<DeploymentAnalytics, ClientError> {
        generated_result(
            self.generated
                .deployment_analytics(application, since, until)
                .await,
        )
        .await
        .map(|r| r.data)
    }
    /// Gets one page of webhook delivery outcomes.
    /// # Errors
    /// Returns transport, decoding or API errors.
    pub async fn notification_deliveries(
        &self,
        cursor: Option<&str>,
    ) -> Result<Page<NotificationDelivery>, ClientError> {
        generated_result(
            self.generated
                .notification_deliveries(cursor, Some(50))
                .await,
        )
        .await
        .map(|r| r.data)
    }
    /// Retries a failed notification under the active destination policy.
    /// # Errors
    /// Returns transport, disabled destination, decoding or API errors.
    pub async fn retry_notification(&self, id: &str) -> Result<(), ClientError> {
        generated_result(self.generated.retry_notification_delivery(id).await)
            .await
            .map(|_| ())
    }
}
