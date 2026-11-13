//! Bounded deployment history. Every deploy, promotion, restore and refresh
//! records a deployment, so without a bound history grows forever.
use super::{Store, StoreError, days_ago_ms, now_ms};

/// Deployments pruned per transaction, so pruning never holds the writer long.
const PRUNE_BATCH: usize = 1_000;
/// How long ago a preview's deployment must have finished before it can be
/// pruned: a superseded one may still be stopping.
const PREVIEW_MIN_AGE_MS: i64 = 3_600_000;
/// Releases each application keeps beyond those a deployment references.
const KEPT_RELEASES: i64 = 50;

/// How much deployment history each environment keeps; see
/// [`Store::prune_deployment_history`].
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct DeploymentRetention {
    /// `retention.deployment_days`; zero keeps every deployment.
    days: u64,
    /// `retention.deployments_per_environment`; zero keeps every deployment.
    per_environment: u32,
    /// `images.keep_deployments`: the newest successful deployments, whose
    /// images are kept so they can be restored.
    succeeded: u32,
}

impl DeploymentRetention {
    pub(super) fn new(config: &piqueld_config::DaemonConfig) -> Self {
        Self {
            days: config.retention.deployment_days,
            per_environment: config.retention.deployments_per_environment,
            succeeded: config.images.keep_deployments,
        }
    }
}

impl Store {
    /// Prunes each environment's finished deployments that are both older
    /// than `retention.deployment_days` and beyond its newest
    /// `retention.deployments_per_environment`, with their operations. It
    /// never prunes an environment's current or latest deployment, its newest
    /// `images.keep_deployments` successful ones (at least one, which an
    /// automatic rollback restores), one a remaining deployment restored,
    /// whose origin names it, or one whose jobs still need cleaning up.
    /// Previews keep only those, of their deployments finished over
    /// [`PREVIEW_MIN_AGE_MS`] ago. Then removes the build and job-run records
    /// of pruned operations whose output expired, and the releases no
    /// deployment references beyond each application's newest
    /// [`KEPT_RELEASES`]. Either setting at zero disables all of this.
    ///
    /// The candidates are chosen once, then deleted [`PRUNE_BATCH`] at a time,
    /// each batch rechecking what can change meanwhile: job cleanup, and
    /// restores accepted since. Deleting a deployment's operation cascades to
    /// it, its attempts, inputs and secret pins, so each batch also advances
    /// `history_coverage.pruned_through_ms`: deployment analytics over the
    /// pruned span are then marked incomplete.
    ///
    /// # Errors
    /// Returns storage or encoding errors.
    pub async fn prune_deployment_history(&self) -> Result<(), StoreError> {
        let retention = self.deployment_retention;
        if retention.days == 0 || retention.per_environment == 0 {
            return Ok(());
        }
        let (succeeded, cutoff, preview_cutoff) = (
            retention.succeeded.max(1),
            days_ago_ms(retention.days),
            now_ms().saturating_sub(PREVIEW_MIN_AGE_MS),
        );
        let candidates = sqlx::query_scalar!(
            r#"WITH current AS (
                SELECT MAX(d.id) AS id FROM deployments d JOIN operations o ON o.id=d.id WHERE o.promoted=1 GROUP BY d.environment_id
            ), ranked AS (
                SELECT d.id,e.kind,o.finished_at_ms,d.succeeded_at_ms IS NOT NULL AS succeeded,
                    ROW_NUMBER() OVER (PARTITION BY d.environment_id ORDER BY d.created_at_ms DESC,d.id DESC) AS rank,
                    ROW_NUMBER() OVER (PARTITION BY d.environment_id,d.succeeded_at_ms IS NOT NULL ORDER BY d.created_at_ms DESC,d.id DESC) AS outcome_rank
                FROM deployments d JOIN operations o ON o.id=d.id JOIN environments e ON e.id=d.environment_id
            )
            SELECT r.id AS "id!" FROM ranked r
            WHERE r.finished_at_ms IS NOT NULL AND r.rank>1
                AND NOT (r.succeeded AND r.outcome_rank<=CASE WHEN r.kind='preview' THEN 1 ELSE ?1 END)
                AND CASE WHEN r.kind='preview' THEN r.finished_at_ms<?4 ELSE r.rank>?2 AND r.finished_at_ms<?3 END
                AND r.id NOT IN (SELECT id FROM current)"#,
            succeeded,
            retention.per_environment,
            cutoff,
            preview_cutoff
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::database)?;
        for batch in candidates.chunks(PRUNE_BATCH) {
            let ids = serde_json::to_string(batch).map_err(StoreError::corrupt)?;
            let (_writer, mut tx) = self.begin_immediate().await?;
            let finished = sqlx::query_scalar!(
                r#"DELETE FROM operations WHERE id IN (SELECT value FROM json_each(?1))
                    AND NOT EXISTS(SELECT 1 FROM job_cleanup j WHERE j.operation_id=operations.id)
                    AND id NOT IN (SELECT json_extract(origin_json,'$.deployment') FROM deployments WHERE json_extract(origin_json,'$.type')='restore')
                    RETURNING finished_at_ms AS "finished_at_ms!""#,
                ids
            )
            .fetch_all(&mut *tx)
            .await
            .map_err(StoreError::database)?;
            // Analytics count deployments by their attempts, which went too.
            if let Some(last) = finished.into_iter().max() {
                sqlx::query!(
                    "UPDATE history_coverage SET pruned_through_ms=MAX(COALESCE(pruned_through_ms,0),?1) WHERE singleton=1",
                    last
                )
                .execute(&mut *tx)
                .await
                .map_err(StoreError::database)?;
            }
            tx.commit().await.map_err(StoreError::database)?;
        }
        let (_writer, mut tx) = self.begin_immediate().await?;
        sqlx::query!("DELETE FROM builds WHERE log_expired=1 AND NOT EXISTS(SELECT 1 FROM operations o WHERE o.id=builds.operation_id)")
            .execute(&mut *tx)
            .await
            .map_err(StoreError::database)?;
        sqlx::query!(
            "DELETE FROM releases WHERE NOT EXISTS(SELECT 1 FROM deployments d WHERE d.release_id=releases.id) AND id NOT IN (SELECT id FROM (SELECT id,ROW_NUMBER() OVER (PARTITION BY application_id ORDER BY id DESC) AS rank FROM releases) WHERE rank<=?1)",
            KEPT_RELEASES
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)
    }
}

#[cfg(test)]
mod tests;
