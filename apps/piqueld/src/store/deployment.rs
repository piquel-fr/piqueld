//! Saved configuration and immutable deployment inputs. Saving never creates work.
use super::{EnvironmentId, NormalizedApplication, Operation, Store, StoreError, now_ms};
use piqueld_core::{
    api::{DeploymentView, DiagnosticView, Page, SavedApplication},
    manifest::{ApplicationTemplate, Rendering, VariableValue},
};
use sqlx::{Sqlite, Transaction};
use std::collections::BTreeMap;

/// `deployments` row shared by the first-page and cursor page queries.
struct DeploymentRow {
    id: String,
    manifest_json: String,
    template_json: String,
    variables_json: String,
    warnings_json: String,
    succeeded_at_ms: Option<i64>,
}

/// A deployment's captured inputs.
pub(crate) struct Snapshot {
    /// The manifest as captured, with references unresolved.
    pub(crate) template: ApplicationTemplate,
    /// The rendered manifest and its values; absent until a
    /// repository-backed manifest is fetched.
    pub(crate) rendering: Option<Rendering>,
    /// Problems found while fetching the manifest that did not stop the deployment.
    pub(crate) warnings: Vec<DiagnosticView>,
}

impl DeploymentRow {
    /// Decodes the captured columns.
    fn snapshot(&self) -> Result<Snapshot, StoreError> {
        let application: Option<NormalizedApplication> =
            serde_json::from_str(&self.manifest_json).map_err(StoreError::corrupt)?;
        let values: BTreeMap<String, VariableValue> =
            serde_json::from_str(&self.variables_json).map_err(StoreError::corrupt)?;
        Ok(Snapshot {
            template: serde_json::from_str(&self.template_json).map_err(StoreError::corrupt)?,
            rendering: application.map(|application| Rendering {
                application,
                values,
            }),
            warnings: serde_json::from_str(&self.warnings_json).map_err(StoreError::corrupt)?,
        })
    }
}

impl Store {
    /// Saves edited configuration with the next generation without creating an
    /// operation, so nothing is deployed until requested. New applications get a
    /// `production` environment that starts as `not_deployed`. Returns
    /// `IllegalTransition` while deletion is pending, `AlreadyExists` for a taken
    /// name, `SecretDeleting` for manifests that reference secrets being deleted,
    /// and `secret_name_conflict` for declared secrets the application stores.
    pub(super) async fn save_configuration_on(
        tx: &mut Transaction<'_, Sqlite>,
        app: &ApplicationTemplate,
        expected: Option<u64>,
    ) -> Result<SavedApplication, StoreError> {
        let id = app.id().as_str();
        let mounted = app
            .spec()
            .services
            .iter()
            .flat_map(|service| &service.secrets)
            .filter_map(|secret| secret.name.as_literal())
            .collect::<Vec<_>>();
        Self::check_secret_references(
            tx,
            &mounted.iter().map(String::as_str).collect(),
            super::secret::SecretScope::Application(app.id()),
        )
        .await?;
        Self::check_declared_against_store_on(tx, id, &app.spec().secrets).await?;
        let previous = Self::generation_on(tx, id, expected).await?;
        let generation = previous.checked_add(1).ok_or(StoreError::InvalidInput)?;
        let json = serde_json::to_string(app).map_err(StoreError::corrupt)?;
        let name = app.metadata().name.as_str();
        let now = now_ms();
        let changed = sqlx::query!("INSERT INTO applications(id,name,desired_json,generation,created_at_ms,updated_at_ms) VALUES(?1,?2,?3,?4,?5,?5) ON CONFLICT(id) DO UPDATE SET desired_json=excluded.desired_json,generation=excluded.generation,updated_at_ms=excluded.updated_at_ms WHERE applications.delete_intent=0",id,name,json,generation,now)
            .execute(&mut **tx).await.map_err(StoreError::constraint)?.rows_affected();
        if changed != 1 {
            return Err(StoreError::IllegalTransition);
        }
        if previous == 0 {
            Self::create_default_environment_on(tx, app, now).await?;
        }
        Self::follow_connection_on(tx, app).await?;
        Ok(SavedApplication {
            application_id: id.into(),
            generation: u64::try_from(generation).map_err(StoreError::corrupt)?,
            operation_id: None,
        })
    }

