//! Atomic acceptance: compare current intent, write the change and its replay receipt together.
use super::{ApplicationRow, OperationKind, Store, StoreError, now_ms};
use super::{Operation, StoredApplication};
use crate::api::{Mutation, MutationResponse};
use piqueld_core::ApplicationId;
use piqueld_core::api::{AcceptedOperation, RenamedApplication};
use sha2::{Digest, Sha256};
use sqlx::{Sqlite, SqliteConnection, Transaction};

impl Store {
    /// Commits a mutation and its optional replay receipt in one transaction.
    /// `expected_generation` compares the inspected intent revision; zero means
    /// the application must be absent, and `None` omits the revision check.
    /// `request_id` is a validated caller-supplied idempotency key, distinct from
    /// the server-generated operation ID (rename does not create an operation).
    pub(crate) async fn accept(
        &self,
        mut mutation: Mutation,
        mut expected_generation: Option<u64>,
        force: bool,
        request_id: Option<&str>,
    ) -> Result<(MutationResponse, bool), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let now = now_ms();
        let fingerprint = Self::mutation_fingerprint(&mutation, expected_generation, force)?;
        if let Some(response) = Self::replay_on(&mut tx, request_id, &fingerprint, now).await? {
            return Ok((response, false));
        }
        // Replay the original acceptance before applying an override to current intent.
        if force {
            expected_generation = None;
            if let Mutation::Save {
                expected_application_id,
                ..
            } = &mut mutation
            {
                *expected_application_id = None;
            }
        }
        let (current, latest) = Self::mutation_snapshot(&mut tx, &mutation).await?;
        if let Mutation::Save { application, .. } = &mutation
            && let Some(current) = &current
            && current.application.spec().manifest.is_some()
            && current.application.spec_hash() != application.spec_hash()
        {
            return Err(StoreError::RepositoryManaged);
        }
        let (response, wake) =
            Self::execute_mutation(&mut tx, mutation, current, latest, expected_generation, now)
                .await?;
        if let Some(request_id) = request_id {
            let response_json = serde_json::to_string(&response).map_err(StoreError::corrupt)?;
            let expires = now.saturating_add(86_400_000);
            sqlx::query!("INSERT INTO request_receipts(request_id,fingerprint,response_json,expires_at_ms) VALUES(?1,?2,?3,?4) ON CONFLICT(request_id) DO UPDATE SET fingerprint=excluded.fingerprint,response_json=excluded.response_json,expires_at_ms=excluded.expires_at_ms",request_id,fingerprint,response_json,expires)
                .execute(&mut *tx).await.map_err(StoreError::database)?;
        }
        tx.commit().await.map_err(StoreError::database)?;
        Ok((response, wake))
    }

    fn mutation_fingerprint(
        mutation: &Mutation,
        expected_generation: Option<u64>,
        force: bool,
    ) -> Result<String, StoreError> {
        Ok(format!(
            "{:x}",
            Sha256::digest(
                serde_json::to_vec(&(mutation, expected_generation, force))
                    .map_err(StoreError::corrupt)?
            )
        ))
    }

    async fn replay_on(
        connection: &mut SqliteConnection,
        request_id: Option<&str>,
        fingerprint: &str,
        now: i64,
    ) -> Result<Option<MutationResponse>, StoreError> {
        let Some(request_id) = request_id else {
            return Ok(None);
        };
        let receipt = sqlx::query!("SELECT fingerprint,response_json FROM request_receipts WHERE request_id=?1 AND expires_at_ms>?2",request_id,now)
            .fetch_optional(connection).await.map_err(StoreError::database)?;
        receipt
            .map(|receipt| {
                if receipt.fingerprint != fingerprint {
                    return Err(StoreError::ReplayConflict);
                }
                serde_json::from_str(&receipt.response_json).map_err(StoreError::corrupt)
            })
            .transpose()
    }

    async fn execute_mutation(
        tx: &mut Transaction<'_, Sqlite>,
        mutation: Mutation,
        current: Option<StoredApplication>,
        latest: Option<Operation>,
        expected_generation: Option<u64>,
        now: i64,
    ) -> Result<(MutationResponse, bool), StoreError> {
        Self::check_mutation_generation(
            &mutation,
            current.as_ref(),
            latest.as_ref(),
            expected_generation,
        )?;
        Ok(match mutation {
            Mutation::Save {
                application,
                expected_application_id,
                deploy,
            } => {
                let application = application.with_id(Self::application_identity(
                    current.as_ref(),
                    expected_application_id.as_deref(),
                )?);
                let mut saved =
                    Self::save_configuration_on(tx, &application, expected_generation).await?;
                Self::deploy_saved(tx, &application, &mut saved, deploy).await?;
                (MutationResponse::Saved(saved), deploy)
            }
            Mutation::Edit { id, edit, deploy } => {
                Self::accept_edit(
                    tx,
                    current.ok_or(StoreError::NotFound)?,
                    latest,
                    id,
                    edit,
                    deploy,
                    now,
                )
                .await?
            }
            Mutation::Deploy { id } => {
                let app = current.ok_or(StoreError::NotFound)?;
                let op = Self::request_deploy_on(tx, &id, expected_generation).await?;
                Self::insert_deployment_on(tx, &op, &app.application).await?;
                (
                    MutationResponse::Operation(AcceptedOperation::from(&op)),
                    true,
                )
            }
            Mutation::Delete { id } => {
                let operation =
                    if let Some(op) = latest.filter(|op| op.kind == OperationKind::Delete) {
                        op
                    } else {
                        Self::request_delete_on(tx, &id, expected_generation).await?
                    };
                (
                    MutationResponse::Operation(AcceptedOperation::from(&operation)),
                    true,
                )
            }
            Mutation::Reconcile { .. } => {
                current.ok_or(StoreError::NotFound)?;
                let operation = latest.ok_or(StoreError::NotFound)?;
                let operation = if operation.state.terminal() || operation.error_code.is_some() {
                    Self::retry_operation_on(tx, &operation).await?
                } else {
                    operation
                };
                (
                    MutationResponse::Operation(AcceptedOperation::from(&operation)),
                    true,
                )
            }
            Mutation::Rename { id, name } => {
                Self::rename_on(
                    tx,
                    current.ok_or(StoreError::NotFound)?,
                    latest,
                    id,
                    name,
                    now,
                )
                .await?
            }
        })
    }

    fn check_mutation_generation(
        mutation: &Mutation,
        current: Option<&StoredApplication>,
        latest: Option<&Operation>,
        expected_generation: Option<u64>,
    ) -> Result<(), StoreError> {
        let actual = current.map_or_else(
            || {
                latest
                    .filter(|op| {
                        matches!(mutation, Mutation::Delete { .. })
                            && op.kind == OperationKind::Delete
                    })
                    .map_or(0, |op| op.generation)
            },
            |app| app.generation,
        );
        Self::check_generation(expected_generation, actual)
    }

    async fn accept_edit(
        tx: &mut Transaction<'_, Sqlite>,
        current: StoredApplication,
        latest: Option<Operation>,
        id: ApplicationId,
        edit: piqueld_core::edit::ApplicationEdit,
        deploy: bool,
        now: i64,
    ) -> Result<(MutationResponse, bool), StoreError> {
        use piqueld_core::edit::ApplicationEdit;
        if current.delete_intent {
            return Err(StoreError::Busy);
        }
        if current.application.spec().manifest.is_some() && !edit.is_repository_setting() {
            return Err(StoreError::RepositoryManaged);
        }
        let mut manifest = current.application.to_manifest();
        edit.clone().apply(&mut manifest)?;
        let application = manifest.validate()?.normalize(id.clone());
        let mut saved = if let ApplicationEdit::Name(name) = edit {
            let (MutationResponse::Rename(renamed), _) =
                Self::rename_on(tx, current, latest, id, name, now).await?
            else {
                unreachable!("rename returns its receipt")
            };
            piqueld_core::api::SavedApplication {
                application_id: renamed.application_id,
                generation: renamed.generation,
                operation_id: None,
            }
        } else {
            Self::save_configuration_on(tx, &application, Some(current.generation)).await?
        };
        Self::deploy_saved(tx, &application, &mut saved, deploy).await?;
        Ok((MutationResponse::Saved(saved), deploy))
    }

    async fn deploy_saved(
        tx: &mut Transaction<'_, Sqlite>,
        application: &piqueld_core::NormalizedApplication,
        saved: &mut piqueld_core::api::SavedApplication,
        deploy: bool,
    ) -> Result<(), StoreError> {
        if deploy {
            let operation =
                Self::request_deploy_on(tx, application.id(), Some(saved.generation)).await?;
            Self::insert_deployment_on(tx, &operation, application).await?;
            saved.operation_id = Some(operation.id);
        }
        Ok(())
    }

    fn application_identity(
        current: Option<&StoredApplication>,
        expected: Option<&str>,
    ) -> Result<ApplicationId, StoreError> {
        if expected.is_some_and(|id| current.is_none_or(|app| app.application.id().as_str() != id))
        {
            return Err(StoreError::IdentityConflict);
        }
        Ok(current.map_or_else(
            || ApplicationId::parse(super::new_id("app")).expect("valid generated ID"),
            |app| app.application.id().clone(),
        ))
    }

    async fn mutation_snapshot(
        tx: &mut Transaction<'_, Sqlite>,
        mutation: &Mutation,
    ) -> Result<(Option<StoredApplication>, Option<Operation>), StoreError> {
        let (id, name) = match mutation {
            Mutation::Save { application, .. } => {
                (None, Some(application.metadata().name.as_str()))
            }
            Mutation::Edit { id, .. }
            | Mutation::Deploy { id }
            | Mutation::Delete { id }
            | Mutation::Reconcile { id }
            | Mutation::Rename { id, .. } => (Some(id.as_str()), None),
        };
        let current = sqlx::query_as!(ApplicationRow,r#"SELECT id AS "id!",desired_json,resolved_json,generation,resolved_generation,delete_intent,created_at_ms,updated_at_ms FROM applications WHERE deleted_at_ms IS NULL AND ((?1 IS NOT NULL AND id=?1) OR (?1 IS NULL AND name=?2))"#,id,name)
            .fetch_optional(&mut **tx).await.map_err(StoreError::database)?.map(ApplicationRow::decode).transpose()?;
        let application_id = current
            .as_ref()
            .map(|app| app.application.id().as_str())
            .or(id);
        let latest_id = sqlx::query_scalar!(r#"SELECT id AS "id!" FROM operations WHERE application_id=?1 ORDER BY created_at_ms DESC,id DESC LIMIT 1"#,application_id)
            .fetch_optional(&mut **tx).await.map_err(StoreError::database)?;
        let latest = match latest_id {
            Some(id) => Some(Self::operation_on(tx, &id).await?),
            None => None,
        };
        Ok((current, latest))
    }

    async fn rename_on(
        tx: &mut Transaction<'_, Sqlite>,
        current: StoredApplication,
        latest: Option<Operation>,
        id: ApplicationId,
        name: String,
        now: i64,
    ) -> Result<(MutationResponse, bool), StoreError> {
        let mut app = current;
        if app.application.spec().manifest.is_some() {
            return Err(StoreError::RepositoryManaged);
        }
        if app.delete_intent || latest.as_ref().is_some_and(|op| !op.state.terminal()) {
            return Err(StoreError::Busy);
        }
        let generation = if app.application.metadata().name.as_str() == name {
            app.generation
        } else {
            app.generation
                .checked_add(1)
                .ok_or(StoreError::InvalidInput)?
        };
        let revision = i64::try_from(generation).map_err(StoreError::invalid_input)?;
        let old_name = app.application.metadata().name.clone();
        app.application = app.application.with_name(
            piqueld_core::ApplicationName::parse(name.clone())
                .map_err(StoreError::invalid_input)?,
        );
        if let Some(target) = app.resolved.as_mut() {
            target.name.clone_from(&app.application.metadata().name);
        }
        let desired = serde_json::to_string(&app.application).map_err(StoreError::corrupt)?;
        let resolved = app
            .resolved
            .as_ref()
            .map(serde_json::to_string)
            .transpose()
            .map_err(StoreError::corrupt)?;
        let app_id = id.as_str();
        sqlx::query!("UPDATE applications SET name=?1,desired_json=?2,resolved_json=?3,generation=?4,resolved_generation=CASE WHEN resolved_generation=generation THEN ?4 ELSE resolved_generation END,updated_at_ms=?5 WHERE id=?6",name,desired,resolved,revision,now,app_id)
                    .execute(&mut **tx).await.map_err(|error| if error.as_database_error().is_some_and(sqlx::error::DatabaseError::is_unique_violation) {StoreError::AlreadyExists} else {StoreError::database(error)})?;
        // The latest target may be retried after a rename. Only its display metadata changes.
        if let Some(op) = latest {
            sqlx::query!("UPDATE operations SET target_json=CASE WHEN target_json IS NULL THEN NULL ELSE json_set(target_json,'$.name',?1) END WHERE id=?2",name,op.id).execute(&mut **tx).await.map_err(StoreError::database)?;
        }
        if old_name.as_str() != name {
            let message = format!("renamed {old_name} to {name}");
            sqlx::query!("INSERT INTO events(application_id,generation,kind,message,created_at_ms) VALUES(?1,?2,'application_renamed',?3,?4)",app_id,revision,message,now).execute(&mut **tx).await.map_err(StoreError::database)?;
        }
        Ok((
            MutationResponse::Rename(RenamedApplication {
                application_id: id.to_string(),
                name,
                generation,
            }),
            false,
        ))
    }

    pub(crate) async fn prune_receipts(&self) -> Result<(), StoreError> {
        let _writer = self.writers.lock().await;
        let now = now_ms();
        sqlx::query!("DELETE FROM request_receipts WHERE expires_at_ms<=?1", now)
            .execute(&self.pool)
            .await
            .map_err(StoreError::database)?;
        Ok(())
    }
}
