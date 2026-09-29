//! Shared observability operations, independent of HTTP and the browser.
use super::{ApplicationError, ApplicationService};
use piqueld_core::{
    Event,
    api::Page,
    observability::{
        DaemonStats, DeploymentAnalytics, Diagnostic, EventFilter, NotificationDelivery,
    },
};

impl ApplicationService {
    /// Lists structured events with indexed filters.
    /// # Errors
    /// Returns invalid filters, cursors or storage failures.
    pub async fn filtered_events(
        &self,
        filter: &EventFilter,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Page<Event>, ApplicationError> {
        Ok(self.store.filtered_events(filter, cursor, limit).await?)
    }
    /// Looks up a diagnostic occurrence and its contextual event.
    /// # Errors
    /// Returns absence or storage failures.
    pub async fn diagnostic(&self, id: &str) -> Result<Event, ApplicationError> {
        Ok(self.store.diagnostic(id).await?)
    }
    /// Reads the next stream batch and the position it covers.
    /// # Errors
    /// Returns expired history, invalid filters or storage failures.
    pub async fn stream_events(
        &self,
        filter: &EventFilter,
        after: i64,
        limit: usize,
    ) -> Result<(Vec<Event>, i64), ApplicationError> {
        Ok(self.store.stream_events(filter, after, limit).await?)
    }
    /// Records an unexpected boundary failure. A failed write keeps its ID in fallback logs.
    pub(crate) async fn record_diagnostic(
        &self,
        diagnostic: &Diagnostic,
        request_id: Option<&str>,
        application: Option<&piqueld_core::ApplicationId>,
    ) {
        if let Err(error) = self
            .store
            .record_diagnostic(diagnostic, request_id, application)
            .await
        {
            tracing::error!(diagnostic_id=%diagnostic.id,?request_id,code=%diagnostic.code,summary=%diagnostic.summary,error=?error,"diagnostic could not be persisted");
        }
    }
    /// Returns cached resource statistics.
    /// # Errors
    /// Returns storage or filesystem failures.
    pub async fn daemon_stats(&self) -> Result<DaemonStats, ApplicationError> {
        Ok(self.store.daemon_stats().await?)
    }
    /// Derives deployment analytics over a bounded time selection.
    /// # Errors
    /// Returns invalid selection or storage failures.
    pub async fn deployment_analytics(
        &self,
        application: Option<&str>,
        since: i64,
        until: i64,
    ) -> Result<DeploymentAnalytics, ApplicationError> {
        Ok(self
            .store
            .deployment_analytics(application, since, until)
            .await?)
    }
    /// Lists retained notification delivery attempts.
    /// # Errors
    /// Returns pagination or storage failures.
    pub async fn notification_deliveries(
        &self,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Page<NotificationDelivery>, ApplicationError> {
        Ok(self.store.deliveries(cursor, limit).await?)
    }
    /// Requests another delivery attempt under the current configuration.
    /// # Errors
    /// Returns absent, disabled or invalid delivery errors.
    pub async fn retry_notification(&self, id: &str) -> Result<(), ApplicationError> {
        Ok(self.store.retry_delivery(id).await?)
    }
    /// Renders the cached daemon statistics for the metrics listener's `GET /metrics`.
    /// Each available measurement becomes a `piqueld_*` sample with Prometheus
    /// HELP/TYPE metadata. Unavailable OS measurements are omitted. Collection
    /// uses the same cache as the daemon status page; this does not contact or
    /// manage Prometheus, retain samples, or export application logs/events.
    /// # Errors
    /// Returns collection failures instead of publishing misleading zero values.
    pub async fn prometheus_metrics(&self) -> Result<String, ApplicationError> {
        use std::fmt::Write as _;
        let stats = self.daemon_stats().await?;
        let mut output = String::new();
        for (name, help, value) in [
            (
                "uptime_seconds",
                "Daemon process uptime",
                Some(stats.uptime_seconds.to_string()),
            ),
            (
                "process_resident_memory_bytes",
                "Process resident memory",
                stats.memory_bytes.map(|v| v.to_string()),
            ),
            (
                "process_cpu_seconds_total",
                "Process CPU seconds",
                stats.cpu_seconds.map(|v| v.to_string()),
            ),
            (
                "process_cpu_percent",
                "CPU percentage; one core equals 100",
                stats.cpu_percent.map(|v| v.to_string()),
            ),
            (
                "database_bytes",
                "SQLite main database file size",
                Some(stats.database_bytes.to_string()),
            ),
            (
                "database_wal_bytes",
                "SQLite WAL file size",
                Some(stats.wal_bytes.to_string()),
            ),
            (
                "disk_available_bytes",
                "Disk bytes available to the daemon",
                stats.available_disk_bytes.map(|v| v.to_string()),
            ),
            (
                "events",
                "Retained event records",
                Some(stats.events.to_string()),
            ),
            (
                "diagnostics",
                "Retained distinct diagnostic occurrences",
                Some(stats.diagnostics.to_string()),
            ),
            (
                "build_output_bytes",
                "Retained build output",
                Some(stats.build_output_bytes.to_string()),
            ),
            (
                "operations_running",
                "Running operations",
                Some(stats.running_operations.to_string()),
            ),
            (
                "operations_queued",
                "Queued operations",
                Some(stats.queued_operations.to_string()),
            ),
            (
                "notification_deliveries_pending",
                "Pending webhook deliveries",
                Some(stats.pending_deliveries.to_string()),
            ),
            (
                "notification_deliveries_failed",
                "Failed webhook deliveries",
                Some(stats.failed_deliveries.to_string()),
            ),
        ] {
            if let Some(value) = value {
                let kind = if name.ends_with("_total") {
                    "counter"
                } else {
                    "gauge"
                };
                writeln!(output,"# HELP piqueld_{name} {help}\n# TYPE piqueld_{name} {kind}\npiqueld_{name} {value}").expect("writing a string cannot fail");
            }
        }
        Ok(output)
    }
}
