//! Saved configuration and immutable deployment inputs. Saving never creates work.
use super::{ApplicationId, NormalizedApplication, Operation, Store, StoreError, now_ms};
use piqueld_core::api::{DeploymentView, Page, SavedApplication};
use sqlx::{Sqlite, Transaction};

impl Store {
    pub(super) async fn save_configuration_on(
        tx: &mut Transaction<'_, Sqlite>,
        app: &NormalizedApplication,
        expected: Option<u64>,
    ) -> Result<SavedApplication, StoreError> {
        let id = app.id().as_str();
        let previous = Self::generation_on(tx, id, expected).await?;
        let generation = previous.checked_add(1).ok_or(StoreError::InvalidInput)?;
        let json = serde_json::to_string(app).map_err(StoreError::corrupt)?;
        let name = app.metadata().name.as_str();
        let now = now_ms();
        let changed = sqlx::query!("INSERT INTO applications(id,name,desired_json,generation,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,?5,?5) ON CONFLICT(id) DO UPDATE SET desired_json=excluded.desired_json,generation=excluded.generation,updated_at_ms=excluded.updated_at_ms WHERE applications.delete_intent=0",id,name,json,generation,now)
            .execute(&mut **tx).await.map_err(|error| if error.as_database_error().is_some_and(sqlx::error::DatabaseError::is_unique_violation) {StoreError::AlreadyExists} else {StoreError::database(error)})?.rows_affected();
        if changed != 1 {
            return Err(StoreError::IllegalTransition);
        }
        if previous == 0 {
            Self::write_status(tx, id, "not_deployed", None, now).await?;
        }
        Ok(SavedApplication {
            application_id: id.into(),
            generation: u64::try_from(generation).map_err(StoreError::corrupt)?,
            operation_id: None,
        })
    }

    pub(super) async fn capture_deployment(
        tx: &mut Transaction<'_, Sqlite>,
        id: &str,
    ) -> Result<(), StoreError> {
        sqlx::query!("INSERT INTO deployments(id,application_id,manifest_json,generation,created_at_ms) SELECT o.id,a.id,a.desired_json,o.generation,o.created_at_ms FROM operations o JOIN applications a ON a.id=o.application_id WHERE o.id=?1",id)
            .execute(&mut **tx).await.map_err(StoreError::database)?;
        Ok(())
    }

    /// Reads the input captured for an execution, never the editable application.
    /// # Errors
    /// Returns storage, decoding, or absence errors.
    pub async fn deployment_manifest(&self, id: &str) -> Result<NormalizedApplication, StoreError> {
        let json = sqlx::query_scalar!("SELECT manifest_json FROM deployments WHERE id=?1", id)
            .fetch_optional(&self.pool)
            .await
            .map_err(StoreError::database)?
            .ok_or(StoreError::NotFound)?;
        serde_json::from_str(&json).map_err(StoreError::corrupt)
    }

    pub(super) async fn record_deployment_attempt(
        tx: &mut Transaction<'_, Sqlite>,
        id: &str,
    ) -> Result<(), StoreError> {
        let op = Self::operation_on(tx, id).await?;
        Self::save_deployment_attempt(tx, &op).await
    }

    pub(super) async fn save_deployment_attempt(
        tx: &mut Transaction<'_, Sqlite>,
        op: &Operation,
    ) -> Result<(), StoreError> {
        let id = &op.id;
        let json = serde_json::to_string(op).map_err(StoreError::corrupt)?;
        let attempt = i64::try_from(op.attempt).map_err(StoreError::corrupt)?;
        sqlx::query!("INSERT INTO deployment_attempts(deployment_id,attempt,outcome_json) SELECT id,?1,?2 FROM deployments WHERE id=?3 ON CONFLICT(deployment_id,attempt) DO UPDATE SET outcome_json=excluded.outcome_json",attempt,json,id).execute(&mut **tx).await.map_err(StoreError::database)?;
        if op.state == super::OperationState::Succeeded {
            sqlx::query!(
                "UPDATE deployments SET succeeded_at_ms=COALESCE(succeeded_at_ms,?1) WHERE id=?2",
                op.finished_at_ms,
                id
            )
            .execute(&mut **tx)
            .await
            .map_err(StoreError::database)?;
        }
        Ok(())
    }

