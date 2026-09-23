//! Durable candidate manifests for manual deployments.
//!
//! `deployment_inputs` starts with the configuration captured by Deploy. After
//! fetching, it holds the validated manifest and repository commit; `fetched`
//! tells retries to reuse that snapshot instead of reading a moving branch again.
//! Deployments without repository backing also become fetched, with no commit.
//! The candidate is copied to deployment history when fetched, but only becomes
//! saved application configuration after source preparation succeeds, provided
//! no newer configuration was saved in the meantime.
use super::{NormalizedApplication, Operation, Store, StoreError, now_ms};
use sqlx::{Sqlite, Transaction};

pub(crate) struct DeploymentInput {
    pub(crate) application: NormalizedApplication,
    pub(crate) fetched: bool,
}

impl Store {
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

    pub(crate) async fn deployment_input(
        &self,
        operation: &Operation,
    ) -> Result<Option<DeploymentInput>, StoreError> {
        sqlx::query!(
            "SELECT application_json,fetched FROM deployment_inputs WHERE operation_id=?1",
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
            })
        })
        .transpose()
    }

    pub(crate) async fn save_deployment_input(
        &self,
        operation: &Operation,
        application: &NormalizedApplication,
        commit: Option<&str>,
    ) -> Result<(), StoreError> {
        let json = application.canonical_json().map_err(StoreError::corrupt)?;
        let (_writer, mut tx) = self.begin_immediate().await?;
        let app_id = operation.application_id.as_str();
        let changed = sqlx::query!("UPDATE deployment_inputs SET application_json=?1,repository_commit=?2,fetched=1 WHERE operation_id=?3 AND fetched=0 AND operation_id=(SELECT id FROM operations WHERE application_id=?4 ORDER BY created_at_ms DESC,id DESC LIMIT 1) AND EXISTS(SELECT 1 FROM operations WHERE id=?3 AND state='running')", json,commit,operation.id,app_id).execute(&mut *tx).await.map_err(StoreError::database)?.rows_affected();
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
        tx.commit().await.map_err(StoreError::database)
    }

    // Called in the same transaction that saves the fully prepared runtime target.
    pub(super) async fn accept_deployment_on(
        tx: &mut Transaction<'_, Sqlite>,
        operation: &Operation,
    ) -> Result<(), StoreError> {
        let row = sqlx::query!(
            "SELECT application_json FROM deployment_inputs WHERE operation_id=?1 AND fetched=1",
            operation.id
        )
        .fetch_optional(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        if let Some(row) = row {
            let now = now_ms();
            let app_id = operation.application_id.as_str();
            let generation = i64::try_from(operation.generation).map_err(StoreError::corrupt)?;
            let changed = sqlx::query!("UPDATE applications SET desired_json=?1,generation=generation+1,updated_at_ms=?2 WHERE id=?3 AND desired_json!=?1 AND deleted_at_ms IS NULL AND generation=?4", row.application_json,now,app_id,generation).execute(&mut **tx).await.map_err(StoreError::database)?.rows_affected();
            if changed != 0 {
                sqlx::query!("UPDATE operations SET generation=(SELECT generation FROM applications WHERE id=?1) WHERE id=?2", app_id,operation.id).execute(&mut **tx).await.map_err(StoreError::database)?;
                sqlx::query!("UPDATE deployments SET generation=(SELECT generation FROM operations WHERE id=?1) WHERE id=?1",operation.id).execute(&mut **tx).await.map_err(StoreError::database)?;
                Self::operation_event(tx, &operation.id, "application_applied", None, now).await?;
            }
        }
        Ok(())
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
                    expected_application_id: Some(op.application_id.to_string()),
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
            InstanceId::parse(store.instance_id()).unwrap(),
            &ResolutionSet::default(),
        )
        .unwrap();
        store.save_prepared(&op, &target).await.unwrap();
        assert_eq!(
            store.get(&op.application_id).await.unwrap().application,
            edited
        );
        assert_eq!(store.get(&op.application_id).await.unwrap().generation, 2);
        assert_eq!(store.operation(&op.id).await.unwrap().generation, 1);
        assert_eq!(store.deployment_manifest(&op.id).await.unwrap(), fetched);
        assert_eq!(
            store
                .deployments(&op.application_id, None, 3)
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
