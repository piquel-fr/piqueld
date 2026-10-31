//! Automatic rollback: once a deployment's rollout fails in an environment
//! with `rollback = "automatic"`, its last successful deployment is restored
//! (see `store::restore`) in the transaction that fails it. That restore
//! becomes the maintained target, so drift repair never reapplies the failed
//! deployment, and Swarm's own service rollback, which would disagree with
//! it, is never used. A rollback is never rolled back itself.
use super::{Controller, DockerApi, Operation, OperationError, StoreError};
use piqueld_core::{api::DeploymentOrigin, manifest::Rollback, observability::DeploymentFailure};

/// Why a failed rollout was not rolled back.
#[derive(Debug, thiserror::Error)]
enum RollbackError {
    /// No other deployment of the environment ever succeeded.
    #[error("no earlier deployment of the environment succeeded")]
    NothingToRestore,
    /// An image of the deployment to restore is gone and can't be pulled
    /// again.
    #[error(transparent)]
    Images(OperationError),
    /// Restoring was refused, or storage failed.
    #[error(transparent)]
    Store(#[from] StoreError),
}

impl<D: DockerApi> Controller<D> {
    /// Fails the running `operation` with `error`, and rolls it back in the
    /// same transaction when its rollout failed in an environment with
    /// automatic rollback and it is no rollback itself. A rollback that
    /// can't be accepted leaves the operation failed as usual, with a
    /// `rollback_unavailable` event saying why.
    pub(super) async fn fail(
        &self,
        operation: &Operation,
        error: &OperationError,
    ) -> Result<(), StoreError> {
        if !self.rolls_back(operation, error).await? {
            return self.fail_operation(operation, error).await;
        }
        let unavailable = match self.roll_back(operation, error).await {
            Ok(rollback) => {
                tracing::warn!(rollback_id = %rollback.id, "rollout failed; rolling back");
                return Ok(());
            }
            // No longer the latest or running: failing it reports that.
            Err(RollbackError::Store(StoreError::IllegalTransition)) => {
                return self.fail_operation(operation, error).await;
            }
            Err(unavailable) => unavailable,
        };
        tracing::error!(error = %unavailable, "rollout failed; automatic rollback unavailable");
        self.fail_operation(operation, error).await?;
        self.store
            .rollback_unavailable(
                operation,
                &format!("not rolled back automatically: {unavailable}"),
            )
            .await
    }

    /// Whether `operation`, failing with `error`, is rolled back: its rollout
    /// failed, its environment rolls back automatically, it is no automatic
    /// rollback itself, and it never succeeded before: a retry or repair of
    /// what already ran is no rollout of something new.
    async fn rolls_back(
        &self,
        operation: &Operation,
        error: &OperationError,
    ) -> Result<bool, StoreError> {
        if error.diagnostic_code().deployment_failure() != Some(DeploymentFailure::Rollout) {
            return Ok(false);
        }
        let origin = self.store.deployment_origin(&operation.id).await?;
        if matches!(
            origin,
            DeploymentOrigin::Restore {
                rolled_back: Some(_),
                ..
            }
        ) {
            return Ok(false);
        }
        let environment = self.store.get(&operation.environment_id).await?;
        Ok(environment.rollback() == Rollback::Automatic
            && !self.store.deployment_succeeded(&operation.id).await?)
    }

    /// Fails `operation` with `error` and accepts restoring its
    /// environment's last successful deployment, once its images are
    /// present: missing registry images are pulled again by digest, with
    /// image cleanup held off until the rollback is saved.
    async fn roll_back(
        &self,
        operation: &Operation,
        error: &OperationError,
    ) -> Result<Operation, RollbackError> {
        let (deployment, target) = self
            .store
            .rollback_target(operation)
            .await?
            .ok_or(RollbackError::NothingToRestore)?;
        let in_use = self.images_in_use().await;
        in_use
            .ensure(&*self.docker, target.sources())
            .await
            .map_err(RollbackError::Images)?;
        let message = error.message();
        Ok(self
            .store
            .roll_back(operation, (error.code(), &message), &deployment)
            .await?)
    }
}