    /// Lists deployment snapshots newest first; history is retained until application deletion.
    /// # Errors
    /// Returns storage, decoding, absence, or invalid cursor errors.
    pub async fn deployments(
        &self,
        app: &ApplicationId,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Page<DeploymentView>, StoreError> {
        self.get(app).await?;
        let limit_sql = super::page_limit(limit)? + 1;
        let before = cursor
            .map(|v| v.strip_prefix("v1:").ok_or(StoreError::InvalidInput))
            .transpose()?;
        let app_id = app.as_str();
        let mut tx = self.pool.begin().await.map_err(StoreError::database)?;
        let mut rows = sqlx::query!("SELECT d.id AS \"id!\",d.manifest_json,d.succeeded_at_ms FROM deployments d WHERE d.application_id=?1 AND (?2 IS NULL OR d.id<?2) ORDER BY d.id DESC LIMIT ?3",app_id,before,limit_sql).fetch_all(&mut *tx).await.map_err(StoreError::database)?;
        let more = rows.len() > limit;
        rows.truncate(limit);
        let next_cursor = more
            .then(|| rows.last().map(|row| format!("v1:{}", row.id)))
            .flatten();
        let current = sqlx::query_scalar!("SELECT d.id FROM deployments d JOIN operations o ON o.id=d.id WHERE d.application_id=?1 AND o.promoted=1 ORDER BY d.id DESC LIMIT 1",app_id).fetch_optional(&mut *tx).await.map_err(StoreError::database)?.flatten();
        let successful = sqlx::query_scalar!("SELECT id FROM deployments WHERE application_id=?1 AND succeeded_at_ms IS NOT NULL ORDER BY id DESC LIMIT 1",app_id).fetch_optional(&mut *tx).await.map_err(StoreError::database)?.flatten();
        let mut items = Vec::with_capacity(rows.len());
        for row in rows {
            items.push(DeploymentView {
                operation: Self::operation_on(&mut tx, &row.id).await?,
                application: serde_json::from_str(&row.manifest_json)
                    .map_err(StoreError::corrupt)?,
                succeeded_at_ms: row.succeeded_at_ms,
                current_target: current.as_ref() == Some(&row.id),
                last_successful: successful.as_ref() == Some(&row.id),
            });
        }
        tx.commit().await.map_err(StoreError::database)?;
        Ok(Page { items, next_cursor })
    }

    /// Lists durable attempt outcomes, including failures followed by successful retries.
    /// # Errors
    /// Returns storage, decoding or pagination errors.
    pub async fn deployment_attempts(
        &self,
        id: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<Page<Operation>, StoreError> {
        let before = cursor
            .map(|v| {
                v.strip_prefix("v1:")
                    .ok_or(StoreError::InvalidInput)?
                    .parse::<i64>()
                    .map_err(StoreError::invalid_input)
            })
            .transpose()?
            .unwrap_or(i64::MAX);
        let limit_sql = super::page_limit(limit)? + 1;
        let mut rows=sqlx::query!("SELECT attempt,outcome_json FROM deployment_attempts WHERE deployment_id=?1 AND attempt<?2 ORDER BY attempt DESC LIMIT ?3",id,before,limit_sql).fetch_all(&self.pool).await.map_err(StoreError::database)?;
        let more = rows.len() > limit;
        rows.truncate(limit);
        let next_cursor = more
            .then(|| rows.last().map(|row| format!("v1:{}", row.attempt)))
            .flatten();
        let items = rows
            .into_iter()
            .map(|row| serde_json::from_str(&row.outcome_json).map_err(StoreError::corrupt))
            .collect::<Result<_, _>>()?;
        Ok(Page { items, next_cursor })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Mutation, MutationResponse};
    use piqueld_core::{ApplicationState, OperationState};

    fn empty() -> NormalizedApplication {
        piqueld_core::parse_toml("api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='empty'\n[spec]").unwrap().normalize(ApplicationId::parse("app-empty-test").unwrap())
    }

    async fn save(store: &Store, app: NormalizedApplication, generation: u64) -> SavedApplication {
        let (MutationResponse::Saved(saved), wake) = store
            .accept(
                Mutation::Save {
                    application: Box::new(app),
                    expected_application_id: None,
                    deploy: false,
                },
                Some(generation),
                false,
                None,
            )
            .await
            .unwrap()
        else {
            panic!("saved response")
        };
        assert!(!wake);
        saved
    }

    #[tokio::test]
    async fn save_and_deploy_accepts_saved_only_configuration_but_cannot_reverse_deletion() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("db")).await.unwrap();
        let saved = save(&store, empty(), 0).await;
        let id = ApplicationId::parse(&saved.application_id).unwrap();
        let application = store.get(&id).await.unwrap().application;
        let (MutationResponse::Saved(deployed), wake) = store
            .accept(
                Mutation::Save {
                    application: Box::new(application.clone()),
                    expected_application_id: Some(saved.application_id),
                    deploy: true,
                },
                Some(saved.generation),
                false,
                None,
            )
            .await
            .unwrap()
        else {
            panic!("saved deployment")
        };
        assert!(wake);
        let operation_id = deployed.operation_id.unwrap();
        assert_eq!(
            store.deployment_manifest(&operation_id).await.unwrap(),
            application
        );
        let deletion = store
            .request_delete(&id, Some(deployed.generation))
            .await
            .unwrap();
        for deploy in [false, true] {
            assert!(matches!(
                store
                    .accept(
                        Mutation::Save {
                            application: Box::new(application.clone()),
                            expected_application_id: Some(id.to_string()),
                            deploy,
                        },
                        Some(deletion.generation),
                        false,
                        None,
                    )
                    .await,
                Err(StoreError::IllegalTransition)
            ));
        }
        assert!(matches!(
            store
                .save_application(&application, None, Some(deletion.generation))
                .await,
            Err(StoreError::IllegalTransition)
        ));
        let current = store.get(&id).await.unwrap();
        assert!(current.delete_intent);
        assert_eq!(current.generation, deletion.generation);
        assert_eq!(
            store
                .latest_operation_for_application(&id)
                .await
                .unwrap()
                .unwrap()
                .id,
            deletion.id
        );
    }

