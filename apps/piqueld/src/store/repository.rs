//! Durable candidate manifests for manual deployments.
//!
//! `deployment_inputs` starts with the configuration captured by Deploy. After
//! fetching, it holds the validated manifest and repository commit; `fetched`
//! tells retries to reuse that snapshot instead of reading a moving branch again.
//! Deployments without repository backing also become fetched, with no commit.
//! The candidate is copied to deployment history when fetched, but only becomes
//! saved application configuration after source preparation succeeds, provided
//! no newer configuration was saved in the meantime.
use super::{ApplicationId, NormalizedApplication, Operation, Store, StoreError, now_ms};
use sqlx::{Sqlite, Transaction};

/// A deployment's candidate manifest and whether it is the fetched snapshot.
pub(crate) struct DeploymentInput {
    pub(crate) application: NormalizedApplication,
    pub(crate) fetched: bool,
    /// Commit the fetched manifest was read from.
    pub(crate) commit: Option<String>,
}

impl Store {
    /// Captures the configuration a Deploy started from, still unfetched.
    pub(crate) async fn insert_deployment_on(
        tx: &mut Transaction<'_, Sqlite>,
        operation: &Operation,
        application: &NormalizedApplication,
    ) -> Result<(), StoreError> {
        let json = application.canonical_json().map_err(StoreError::corrupt)?;
        sqlx::query!(
            "INSERT INTO deployment_inputs(operation_id,application_json) VALUES(?1,?2)",
            operation.id,
            json
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        Ok(())
    }

    /// Loads the operation's candidate manifest, if one was captured.
    pub(crate) async fn deployment_input(
        &self,
        operation: &Operation,
    ) -> Result<Option<DeploymentInput>, StoreError> {
        sqlx::query!(
            "SELECT application_json,fetched,repository_commit FROM deployment_inputs WHERE operation_id=?1",
            operation.id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)?
        .map(|row| {
            Ok(DeploymentInput {
                application: serde_json::from_str(&row.application_json)
                    .map_err(StoreError::corrupt)?,
                fetched: row.fetched != 0,
                commit: row.repository_commit,
            })
        })
        .transpose()
    }

    /// Stores the fetched manifest and commit for the latest running operation,
    /// copying it into deployment history, pinning its secret versions, and
    /// re-checking hostname reservations. Fetching happens once: a second save,
    /// or one for superseded work, fails with `StoreError::IllegalTransition`.
    pub(crate) async fn save_deployment_input(
        &self,
        operation: &Operation,
        application: &NormalizedApplication,
        commit: Option<&str>,
    ) -> Result<(), StoreError> {
        let json = application.canonical_json().map_err(StoreError::corrupt)?;
        self.generate_secrets(&operation.environment_id, application)
            .await?;
        let (_writer, mut tx) = self.begin_immediate().await?;
        let app_id = operation.environment_id.as_str();
        let changed = sqlx::query!("UPDATE deployment_inputs SET application_json=?1,repository_commit=?2,fetched=1 WHERE operation_id=?3 AND fetched=0 AND operation_id=(SELECT id FROM operations WHERE environment_id=?4 ORDER BY created_at_ms DESC,id DESC LIMIT 1) AND EXISTS(SELECT 1 FROM operations WHERE id=?3 AND state='running')", json,commit,operation.id,app_id).execute(&mut *tx).await.map_err(StoreError::database)?.rows_affected();
        if changed != 1 {
            return Err(StoreError::IllegalTransition);
        }
        sqlx::query!(
            "UPDATE deployments SET manifest_json=?1 WHERE id=?2",
            json,
            operation.id
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        Self::operation_event(&mut tx, &operation.id, "manifest_fetched", commit, now_ms()).await?;
        Self::pin_secrets_on(&mut tx, operation, application).await?;
        Self::commit_environment_changes(tx, [app_id]).await
    }

    /// Promotes a fetched candidate to its application's saved configuration,
    /// bumping the application generation and recording `application_applied`.
    /// Skipped when the candidate matches the saved configuration or a newer
    /// save changed the generation since the operation started. Returns the
    /// application whose configuration changed.
    /// Called in the same transaction that saves the fully prepared runtime target.
    pub(super) async fn accept_deployment_on(
        tx: &mut Transaction<'_, Sqlite>,
        operation: &Operation,
    ) -> Result<Option<ApplicationId>, StoreError> {
        let row = sqlx::query!(
            "SELECT application_json FROM deployment_inputs WHERE operation_id=?1 AND fetched=1",
            operation.id
        )
        .fetch_optional(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        let Some(row) = row else {
            return Ok(None);
        };
        let now = now_ms();
        let environment = operation.environment_id.as_str();
        let application = sqlx::query_scalar!(
            r#"SELECT application_id AS "application_id!" FROM environments WHERE id=?1"#,
            environment
        )
        .fetch_one(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        let generation = i64::try_from(operation.generation).map_err(StoreError::corrupt)?;
        let changed = sqlx::query!("UPDATE applications SET desired_json=?1,generation=generation+1,updated_at_ms=?2 WHERE id=?3 AND desired_json!=?1 AND generation=?4", row.application_json,now,application,generation).execute(&mut **tx).await.map_err(StoreError::database)?.rows_affected();
        if changed == 0 {
            return Ok(None);
        }
        sqlx::query!("UPDATE operations SET generation=(SELECT generation FROM applications WHERE id=?1) WHERE id=?2", application,operation.id).execute(&mut **tx).await.map_err(StoreError::database)?;
        sqlx::query!("UPDATE deployments SET generation=(SELECT generation FROM operations WHERE id=?1) WHERE id=?1",operation.id).execute(&mut **tx).await.map_err(StoreError::database)?;
        Self::operation_event(tx, &operation.id, "application_applied", None, now).await?;
        ApplicationId::parse(application)
            .map(Some)
            .map_err(StoreError::corrupt)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Mutation, MutationResponse};
    use piqueld_core::{ApplicationId, InstanceId, OperationState, ResolutionSet};

    #[tokio::test]
    async fn fetched_snapshot_preserves_intervening_saved_configuration() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("db")).await.unwrap();
        let initial = piqueld_core::parse_toml("api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='test'\n[spec.manifest]\npath='app.toml'\n[spec.manifest.repository]\nurl='https://example.com/app.git'\nbranch='main'").unwrap().normalize(ApplicationId::parse("test-app").unwrap());
        let (MutationResponse::Saved(saved), _) = store
            .accept(
                Mutation::Save {
                    application: Box::new(initial),
                    expected_application_id: None,
                    deploy: true,
                },
                Some(0),
                false,
                None,
            )
            .await
            .unwrap()
        else {
            panic!("saved")
        };
        let op = store.operation(&saved.operation_id.unwrap()).await.unwrap();
        store
            .transition_operation(
                &op.id,
                OperationState::Requested,
                OperationState::Running,
                None,
            )
            .await
            .unwrap();
        let captured = store.deployment_manifest(&op.id).await.unwrap();
        let mut fetched = captured.to_manifest();
        fetched.spec.manifest = None;
        let fetched = fetched.validate().unwrap().normalize(captured.id().clone());
        store
            .save_deployment_input(&op, &fetched, Some(&"a".repeat(40)))
            .await
            .unwrap();
        let mut edited = captured.to_manifest();
        edited.spec.manifest.as_mut().unwrap().path = "fixed.toml".into();
        let edited = edited.validate().unwrap().normalize(captured.id().clone());
        store
            .accept(
                Mutation::Save {
                    application: Box::new(edited.clone()),
                    expected_application_id: Some(op.environment_id.to_string()),
                    deploy: false,
                },
                Some(1),
                false,
                None,
            )
            .await
            .unwrap();
        let target = piqueld_core::compile_application(
            &fetched,
            &op.environment_id,
            InstanceId::parse(store.instance_id()).unwrap(),
            &ResolutionSet::default(),
        )
        .unwrap();
        store.save_prepared(&op, &target).await.unwrap();
        assert_eq!(
            store
                .get(&op.environment_id)
                .await
                .unwrap()
                .application
                .application,
            edited
        );
        assert_eq!(
            store
                .get(&op.environment_id)
                .await
                .unwrap()
                .application
                .generation,
            2
        );
        assert_eq!(store.operation(&op.id).await.unwrap().generation, 1);
        assert_eq!(store.deployment_manifest(&op.id).await.unwrap(), fetched);
        assert_eq!(
            store
                .deployments(&op.environment_id, None, 3)
                .await
                .unwrap()
                .items[0]
                .application,
            fetched
        );
        assert!(matches!(
            store.save_deployment_input(&op, &edited, None).await,
            Err(StoreError::IllegalTransition)
        ));
    }
}
