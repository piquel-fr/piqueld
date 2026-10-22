//! Restoring a deployment: deploying an earlier, succeeded deployment of the
//! same environment or preview again exactly as it ran, with its rendered
//! manifest, images and pinned secret versions. Nothing is rendered, fetched
//! or built, and no newer input is ever substituted, so the prepared target
//! is saved in the accepting transaction. Restores never run jobs.
use super::{
    ApplicationState, EnvironmentId, Operation, OperationKind, Store, StoreError,
    StoredEnvironment, acceptance::Accepted, now_ms,
};
use crate::api::Restore;
use piqueld_core::{
    api::DeploymentOrigin, manifest::ApplicationTemplate, resource::ResolvedApplication,
};
use sqlx::{Sqlite, SqliteConnection, Transaction};

/// Why a deployment can't be restored.
#[derive(Debug, thiserror::Error)]
pub enum RestoreError {
    /// Only deployments that succeeded once ran as recorded.
    #[error("deployment {deployment} can't be restored: {reason}")]
    NotRestorable {
        /// The requested deployment.
        deployment: String,
        /// What it lacks.
        reason: &'static str,
    },
    /// Secret versions the deployment pinned were deleted since, or their
    /// values discarded by key recovery.
    #[error(
        "deployment {deployment} mounts secret versions that no longer exist: {}; deploy instead to use their current versions",
        secrets.join(", ")
    )]
    SecretsGone {
        /// The requested deployment.
        deployment: String,
        /// The logical names of the secrets, in name order.
        secrets: Vec<String>,
    },
}

impl Store {
    /// Accepts `restore` in `tx`: finds its environment, or its preview,
    /// only among its own kind, then restores the deployment (see
    /// `accept_restore_on`). Neither needs the application revision.
    pub(super) async fn restore_on(
        tx: &mut Transaction<'_, Sqlite>,
        restore: Restore,
    ) -> Result<Accepted, StoreError> {
        let current = match &restore {
            Restore::Environment { id, .. } => Self::checked_environment_on(tx, id, None).await?,
            Restore::Preview { id, .. } => Self::preview_on(tx, id).await?,
        };
        let operation = Self::accept_restore_on(tx, &current, restore.parts().1).await?;
        Ok(Self::operation_accepted(&operation))
    }

    /// The prepared target of `environment`'s deployment `deployment`, which
    /// must have succeeded (`deployment_not_restorable`): what restoring it
    /// deploys, read before acceptance so the caller can make sure its images
    /// are present.
    ///
    /// # Errors
    /// `NotFound` for a deployment of another environment, the restore errors
    /// above, and storage errors.
    pub async fn restorable_target(
        &self,
        environment: &EnvironmentId,
        deployment: &str,
    ) -> Result<ResolvedApplication, StoreError> {
        Self::restorable_on(&mut *self.connection().await?, environment, deployment).await
    }

    /// See [`Self::restorable_target`].
    async fn restorable_on(
        connection: &mut SqliteConnection,
        environment: &EnvironmentId,
        deployment: &str,
    ) -> Result<ResolvedApplication, StoreError> {
        let id = environment.as_str();
        let row = sqlx::query!(
            "SELECT d.succeeded_at_ms,o.target_json FROM deployments d JOIN operations o ON o.id=d.id WHERE d.id=?1 AND d.environment_id=?2",
            deployment,
            id
        )
        .fetch_optional(connection)
        .await
        .map_err(StoreError::database)?
        .ok_or(StoreError::NotFound)?;
        let not_restorable = |reason| RestoreError::NotRestorable {
            deployment: deployment.to_owned(),
            reason,
        };
        if row.succeeded_at_ms.is_none() {
            return Err(not_restorable("it never succeeded").into());
        }
        let target = row
            .target_json
            .ok_or_else(|| not_restorable("it recorded no prepared target"))?;
        serde_json::from_str(&target).map_err(StoreError::corrupt)
    }

    /// Accepts restoring `deployment` into `environment` in `tx`, where
    /// nothing can change underneath: it starts a deployment recorded as a
    /// restore of it, with its rendered manifest, values and release, pins the
    /// secret versions it pinned (failing, naming them, for those gone since),
    /// and saves its prepared target under the application's current name,
    /// with links bound again. A branch-following or promoted environment's
    /// own manifest becomes the restored one. Its images are then retention
    /// roots, so callers holding cleanup off may let go once this commits.
    pub(super) async fn accept_restore_on(
        tx: &mut Transaction<'_, Sqlite>,
        environment: &StoredEnvironment,
        deployment: &str,
    ) -> Result<Operation, StoreError> {
        if environment.delete_intent() {
            return Err(StoreError::IllegalTransition);
        }
        let mut target = Self::restorable_on(tx, environment.id(), deployment).await?;
        let now = now_ms();
        let operation =
            Self::start_operation(tx, environment.id(), OperationKind::Refresh, now).await?;
        Self::write_status(
            tx,
            environment.id().as_str(),
            ApplicationState::Pending,
            now,
        )
        .await?;
        Self::restore_secret_pins_on(tx, &operation, deployment, &target.secret_names).await?;
        let name = environment.application.application.metadata().name.clone();
        target.name = name.clone();
        Box::pin(Self::bind_links_on(tx, &mut target)).await?;
        let origin = serde_json::to_string(&DeploymentOrigin::Restore {
            deployment: deployment.to_owned(),
        })
        .map_err(StoreError::corrupt)?;
        sqlx::query!("INSERT INTO deployments(id,environment_id,manifest_json,template_json,variables_json,generation,created_at_ms,release_id,origin_json) SELECT o.id,o.environment_id,d.manifest_json,d.template_json,d.variables_json,o.generation,o.created_at_ms,d.release_id,?3 FROM operations o JOIN deployments d ON d.id=?2 WHERE o.id=?1",operation.id,deployment,origin)
            .execute(&mut **tx).await.map_err(StoreError::database)?;
        let target = serde_json::to_string(&target).map_err(StoreError::corrupt)?;
        sqlx::query!(
            "UPDATE operations SET target_json=?1 WHERE id=?2",
            target,
            operation.id
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        let template = sqlx::query_scalar!(
            "SELECT template_json FROM deployments WHERE id=?1",
            deployment
        )
        .fetch_one(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        // The deployment keeps the files it captured.
        let template = serde_json::from_str::<ApplicationTemplate>(&template)
            .map_err(StoreError::corrupt)?
            .with_name(name)
            .without_files()
            .canonical_json()
            .map_err(StoreError::corrupt)?;
        let id = environment.id().as_str();
        sqlx::query!(
            "UPDATE environments SET manifest_json=?1,updated_at_ms=?2 WHERE id=?3 AND (branch IS NOT NULL OR promoted_from IS NOT NULL)",
            template,
            now,
            id
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        Self::operation_event(tx, &operation.id, "target_resolved", None, now).await?;
        Ok(operation)
    }
}

#[cfg(test)]
mod tests;