    #[tokio::test]
    async fn saved_changes_cannot_enter_deployments_or_retries_after_restart() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("db");
        let store = Store::open(&path).await.unwrap();
        let saved = save(&store, empty(), 0).await;
        let id = ApplicationId::parse(&saved.application_id).unwrap();
        assert_eq!(
            store.status(&id).await.unwrap().state,
            ApplicationState::NotDeployed
        );
        assert!(
            store
                .latest_operation_for_application(&id)
                .await
                .unwrap()
                .is_none()
        );
        let (MutationResponse::Operation(deploy), wake) = store
            .accept(
                Mutation::Deploy { id: id.clone() },
                Some(saved.generation),
                false,
                Some("deploy-once"),
            )
            .await
            .unwrap()
        else {
            panic!("deployment")
        };
        assert!(wake);
        let original = store
            .deployment_manifest(&deploy.operation_id)
            .await
            .unwrap();
        store
            .transition_operation(
                &deploy.operation_id,
                OperationState::Requested,
                OperationState::Running,
                None,
            )
            .await
            .unwrap();
        let mut edited = original.to_manifest();
        edited.spec.volumes.push(piqueld_core::manifest::Volume {
            name: "later".into(),
        });
        let edited = edited.validate().unwrap().normalize(original.id().clone());
        let changed = save(&store, edited, 1).await;
        assert_eq!(changed.generation, 2);
        drop(store);
        let store = Store::open(&path).await.unwrap();
        store.recover_interrupted().await.unwrap();
        let op = store.operation(&deploy.operation_id).await.unwrap();
        let attempts = store.deployment_attempts(&op.id, None, 100).await.unwrap();
        assert_eq!(attempts.items.len(), 1);
        assert_eq!(attempts.items[0].attempt, 1);
        assert_eq!(attempts.items[0].state, OperationState::Cancelled);
        assert!(attempts.items[0].finished_at_ms.is_some());
        assert_eq!(store.recover_interrupted().await.unwrap(), 0);
        assert_eq!(op.generation, 1);
        assert_eq!(store.deployment_manifest(&op.id).await.unwrap(), original);
        let (MutationResponse::Operation(replay), wake) = store
            .accept(
                Mutation::Deploy { id: id.clone() },
                Some(1),
                false,
                Some("deploy-once"),
            )
            .await
            .unwrap()
        else {
            panic!("replay")
        };
        assert!(!wake);
        assert_eq!(replay.operation_id, op.id);
        let (MutationResponse::Operation(next), _) = store
            .accept(Mutation::Deploy { id: id.clone() }, Some(2), false, None)
            .await
            .unwrap()
        else {
            panic!("deployment")
        };
        assert_ne!(next.operation_id, op.id);
        assert!(matches!(
            store.retry_operation(&op).await,
            Err(StoreError::IllegalTransition)
        ));
        assert_eq!(
            store.deployments(&id, None, 3).await.unwrap().items.len(),
            2
        );
        assert_eq!(store.prune_finished_operations(i64::MAX).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn failure_survives_success_and_empty_target_has_no_resources() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("db")).await.unwrap();
        let saved = save(&store, empty(), 0).await;
        let id = ApplicationId::parse(saved.application_id).unwrap();
        let op = store.request_deploy(&id, Some(1)).await.unwrap();
        let target = piqueld_core::compile_application(
            &store.deployment_manifest(&op.id).await.unwrap(),
            piqueld_core::InstanceId::parse(store.instance_id()).unwrap(),
            &piqueld_core::ResolutionSet::default(),
        )
        .unwrap();
        assert!(
            target.services.is_empty() && target.networks.is_empty() && target.volumes.is_empty()
        );
        store
            .transition_operation(
                &op.id,
                OperationState::Requested,
                OperationState::Running,
                None,
            )
            .await
            .unwrap();
        store
            .transition_operation(
                &op.id,
                OperationState::Running,
                OperationState::Failed,
                Some(("docker_unavailable", "Docker is unavailable")),
            )
            .await
            .unwrap();
        let op = store.operation(&op.id).await.unwrap();
        store.retry_operation(&op).await.unwrap();
        store
            .transition_operation(
                &op.id,
                OperationState::Requested,
                OperationState::Running,
                None,
            )
            .await
            .unwrap();
        store.save_prepared(&op, &target).await.unwrap();
        store.publish_prepared(&op).await.unwrap();
        store
            .transition_operation(
                &op.id,
                OperationState::Running,
                OperationState::Succeeded,
                None,
            )
            .await
            .unwrap();
        store.prune_events(i64::MAX).await.unwrap();
        let history = store.deployment_attempts(&op.id, None, 100).await.unwrap();
        assert_eq!(history.items.len(), 2);
        assert_eq!(
            history.items[1].error_code.as_deref(),
            Some("docker_unavailable")
        );
        let deployments = store.deployments(&id, None, 3).await.unwrap();
        assert!(deployments.items[0].current_target && deployments.items[0].last_successful);
    }
}
