//! Diagnostic history, daemon measurements, notification destinations and
//! delivery controls.
use crate::{
    Client, ClientError, Event, Page,
    client::{EnvelopedCall, GeneratedCall, check_page_limit},
};
pub use piqueld_core::observability::*;
use std::{fmt, time::Duration};

/// A duration as people read it: `850 ms`, `3.2 s`, `4 min 12 s` or
/// `2 h 5 min`. The dashboard and `piquelctl` show durations this way.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Elapsed(pub Duration);

impl Elapsed {
    /// From whole milliseconds, such as a difference of timestamps; negative
    /// ones are zero.
    #[must_use]
    pub fn from_millis(milliseconds: i64) -> Self {
        Self(Duration::from_millis(
            u64::try_from(milliseconds).unwrap_or(0),
        ))
    }

    /// From fractional milliseconds, such as analytics means; negative,
    /// non-finite or unrepresentable ones are zero.
    #[must_use]
    pub fn from_millis_f64(milliseconds: f64) -> Self {
        Self(Duration::try_from_secs_f64(milliseconds / 1000.0).unwrap_or_default())
    }
}

impl fmt::Display for Elapsed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let milliseconds = self.0.as_millis();
        match milliseconds {
            0..=999 => write!(formatter, "{milliseconds} ms"),
            // Tenths of a second, rounded half up, below the 60.0 s that is
            // shown as a minute.
            1_000..=59_949 => {
                let tenths = (milliseconds + 50) / 100;
                write!(formatter, "{}.{} s", tenths / 10, tenths % 10)
            }
            59_950..=3_599_999 => {
                let seconds = (milliseconds + 500) / 1_000;
                write!(formatter, "{} min {} s", seconds / 60, seconds % 60)
            }
            _ => write!(
                formatter,
                "{} h {} min",
                milliseconds / 3_600_000,
                (milliseconds % 3_600_000) / 60_000
            ),
        }
    }
}

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
        check_page_limit("event", limit)?;
        self.generated
            .list_events(
                filter.action_id.as_deref(),
                filter.application_id.as_deref(),
                filter.attempt,
                cursor,
                filter.descending.then_some(true),
                filter.environment_id.as_deref(),
                filter.error_code.as_deref(),
                filter.errors_only.then_some(true),
                filter.kind.as_deref(),
                Some(i64::from(limit)),
                filter.operation_id.as_deref(),
                filter.scope.as_ref(),
                filter.since_ms,
                filter.until_ms,
            )
            .data()
            .await
    }
    /// Reads a page of audited API requests, newest first. Callers without
    /// `audit:read` see only their own account's requests.
    /// # Errors
    /// Returns invalid page size, transport, decoding or API errors.
    pub async fn audit_events(
        &self,
        filter: &piqueld_core::audit::AuditFilter,
        cursor: Option<&str>,
        limit: u16,
    ) -> Result<Page<piqueld_core::audit::AuditEvent>, ClientError> {
        check_page_limit("audit", limit)?;
        self.generated
            .list_audit(
                filter.credential_id.as_deref(),
                cursor,
                Some(i64::from(limit)),
                filter.outcome.as_ref(),
                filter.user_id.as_deref(),
                filter.username.as_deref(),
            )
            .data()
            .await
    }
    /// Verifies the audit trail's hash chain; requires `audit:read`.
    /// # Errors
    /// Returns transport, decoding or API errors.
    pub async fn verify_audit(
        &self,
    ) -> Result<piqueld_core::audit::AuditVerification, ClientError> {
        self.generated.verify_audit().data().await
    }
    /// Finds the contextual event for a diagnostic occurrence.
    /// # Errors
    /// Returns transport, decoding, absence or API errors.
    pub async fn diagnostic(&self, id: &str) -> Result<Event, ClientError> {
        self.generated.get_diagnostic(id).data().await
    }
    /// Gets cached daemon resource usage and queue measurements.
    /// # Errors
    /// Returns transport, decoding or API errors.
    pub async fn daemon_stats(&self) -> Result<DaemonStats, ClientError> {
        self.generated.system_resources().data().await
    }
    /// Gets deployment statistics for a selected time interval.
    /// # Errors
    /// Returns transport, decoding or API errors.
    pub async fn deployment_analytics(
        &self,
        environment: Option<&str>,
        since: Option<i64>,
        until: Option<i64>,
    ) -> Result<DeploymentAnalytics, ClientError> {
        self.generated
            .deployment_analytics(environment, since, until)
            .data()
            .await
    }
    /// Gets one page of webhook delivery outcomes.
    /// # Errors
    /// Returns transport, decoding or API errors.
    pub async fn notification_deliveries(
        &self,
        cursor: Option<&str>,
    ) -> Result<Page<NotificationDelivery>, ClientError> {
        self.generated
            .notification_deliveries(cursor, Some(i64::from(crate::DEFAULT_PAGE_SIZE)))
            .data()
            .await
    }
    /// Lists notification destinations, without their URLs.
    /// # Errors
    /// Returns transport, decoding or API errors.
    pub async fn notification_destinations(
        &self,
    ) -> Result<Vec<NotificationDestination>, ClientError> {
        self.generated.notification_destinations().data().await
    }
    /// Creates or replaces the destination `name`; see [`DestinationRequest`].
    /// # Errors
    /// Returns transport, invalid destination, decoding or API errors.
    pub async fn put_notification_destination(
        &self,
        name: &str,
        request: &DestinationRequest,
    ) -> Result<NotificationDestination, ClientError> {
        self.generated
            .put_notification_destination(name, request)
            .data()
            .await
    }
    /// Deletes the destination `name`, cancelling its pending deliveries.
    /// # Errors
    /// Returns transport, absence, decoding or API errors.
    pub async fn delete_notification_destination(&self, name: &str) -> Result<(), ClientError> {
        self.generated
            .delete_notification_destination(name)
            .payload()
            .await
            .map(|_| ())
    }
    /// Sends a test notification to the destination `name` at once.
    /// # Errors
    /// Returns transport, absence, decoding or API errors.
    pub async fn test_notification_destination(
        &self,
        name: &str,
    ) -> Result<DestinationTest, ClientError> {
        self.generated
            .test_notification_destination(name)
            .data()
            .await
    }
    /// Retries a failed notification under the active destination policy.
    /// # Errors
    /// Returns transport, disabled destination, decoding or API errors.
    pub async fn retry_notification(&self, id: &str) -> Result<(), ClientError> {
        self.generated
            .retry_notification_delivery(id)
            .payload()
            .await
            .map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::Elapsed;

    #[test]
    fn elapsed_uses_the_largest_fitting_units() {
        for (milliseconds, text) in [
            (-5, "0 ms"),
            (850, "850 ms"),
            (12_340, "12.3 s"),
            (59_949, "59.9 s"),
            (59_950, "1 min 0 s"),
            (252_000, "4 min 12 s"),
            (7_500_000, "2 h 5 min"),
        ] {
            assert_eq!(Elapsed::from_millis(milliseconds).to_string(), text);
        }
        assert_eq!(Elapsed::from_millis_f64(3_245.7).to_string(), "3.2 s");
        assert_eq!(Elapsed::from_millis_f64(f64::NAN).to_string(), "0 ms");
    }
}