    /// Snapshots the manifest `environment` deploys next (see
    /// [`super::StoredEnvironment::candidate`]) as operation `id`'s deployment
    /// record, rendered for that environment, together with the values its
    /// references resolved to, so retries never read variables again. A
    /// reference without a value, or a stored secret the environment may not
    /// mount (`SecretAccessDenied`), fails the request. Repository-backed
    /// manifests are rendered and checked once fetched, when their revision is known.
    /// The deployment row shares the operation's ID.
    pub(super) async fn capture_deployment(
        tx: &mut Transaction<'_, Sqlite>,
        id: &str,
        environment: &EnvironmentId,
    ) -> Result<(), StoreError> {
        let environment = Self::environment_on(tx, environment.as_str())
            .await?
            .ok_or(StoreError::NotFound)?;
        let template = environment.candidate(None)?;
        let rendering = if template.spec().manifest.is_some() {
            None
        } else {
            let rendering = environment.render(&template, id)?;
            Self::check_secret_access_on(tx, &environment.environment, &rendering.application)
                .await?;
            Some(rendering)
        };
        let (manifest, variables) = Self::snapshot_json(rendering.as_ref())?;
        let template = template.canonical_json().map_err(StoreError::corrupt)?;
        sqlx::query!("INSERT INTO deployments(id,environment_id,manifest_json,template_json,variables_json,generation,created_at_ms) SELECT id,environment_id,?2,?3,?4,generation,created_at_ms FROM operations WHERE id=?1",id,manifest,template,variables)
            .execute(&mut **tx).await.map_err(StoreError::database)?;
        Ok(())
    }

    /// The rendered manifest (JSON null when absent) and values columns.
    pub(super) fn snapshot_json(
        rendering: Option<&Rendering>,
    ) -> Result<(String, String), StoreError> {
        let manifest = rendering
            .map(|rendering| rendering.application.canonical_json())
            .transpose()
            .map_err(StoreError::corrupt)?
            .unwrap_or_else(|| "null".into());
        let variables = rendering
            .map(|rendering| serde_json::to_string(&rendering.values))
            .transpose()
            .map_err(StoreError::corrupt)?
            .unwrap_or_else(|| "{}".into());
        Ok((manifest, variables))
    }

    /// Reads the inputs captured for an execution, never the editable application.
    /// # Errors
    /// Returns storage, decoding, or absence errors.
    pub(crate) async fn deployment_snapshot(&self, id: &str) -> Result<Snapshot, StoreError> {
        sqlx::query_as!(
            DeploymentRow,
            r#"SELECT id AS "id!",manifest_json,template_json,variables_json,warnings_json,succeeded_at_ms FROM deployments WHERE id=?1"#,
            id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)?
        .ok_or(StoreError::NotFound)?
        .snapshot()
    }

    /// Loads operation `id` and records its current attempt outcome.
    pub(super) async fn record_deployment_attempt(
        tx: &mut Transaction<'_, Sqlite>,
        id: &str,
    ) -> Result<(), StoreError> {
        let op = Self::operation_on(tx, id).await?;
        Self::save_deployment_attempt(tx, &op).await
    }

    /// Upserts the outcome of the operation's current attempt in deployment
    /// history, and stamps the deployment's first success time when it succeeded.
    /// Operations without a deployment row (deletes) record nothing.
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

