//! Cached local measurements and bounded historical aggregates.
use super::{Store, StoreError, now_ms};
use piqueld_core::observability::{ActionDuration, DaemonStats, DeploymentAnalytics, FailureCount};

impl Store {
    /// Configures startup-loaded observability without exposing destination secrets.
    #[must_use]
    pub fn with_observability(mut self, config: &crate::config::DaemonConfig) -> Self {
        self.notifications = config.notifications.clone();
        self.daemon_event_days = config.retention.daemon_event_days;
        self
    }
    /// Returns a snapshot cached for five seconds to avoid expensive per-request scans.
    /// # Errors
    /// Returns storage errors; unavailable OS measurements are represented by null.
    pub async fn daemon_stats(&self) -> Result<DaemonStats, StoreError> {
        let mut cache = self.stats_cache.lock().await;
        let now = now_ms();
        if let Some(stats) = cache
            .as_ref()
            .filter(|s| now.saturating_sub(s.sampled_at_ms) < 5000)
        {
            return Ok(stats.clone());
        }
        let row=sqlx::query!("SELECT (SELECT COUNT(*) FROM events) AS events,(SELECT COUNT(DISTINCT diagnostic_id) FROM events WHERE diagnostic_id IS NOT NULL) AS diagnostics,(SELECT COALESCE(SUM(length(data)),0) FROM build_log_chunks) AS build_bytes,(SELECT COUNT(*) FROM operations WHERE state='running') AS running,(SELECT COUNT(*) FROM operations WHERE state='requested') AS queued,(SELECT COUNT(*) FROM notification_deliveries WHERE state='pending') AS pending,(SELECT COUNT(*) FROM notification_deliveries WHERE state='failed') AS failed").fetch_one(&self.pool).await.map_err(StoreError::database)?;
        let database_bytes = tokio::fs::metadata(&self.database_path)
            .await
            .map_err(StoreError::path)?
            .len();
        let wal_path = self.database_path.with_file_name(format!(
            "{}-wal",
            self.database_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
        ));
        let wal_bytes = match tokio::fs::metadata(wal_path).await {
            Ok(m) => m.len(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => 0,
            Err(e) => return Err(StoreError::path(e)),
        };
        let (memory_bytes, cpu_seconds) = Self::process_usage().await;
        let cpu_percent = cache.as_ref().and_then(|old| {
            Some(
                ((cpu_seconds? - old.cpu_seconds?).max(0.0)
                    / std::time::Duration::from_millis(
                        u64::try_from(now - old.sampled_at_ms).ok()?,
                    )
                    .as_secs_f64())
                    * 100.0,
            )
        });
        let available_disk_bytes = self
            .database_path
            .parent()
            .and_then(|path| rustix::fs::statvfs(path).ok())
            .map(|s| s.f_bavail.saturating_mul(s.f_frsize));
        let stats = DaemonStats {
            sampled_at_ms: now,
            uptime_seconds: self.started.elapsed().as_secs(),
            memory_bytes,
            cpu_seconds,
            cpu_percent,
            database_bytes,
            wal_bytes,
            available_disk_bytes,
            events: row.events,
            diagnostics: row.diagnostics,
            build_output_bytes: row.build_bytes,
            running_operations: row.running,
            queued_operations: row.queued,
            pending_deliveries: row.pending,
            failed_deliveries: row.failed,
        };
        *cache = Some(stats.clone());
        Ok(stats)
    }
    async fn process_usage() -> (Option<u64>, Option<f64>) {
        let memory = tokio::fs::read_to_string("/proc/self/status")
            .await
            .ok()
            .and_then(|s| {
                s.lines().find_map(|line| {
                    line.strip_prefix("VmRSS:")
                        .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
                        .map(|kb| kb.saturating_mul(1024))
                })
            });
        let cpu = tokio::fs::read_to_string("/proc/self/stat")
            .await
            .ok()
            .and_then(|s| {
                let fields = s
                    .rsplit_once(") ")?
                    .1
                    .split_whitespace()
                    .collect::<Vec<_>>();
                let ticks = fields
                    .get(11)?
                    .parse::<u64>()
                    .ok()?
                    .saturating_add(fields.get(12)?.parse::<u64>().ok()?);
                let hz = rustix::param::clock_ticks_per_second();
                Some(
                    std::time::Duration::from_millis(ticks.saturating_mul(1000).checked_div(hz)?)
                        .as_secs_f64(),
                )
            });
        (memory, cpu)
    }
    /// Derives deployment outcomes from retained attempts and detailed durations from events.
    /// # Errors
    /// Returns invalid time intervals or storage errors.
    pub async fn deployment_analytics(
        &self,
        application: Option<&str>,
        since: i64,
        until: i64,
    ) -> Result<DeploymentAnalytics, StoreError> {
        if since < 0 || until < since {
            return Err(StoreError::InvalidInput);
        }
        let mut tx = self.pool.begin().await.map_err(StoreError::database)?;
        let coverage = sqlx::query!(
            "SELECT started_at_ms,pruned_through_ms FROM history_coverage WHERE singleton=1"
        )
        .fetch_one(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        let attempts=sqlx::query!("SELECT a.deployment_id,a.attempt,a.outcome_json FROM deployment_attempts a JOIN deployments d ON d.id=a.deployment_id WHERE (?1 IS NULL OR d.application_id=?1) AND json_extract(a.outcome_json,'$.finished_at_ms') BETWEEN ?2 AND ?3 ORDER BY a.deployment_id,a.attempt",application,since,until).fetch_all(&mut *tx).await.map_err(StoreError::database)?;
        let mut result = DeploymentAnalytics {
            since_ms: since,
            until_ms: until,
            history_started_at_ms: coverage.started_at_ms,
            pruned_through_ms: coverage.pruned_through_ms,
            incomplete: since < coverage.started_at_ms
                || coverage.pruned_through_ms.is_some_and(|v| v >= since),
            ..DeploymentAnalytics::default()
        };
        let mut outcomes = std::collections::BTreeMap::new();
        let mut durations = Vec::new();
        for row in attempts {
            let op: piqueld_core::Operation =
                serde_json::from_str(&row.outcome_json).map_err(StoreError::corrupt)?;
            result.failed_attempts += i64::from(op.state == piqueld_core::OperationState::Failed);
            result.retry_attempts += i64::from(row.attempt > 1);
            if let Some((start, end)) = op.started_at_ms.zip(op.finished_at_ms) {
                durations.push(end.saturating_sub(start));
            }
            outcomes.insert(row.deployment_id, op.state);
        }
        result.deployments = i64::try_from(outcomes.len()).map_err(StoreError::corrupt)?;
        result.succeeded = i64::try_from(
            outcomes
                .values()
                .filter(|&&s| s == piqueld_core::OperationState::Succeeded)
                .count(),
        )
        .map_err(StoreError::corrupt)?;
        result.failed = i64::try_from(
            outcomes
                .values()
                .filter(|&&s| s == piqueld_core::OperationState::Failed)
                .count(),
        )
        .map_err(StoreError::corrupt)?;
        if !durations.is_empty() {
            let sum = durations.iter().fold(0_u64, |a, &b| {
                a.saturating_add(u64::try_from(b).unwrap_or(0))
            });
            let count = u32::try_from(durations.len()).map_err(StoreError::corrupt)?;
            result.mean_duration_ms = Some(
                std::time::Duration::from_millis(sum).as_secs_f64() * 1000.0 / f64::from(count),
            );
        }
        result.action_retries=sqlx::query_scalar!("SELECT COUNT(*) FROM events WHERE kind='action_retry' AND (?1 IS NULL OR application_id=?1) AND created_at_ms BETWEEN ?2 AND ?3",application,since,until).fetch_one(&mut *tx).await.map_err(StoreError::database)?;
        result.actions=sqlx::query!("SELECT phase,COUNT(*) AS \"count!: i64\",AVG(duration_ms) AS \"mean?: f64\" FROM events WHERE duration_ms IS NOT NULL AND phase IS NOT NULL AND (?1 IS NULL OR application_id=?1) AND created_at_ms BETWEEN ?2 AND ?3 GROUP BY phase ORDER BY phase",application,since,until).fetch_all(&mut *tx).await.map_err(StoreError::database)?.into_iter().map(|r|ActionDuration {phase:r.phase.unwrap_or_default(),count:r.count,mean_ms:r.mean.unwrap_or(0.0)}).collect();
        result.failures=sqlx::query!("SELECT error_code,COUNT(DISTINCT COALESCE(diagnostic_id,CAST(id AS TEXT))) AS \"count!: i64\" FROM events WHERE error_code IS NOT NULL AND (?1 IS NULL OR application_id=?1) AND created_at_ms BETWEEN ?2 AND ?3 GROUP BY error_code ORDER BY 2 DESC,error_code LIMIT 20",application,since,until).fetch_all(&mut *tx).await.map_err(StoreError::database)?.into_iter().map(|r|FailureCount{code:r.error_code.unwrap_or_default(),count:r.count}).collect();
        tx.commit().await.map_err(StoreError::database)?;
        Ok(result)
    }
}
