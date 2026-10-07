//! Durable requests to stop job runs, independent of deployment retries.
use super::{EnvironmentId, Store, StoreError};
use sqlx::SqliteConnection;

impl Store {
    /// Saves cleanup intent before attempting to stop an operation's job runs.
    pub(crate) async fn request_job_cleanup(&self, operation: &str) -> Result<(), StoreError> {
        let _writer = self.writers.lock().await;
        Self::request_job_cleanup_on(
            &mut *self.pool.acquire().await.map_err(StoreError::database)?,
            operation,
        )
        .await
    }

    /// Also used inside the transaction that records a terminal job failure,
    /// so failed persistence of the earlier request cannot lose cleanup intent.
    pub(super) async fn request_job_cleanup_on(
        connection: &mut SqliteConnection,
        operation: &str,
    ) -> Result<(), StoreError> {
        sqlx::query!(
            "INSERT INTO job_cleanup(operation_id) VALUES(?1) ON CONFLICT DO NOTHING",
            operation
        )
        .execute(connection)
        .await
        .map_err(StoreError::constraint)?;
        Ok(())
    }

    /// Finds pending cleanup, including runs from superseded operations.
    pub(crate) async fn pending_job_cleanup(
        &self,
        environment: &EnvironmentId,
    ) -> Result<Vec<String>, StoreError> {
        let environment = environment.as_str();
        sqlx::query_scalar!(
            "SELECT job_cleanup.operation_id FROM job_cleanup JOIN operations ON operations.id=job_cleanup.operation_id WHERE operations.environment_id=?1 ORDER BY job_cleanup.operation_id",
            environment
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::database)
    }

    /// Clears intent only after Docker confirms the job containers have stopped.
    pub(crate) async fn finish_job_cleanup(&self, operation: &str) -> Result<(), StoreError> {
        let _writer = self.writers.lock().await;
        sqlx::query!("DELETE FROM job_cleanup WHERE operation_id=?1", operation)
            .execute(&self.pool)
            .await
            .map_err(StoreError::database)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::OperationState;
    use piqueld_core::manifest::ApplicationTemplate;

    #[tokio::test]
    async fn terminal_job_failure_keeps_cleanup_atomic_and_retained() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("db");
        let store = Store::open(&path).await.unwrap();
        let application = piqueld_core::parse_toml(
            "api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='jobs'\n[spec]",
        )
        .unwrap()
        .normalize(piqueld_core::ApplicationId::parse("app-jobs").unwrap());
        let operation = store
            .save_application(&ApplicationTemplate::from(&application), None, None)
            .await
            .unwrap();
        store
            .transition_operation(
                &operation.id,
                OperationState::Requested,
                OperationState::Running,
                None,
            )
            .await
            .unwrap();

        // A terminal failure cannot commit without its cleanup request, even
        // if the earlier request from job execution could not be persisted.
        sqlx::query("CREATE TRIGGER reject_cleanup BEFORE INSERT ON job_cleanup BEGIN SELECT RAISE(FAIL,'cleanup storage unavailable'); END")
            .execute(&store.pool).await.unwrap();
        assert!(
            store
                .transition_operation(
                    &operation.id,
                    OperationState::Running,
                    OperationState::Failed,
                    Some(("job_timeout", "migration timed out"))
                )
                .await
                .is_err()
        );
        assert_eq!(
            store.operation(&operation.id).await.unwrap().state,
            OperationState::Running
        );
        sqlx::query("DROP TRIGGER reject_cleanup")
            .execute(&store.pool)
            .await
            .unwrap();
        store
            .transition_operation(
                &operation.id,
                OperationState::Running,
                OperationState::Failed,
                Some(("job_timeout", "migration timed out")),
            )
            .await
            .unwrap();
        drop(store);

        let store = Store::open(&path).await.unwrap();
        assert_eq!(
            store
                .pending_job_cleanup(&operation.environment_id)
                .await
                .unwrap(),
            std::slice::from_ref(&operation.id)
        );
        store
            .save_application(
                &ApplicationTemplate::from(&application),
                None,
                Some(operation.generation),
            )
            .await
            .unwrap();
        // Model expiry of the old deployment snapshot; operation retention
        // must still preserve the cleanup request and its journal context.
        sqlx::query("DELETE FROM deployments WHERE id=?1")
            .bind(&operation.id)
            .execute(&store.pool)
            .await
            .unwrap();
        assert_eq!(store.prune_finished_operations(i64::MAX).await.unwrap(), 0);
        assert_eq!(
            store
                .pending_job_cleanup(&operation.environment_id)
                .await
                .unwrap(),
            std::slice::from_ref(&operation.id)
        );
        store.finish_job_cleanup(&operation.id).await.unwrap();
        assert_eq!(store.prune_finished_operations(i64::MAX).await.unwrap(), 1);
    }
}
