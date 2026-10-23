//! Diagnostic history, daemon measurements, notification destinations and
//! delivery controls.
use crate::{
    Client, ClientError, Event, Page,
    client::{EnvelopedCall, GeneratedCall, check_page_limit},
};
pub use piqueld_core::observability::*;
use std::{fmt, time::Duration};

/// A size as people read it, in binary units with one decimal from a
/// kibibyte: `512 B`, `1.5 KiB` or `118.3 GiB`. The dashboard and
/// `piquelctl` show sizes this way.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Bytes(pub u64);

impl fmt::Display for Bytes {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
        // The largest unit it reaches, of 2^(10 × power) bytes.
        match (1..=UNITS.len())
            .rev()
            .find(|power| self.0 >> (10 * power) > 0)
        {
            None => write!(formatter, "{} B", self.0),
            Some(power) => {
                let unit = 1_u128 << (10 * power);
                // Tenths of the unit, rounded half up.
                let tenths = (u128::from(self.0) * 10 + unit / 2) / unit;
                let unit = UNITS[power - 1];
                write!(formatter, "{}.{} {unit}", tenths / 10, tenths % 10)
            }
        }
    }
}

/// A long duration to the minute, as uptime reads: `3 d 4 h`, `2 h 5 min`,
/// `12 min` or, under a minute, `45 s`. The dashboard and `piquelctl` show
/// the daemon's uptime this way.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Uptime(pub Duration);

impl fmt::Display for Uptime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let seconds = self.0.as_secs();
        match (
            seconds / 86_400,
            seconds % 86_400 / 3_600,
            seconds % 3_600 / 60,
        ) {
            (days @ 1.., hours, _) => write!(formatter, "{days} d {hours} h"),
            (0, hours @ 1.., minutes) => write!(formatter, "{hours} h {minutes} min"),
            (0, 0, minutes @ 1..) => write!(formatter, "{minutes} min"),
            _ => write!(formatter, "{seconds} s"),
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
    use super::{Bytes, Uptime};
    use std::time::Duration;

    #[test]
    fn bytes_use_the_largest_reached_binary_unit() {
        for (bytes, text) in [
            (0, "0 B"),
            (1023, "1023 B"),
            (1024, "1.0 KiB"),
            (1536, "1.5 KiB"),
            (52_428_800, "50.0 MiB"),
            (127_026_911_232, "118.3 GiB"),
            (u64::MAX, "16777216.0 TiB"),
        ] {
            assert_eq!(Bytes(bytes).to_string(), text);
        }
    }

    #[test]
    fn uptime_keeps_its_two_largest_units() {
        for (seconds, text) in [
            (45, "45 s"),
            (720, "12 min"),
            (7_500, "2 h 5 min"),
            (273_600, "3 d 4 h"),
        ] {
            assert_eq!(Uptime(Duration::from_secs(seconds)).to_string(), text);
        }
    }
}
