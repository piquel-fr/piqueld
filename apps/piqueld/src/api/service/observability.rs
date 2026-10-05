//! Shared observability operations, independent of HTTP and the browser.
use super::{ApplicationError, ApplicationService};
use crate::store::Visibility;
use piqueld_core::{
    Event,
    api::Page,
    observability::{
        DaemonStats, DeploymentAnalytics, Diagnostic, EventFilter, NotificationDelivery,
    },
};

impl ApplicationService {
    /// Lists structured events with indexed filters, limited to `visible`.
    /// # Errors
    /// Returns invalid filters, cursors or storage failures.
    pub async fn filtered_events(
        &self,
        filter: &EventFilter,
        visible: &Visibility,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Page<Event>, ApplicationError> {
        Ok(self
            .store
            .filtered_events(filter, visible, cursor, limit)
            .await?)
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
        visible: &Visibility,
        after: i64,
        limit: usize,
    ) -> Result<(Vec<Event>, i64), ApplicationError> {
        Ok(self
            .store
            .stream_events(filter, visible, after, limit)
            .await?)
    }
    /// Records an unexpected boundary failure. A failed write keeps its ID in fallback logs.
    pub(crate) async fn record_diagnostic(
        &self,
        diagnostic: &Diagnostic,
        request_id: Option<&str>,
        application: Option<&piqueld_core::EnvironmentId>,
        actor: crate::store::Attribution<'_>,
    ) {
        if let Err(error) = self
            .store
            .record_diagnostic(diagnostic, request_id, application, actor)
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
    /// Derives deployment analytics over a bounded time selection, from the
    /// history `visible` allows.
    /// # Errors
    /// Returns invalid selection or storage failures.
    pub async fn deployment_analytics(
        &self,
        application: Option<&str>,
        visible: &Visibility,
        since: i64,
        until: i64,
    ) -> Result<DeploymentAnalytics, ApplicationError> {
        Ok(self
            .store
            .deployment_analytics(application, visible, since, until)
            .await?)
    }
    /// Records one audited API request in the background, so responses never
    /// wait for the writer, e.g. during a storage outage. At most
    /// [`super::AUDIT_BACKLOG`] records wait at once; further ones are dropped and
    /// logged rather than accumulating without bound.
    pub(crate) fn record_audit(&self, event: crate::store::NewAuditEvent) {
        let Ok(slot) = self.audit_backlog.clone().try_acquire_owned() else {
            tracing::error!(action = %event.action, "audit backlog is full; record dropped");
            return;
        };
        let store = std::sync::Arc::clone(&self.store);
        tokio::spawn(async move {
            if let Err(error) = store.record_audit(&event).await {
                tracing::error!(?error, action = %event.action, "audit event could not be recorded");
            }
            drop(slot);
        });
    }
    /// Waits up to `deadline` for background audit records to be written, so a
    /// shutdown keeps the requests it just served. Called once the API
    /// listeners have stopped; records still pending afterwards are logged.
    pub async fn drain_audit(&self, deadline: std::time::Duration) {
        let all = u32::try_from(super::AUDIT_BACKLOG).unwrap_or(u32::MAX);
        let drained = tokio::time::timeout(deadline, self.audit_backlog.acquire_many(all)).await;
        if drained.is_err() {
            let pending = super::AUDIT_BACKLOG - self.audit_backlog.available_permits();
            tracing::error!(pending, "audit records still pending at shutdown were lost");
        }
    }
    /// Counts one refused API request for the `access_denied_total` metric.
    pub(crate) fn count_denial(&self) {
        self.denials
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    /// Lists audited requests, newest first.
    /// # Errors
    /// Returns invalid cursor or storage errors.
    pub async fn audit_events(
        &self,
        filter: &piqueld_core::audit::AuditFilter,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Page<piqueld_core::audit::AuditEvent>, ApplicationError> {
        Ok(self.store.audit_events(filter, cursor, limit).await?)
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
    pub async fn retry_notification(
        &self,
        actor: super::Actor<'_>,
        id: &str,
    ) -> Result<(), ApplicationError> {
        Ok(self.store.retry_delivery(actor, id).await?)
    }
    /// Renders the cached daemon statistics for the metrics listener's `GET /metrics`.
    /// Each available measurement becomes a `piqueld_*` sample with Prometheus
    /// HELP/TYPE metadata. Unavailable OS measurements are omitted. Collection
    /// uses the same cache as the daemon status page; this does not contact or
    /// manage Prometheus, retain samples, or export application logs/events.
    ///
    /// Names ending in `_total` are counters; everything else is a gauge:
    ///
    /// ```text
    /// # HELP piqueld_uptime_seconds Daemon process uptime
    /// # TYPE piqueld_uptime_seconds gauge
    /// piqueld_uptime_seconds 3600
    /// ```
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
                "access_denied_total",
                "API requests refused for missing credentials or permission since start",
                Some(
                    self.denials
                        .load(std::sync::atomic::Ordering::Relaxed)
                        .to_string(),
                ),
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