    /// Lists deployment snapshots newest first; history is retained until environment deletion.
    /// # Errors
    /// Returns storage, decoding, absence, or invalid cursor errors.
    pub async fn deployments(
        &self,
        app: &EnvironmentId,
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
        let mut rows = if let Some(before) = before {
            sqlx::query_as!(
                DeploymentRow,
                "SELECT id AS \"id!\",manifest_json,template_json,variables_json,warnings_json,succeeded_at_ms FROM deployments
                 WHERE environment_id=?1 AND id<?2 ORDER BY id DESC LIMIT ?3",
                app_id,
                before,
                limit_sql
            )
            .fetch_all(&mut *tx)
            .await
        } else {
            sqlx::query_as!(
                DeploymentRow,
                "SELECT id AS \"id!\",manifest_json,template_json,variables_json,warnings_json,succeeded_at_ms FROM deployments
                 WHERE environment_id=?1 ORDER BY id DESC LIMIT ?2",
                app_id,
                limit_sql
            )
            .fetch_all(&mut *tx)
            .await
        }
        .map_err(StoreError::database)?;
        let more = rows.len() > limit;
        rows.truncate(limit);
        let next_cursor = more
            .then(|| rows.last().map(|row| format!("v1:{}", row.id)))
            .flatten();
        let current = sqlx::query_scalar!("SELECT d.id FROM deployments d JOIN operations o ON o.id=d.id WHERE d.environment_id=?1 AND o.promoted=1 ORDER BY d.id DESC LIMIT 1",app_id).fetch_optional(&mut *tx).await.map_err(StoreError::database)?.flatten();
        let successful = sqlx::query_scalar!("SELECT id FROM deployments WHERE environment_id=?1 AND succeeded_at_ms IS NOT NULL ORDER BY id DESC LIMIT 1",app_id).fetch_optional(&mut *tx).await.map_err(StoreError::database)?.flatten();
        let mut items = Vec::with_capacity(rows.len());
        for row in rows {
            let snapshot = row.snapshot()?;
            let (application, variables) = snapshot.rendering.map_or_else(
                || (None, BTreeMap::new()),
                |rendering| (Some(rendering.application), rendering.values),
            );
            items.push(DeploymentView {
                operation: Self::operation_on(&mut tx, &row.id).await?,
                template: snapshot.template,
                variables,
                application,
                warnings: snapshot.warnings,
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
    use crate::api::Actor::Daemon;
    use crate::api::{Mutation, MutationResponse};
    use piqueld_core::{ApplicationState, OperationState};

    fn empty() -> ApplicationTemplate {
        piqueld_core::manifest::parse_template_toml("api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='empty'\n[spec]").unwrap().normalize(piqueld_core::ApplicationId::parse("app-empty-test").unwrap())
    }

    /// The environment created with a saved application, which shares its ID.
    fn environment(saved: &SavedApplication) -> EnvironmentId {
        EnvironmentId::parse(saved.application_id.as_str()).unwrap()
    }

    async fn save(store: &Store, app: ApplicationTemplate, generation: u64) -> SavedApplication {
        let (MutationResponse::Saved(saved), wake) = store
            .accept(
                Daemon,
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

    /// Requests a deployment of `id`, returning it and whether the controller should wake.
    async fn deploy(
        store: &Store,
        id: &EnvironmentId,
        generation: u64,
        key: Option<&str>,
    ) -> (piqueld_core::api::AcceptedOperation, bool) {
        let (MutationResponse::Operation(operation), wake) = store
            .accept(
                Daemon,
                Mutation::deploy(id.clone()),
                Some(generation),
                false,
                key,
            )
            .await
            .unwrap()
        else {
            panic!("operation response")
        };
        (operation, wake)
    }

    #[tokio::test]
    async fn save_and_deploy_accepts_saved_only_configuration_but_cannot_reverse_deletion() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("db")).await.unwrap();
        let saved = save(&store, empty(), 0).await;
        let id = environment(&saved);
        let application = store.get(&id).await.unwrap().application.application;
        let (MutationResponse::Saved(deployed), wake) = store
            .accept(
                Daemon,
                Mutation::Save {
                    application: Box::new(application.clone()),
                    expected_application_id: Some(saved.application_id.clone()),
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
            store
                .deployment_snapshot(&operation_id)
                .await
                .unwrap()
                .template,
            application
        );
        let (MutationResponse::Deleted(deletion), _) = store
            .accept(
                Daemon,
                Mutation::DeleteApplication {
                    id: application.id().clone(),
                    environments: Vec::new(),
                },
                Some(deployed.generation),
                false,
                None,
            )
            .await
            .unwrap()
        else {
            panic!("deleted")
        };
        for deploy in [false, true] {
            assert!(matches!(
                store
                    .accept(
                        Daemon,
                        Mutation::Save {
                            application: Box::new(application.clone()),
                            expected_application_id: Some(saved.application_id.clone()),
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
        assert!(current.delete_intent() && current.application.delete_intent);
        assert_eq!(current.application.generation, deletion.generation);
        assert_eq!(
            store
                .latest_operation_for_environment(&id)
                .await
                .unwrap()
                .unwrap()
                .id,
            deletion.operations[0].operation_id
        );
    }

    #[tokio::test]
    async fn saved_changes_cannot_enter_deployments_or_retries_after_restart() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("db");
        let store = Store::open(&path).await.unwrap();
        let saved = save(&store, empty(), 0).await;
        let id = environment(&saved);
        assert_eq!(
            store.status(&id).await.unwrap().state,
            ApplicationState::NotDeployed
        );
        assert!(
            store
                .latest_operation_for_environment(&id)
                .await
                .unwrap()
                .is_none()
        );
        let (first, wake) = deploy(&store, &id, saved.generation, Some("deploy-once")).await;
        assert!(wake);
        let original = store
            .deployment_snapshot(&first.operation_id)
            .await
            .unwrap()
            .template;
        store
            .transition_operation(
                &first.operation_id,
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
        let edited = edited
            .validate_template()
            .unwrap()
            .normalize(original.id().clone());
        let changed = save(&store, edited, 1).await;
        assert_eq!(changed.generation, 2);
        drop(store);
        let store = Store::open(&path).await.unwrap();
        store.recover_interrupted().await.unwrap();
        let op = store.operation(&first.operation_id).await.unwrap();
        let attempts = store.deployment_attempts(&op.id, None, 100).await.unwrap();
        assert_eq!(attempts.items.len(), 1);
        assert_eq!(attempts.items[0].attempt, 1);
        assert_eq!(attempts.items[0].state, OperationState::Cancelled);
        assert!(attempts.items[0].finished_at_ms.is_some());
        assert_eq!(store.recover_interrupted().await.unwrap(), 0);
        assert_eq!(op.generation, 1);
        assert_eq!(
            store.deployment_snapshot(&op.id).await.unwrap().template,
            original
        );
        let (replay, wake) = deploy(&store, &id, 1, Some("deploy-once")).await;
        assert!(!wake);
        assert_eq!(replay.operation_id, op.id);
        let (next, _) = deploy(&store, &id, 2, None).await;
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
        let id = environment(&saved);
        let op = store.request_deploy(&id, Some(1)).await.unwrap();
        let target = piqueld_core::compile_application(
            &store
                .deployment_snapshot(&op.id)
                .await
                .unwrap()
                .rendering
                .unwrap()
                .application,
            &id,
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
