//! Atomic acceptance: compare current intent, write the change and its replay receipt together.
use super::{Operation, OperationKind, Store, StoreError, StoredApplication, now_ms};
use crate::api::{Mutation, MutationResponse};
use piqueld_core::api::{AcceptedOperation, DeletedApplication, RenamedApplication};
use piqueld_core::{ApplicationId, EnvironmentId, EnvironmentName};
use sha2::{Digest, Sha256};
use sqlx::{Sqlite, SqliteConnection, Transaction};

/// A mutation's outcome inside its transaction.
struct Accepted {
    /// Response returned to the caller and stored for replay.
    response: MutationResponse,
    /// Whether the controller has new work.
    wake: bool,
    /// Environments whose hostname reservations may have changed.
    environments: Vec<EnvironmentId>,
}

impl Store {
    /// Commits a mutation and its optional replay receipt in one transaction.
    /// `expected_generation` compares the inspected application revision; zero
    /// means the application must be absent, and `None` omits the revision check.
    /// `request_id` is a validated caller-supplied idempotency key, distinct from
    /// the server-generated operation ID (renames do not create an operation).
    ///
    /// 1. Returns the stored response when `request_id` replays an identical request.
    /// 2. With `force`, drops the generation and identity preconditions.
    /// 3. Executes the mutation against the current application or environment,
    ///    refusing manifest changes to repository-managed applications.
    /// 4. Stores the replay receipt for 24 hours and commits through hostname
    ///    reservation checks of every affected environment.
    ///
    /// The returned flag asks the controller to wake up; replays never wake it.
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
        let accepted = Self::execute_mutation(&mut tx, mutation, expected_generation, now).await?;
        if let Some(request_id) = request_id {
            let response_json =
                serde_json::to_string(&accepted.response).map_err(StoreError::corrupt)?;
            let expires = now.saturating_add(86_400_000);
            sqlx::query!("INSERT INTO request_receipts(request_id,fingerprint,response_json,expires_at_ms) VALUES(?1,?2,?3,?4) ON CONFLICT(request_id) DO UPDATE SET fingerprint=excluded.fingerprint,response_json=excluded.response_json,expires_at_ms=excluded.expires_at_ms",request_id,fingerprint,response_json,expires)
                .execute(&mut *tx).await.map_err(StoreError::database)?;
        }
        Self::commit_environment_changes(
            tx,
            accepted.environments.iter().map(EnvironmentId::as_str),
        )
        .await?;
        Ok((accepted.response, accepted.wake))
    }

    /// Hashes the full request (mutation, precondition, and `force`) so a reused
    /// request ID can be told apart from an exact retry.
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

    /// Returns the stored response for an unexpired receipt with a matching
    /// fingerprint, `ReplayConflict` for a mismatched one, or `None` without a receipt.
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

    /// Loads the mutation's application or environment, checks the revision
    /// precondition against its application (an absent one counts as revision
    /// zero), then dispatches to the transactional handler.
    async fn execute_mutation(
        tx: &mut Transaction<'_, Sqlite>,
        mutation: Mutation,
        expected: Option<u64>,
        now: i64,
    ) -> Result<Accepted, StoreError> {
        match mutation {
            Mutation::Save {
                application,
                expected_application_id,
                deploy,
            } => {
                let current =
                    Self::find_by_name_on(tx, application.metadata().name.as_str()).await?;
                if let Some(current) = &current
                    && current.application.spec().manifest.is_some()
                    && current.application.spec_hash() != application.spec_hash()
                {
                    return Err(StoreError::RepositoryManaged);
                }
                Self::check_generation(expected, current.as_ref().map_or(0, |app| app.generation))?;
                let application = application.with_id(Self::application_identity(
                    current.as_ref(),
                    expected_application_id.as_deref(),
                )?);
                let mut saved = Self::save_configuration_on(tx, &application, expected).await?;
                Self::deploy_saved(tx, application.id(), &application, &mut saved, deploy).await?;
                Ok(Accepted {
                    environments: Self::environment_ids_on(tx, application.id()).await?,
                    response: MutationResponse::Saved(saved),
                    wake: deploy,
                })
            }
            Mutation::Edit { id, edit, deploy } => {
                let current = Self::checked_application_on(tx, &id, expected).await?;
                Self::accept_edit(tx, current, *edit, deploy, now).await
            }
            Mutation::Rename { id, name } => {
                let current = Self::checked_application_on(tx, &id, expected).await?;
                let renamed = Self::rename_on(tx, current, name, now).await?;
                Ok(Accepted {
                    environments: Self::environment_ids_on(tx, &id).await?,
                    response: MutationResponse::Rename(renamed),
                    wake: false,
                })
            }
            Mutation::DeleteApplication { id, environments } => {
                let current = Self::checked_application_on(tx, &id, expected).await?;
                Self::delete_application_on(tx, current, environments, now).await
            }
            Mutation::CreateEnvironment { application, name } => {
                let current = Self::checked_application_on(tx, &application, expected).await?;
                let id = Self::create_environment_on(tx, &current, &name, now).await?;
                Self::environment_accepted(tx, id).await
            }
            Mutation::RenameEnvironment { id, name } => {
                let current = Self::checked_environment_on(tx, &id, expected).await?;
                Self::rename_environment_on(tx, &current, &name, now).await?;
                Self::environment_accepted(tx, id).await
            }
            Mutation::Deploy { id, revision } => {
                let current = Self::checked_environment_on(tx, &id, expected).await?;
                let mut application = current.application.application.clone();
                // Only the captured snapshot changes; the fetched manifest replaces it.
                if let Some(revision) = &revision {
                    application = application.with_manifest_revision(revision)?;
                }
                let operation = Self::request_deploy_on(tx, &current).await?;
                Self::insert_deployment_on(tx, &operation, &application).await?;
                Ok(Self::operation_accepted(&operation))
            }
            Mutation::Delete { id } => {
                let current = Self::checked_environment_on(tx, &id, expected).await?;
                let operation = match Self::latest_operation_on(tx, &id).await? {
                    Some(operation)
                        if current.delete_intent() && operation.kind == OperationKind::Delete =>
                    {
                        operation
                    }
                    _ => Self::request_delete_on(tx, &id).await?,
                };
                Ok(Self::operation_accepted(&operation))
            }
            Mutation::Reconcile { id } => {
                Self::checked_environment_on(tx, &id, expected).await?;
                let operation = Self::latest_operation_on(tx, &id)
                    .await?
                    .ok_or(StoreError::NotFound)?;
                let operation = if operation.state.terminal() || operation.error_code.is_some() {
                    Self::retry_operation_on(tx, &operation).await?
                } else {
                    operation
                };
                Ok(Self::operation_accepted(&operation))
            }
        }
    }

    /// Loads an application after checking the revision precondition, so a
    /// stale revision is reported as a conflict before absence.
    async fn checked_application_on(
        tx: &mut Transaction<'_, Sqlite>,
        id: &ApplicationId,
        expected: Option<u64>,
    ) -> Result<StoredApplication, StoreError> {
        let current = Self::application_on(tx, id.as_str()).await?;
        Self::check_generation(expected, current.as_ref().map_or(0, |app| app.generation))?;
        current.ok_or(StoreError::NotFound)
    }

    /// Loads an environment after checking the revision precondition against
    /// its application's revision.
    async fn checked_environment_on(
        tx: &mut Transaction<'_, Sqlite>,
        id: &EnvironmentId,
        expected: Option<u64>,
    ) -> Result<super::StoredEnvironment, StoreError> {
        let current = Self::environment_on(tx, id.as_str()).await?;
        Self::check_generation(
            expected,
            current
                .as_ref()
                .map_or(0, |environment| environment.application.generation),
        )?;
        current.ok_or(StoreError::NotFound)
    }

    /// Responds with an accepted environment operation.
    fn operation_accepted(operation: &Operation) -> Accepted {
        Accepted {
            response: MutationResponse::Operation(AcceptedOperation::from(operation)),
            wake: true,
            environments: vec![operation.environment_id.clone()],
        }
    }

    /// Responds with the current state of a created or renamed environment.
    async fn environment_accepted(
        tx: &mut Transaction<'_, Sqlite>,
        id: EnvironmentId,
    ) -> Result<Accepted, StoreError> {
        let environment = Self::environment_on(tx, id.as_str())
            .await?
            .ok_or(StoreError::NotFound)?;
        Ok(Accepted {
            response: MutationResponse::Environment(environment.environment),
            wake: false,
            environments: vec![id],
        })
    }

    /// The IDs of an application's environments.
    async fn environment_ids_on(
        tx: &mut Transaction<'_, Sqlite>,
        application: &ApplicationId,
    ) -> Result<Vec<EnvironmentId>, StoreError> {
        Ok(Self::environments_on(tx, application.as_str())
            .await?
            .into_iter()
            .map(|environment| environment.id)
            .collect())
    }

    /// Fetches an environment's newest operation inside `tx`.
    async fn latest_operation_on(
        tx: &mut Transaction<'_, Sqlite>,
        id: &EnvironmentId,
    ) -> Result<Option<Operation>, StoreError> {
        let id = id.as_str();
        let latest = sqlx::query_scalar!(r#"SELECT id AS "id!" FROM operations WHERE environment_id=?1 ORDER BY created_at_ms DESC,id DESC LIMIT 1"#,id)
            .fetch_optional(&mut **tx).await.map_err(StoreError::database)?;
        match latest {
            Some(id) => Ok(Some(Self::operation_on(tx, &id).await?)),
            None => Ok(None),
        }
    }

    /// Applies a single field edit to the saved manifest, revalidates it, and
    /// saves it (renames go through `rename_on`), optionally starting a deployment.
    /// Repository-managed applications only accept repository settings; applications
    /// being deleted are `Busy`. Records an `application_edited` event naming the
    /// edited field and resource in every environment's history.
    async fn accept_edit(
        tx: &mut Transaction<'_, Sqlite>,
        current: StoredApplication,
        edit: piqueld_core::edit::ApplicationEdit,
        deploy: bool,
        now: i64,
    ) -> Result<Accepted, StoreError> {
        use piqueld_core::edit::ApplicationEdit;
        if current.delete_intent {
            return Err(StoreError::Busy);
        }
        if current.application.spec().manifest.is_some() && !edit.is_repository_setting() {
            return Err(StoreError::RepositoryManaged);
        }
        let id = current.application.id().clone();
        let mut manifest = current.application.to_manifest();
        edit.clone().apply(&mut manifest)?;
        let application = manifest.validate()?.normalize(id.clone());
        let field = match &edit {
            ApplicationEdit::Name(_) => "name",
            ApplicationEdit::Repository(_)
            | ApplicationEdit::RepositoryUrl(_)
            | ApplicationEdit::RepositoryBranch(_)
            | ApplicationEdit::RepositoryCommit(_)
            | ApplicationEdit::RepositoryPath(_) => "repository",
            ApplicationEdit::AddService(_) | ApplicationEdit::RemoveService(_) => "services",
            ApplicationEdit::Service { .. } => "service",
            ApplicationEdit::AddVolume(_)
            | ApplicationEdit::Volumes(_)
            | ApplicationEdit::RemoveVolume(_) => "volumes",
            ApplicationEdit::Routes(_) => "routes",
            ApplicationEdit::Jobs(_) => "jobs",
        };
        let resource = match &edit {
            ApplicationEdit::Service { name, .. } | ApplicationEdit::RemoveService(name) => {
                Some(name.as_str())
            }
            ApplicationEdit::AddService(service) => Some(service.name.as_str()),
            _ => None,
        };
        let mut saved = if let ApplicationEdit::Name(name) = edit {
            let renamed = Self::rename_on(tx, current, name, now).await?;
            piqueld_core::api::SavedApplication {
                application_id: renamed.application_id,
                generation: renamed.generation,
                operation_id: None,
            }
        } else {
            let saved =
                Self::save_configuration_on(tx, &application, Some(current.generation)).await?;
            let generation = i64::try_from(saved.generation).map_err(StoreError::corrupt)?;
            let app_id = id.as_str();
            sqlx::query!("INSERT INTO events(environment_id,generation,kind,message,phase,resource,created_at_ms) SELECT id,?2,'application_edited','Saved application configuration',?3,?4,?5 FROM environments WHERE application_id=?1",app_id,generation,field,resource,now).execute(&mut **tx).await.map_err(StoreError::database)?;
            saved
        };
        Self::deploy_saved(tx, &id, &application, &mut saved, deploy).await?;
        Ok(Accepted {
            environments: Self::environment_ids_on(tx, &id).await?,
            response: MutationResponse::Saved(saved),
            wake: deploy,
        })
    }

    /// When `deploy` is set, starts a deployment of the just-saved configuration
    /// to the application's only environment and records its operation ID on `saved`.
    async fn deploy_saved(
        tx: &mut Transaction<'_, Sqlite>,
        id: &ApplicationId,
        application: &piqueld_core::NormalizedApplication,
        saved: &mut piqueld_core::api::SavedApplication,
        deploy: bool,
    ) -> Result<(), StoreError> {
        if deploy {
            let environment = Self::sole_environment_on(tx, id).await?;
            let environment = Self::environment_on(tx, environment.as_str())
                .await?
                .ok_or(StoreError::NotFound)?;
            let operation = Self::request_deploy_on(tx, &environment).await?;
            Self::insert_deployment_on(tx, &operation, application).await?;
            saved.operation_id = Some(operation.id);
        }
        Ok(())
    }

    /// Resolves the ID for a save by name: the existing application's ID,
    /// or a freshly generated one. Fails with `IdentityConflict` when the caller
    /// expected an ID that the name no longer selects.
    fn application_identity(
        current: Option<&StoredApplication>,
        expected: Option<&str>,
    ) -> Result<ApplicationId, StoreError> {
        if expected.is_some_and(|id| current.is_none_or(|app| app.application.id().as_str() != id))
        {
            return Err(StoreError::IdentityConflict);
        }
        current.map_or_else(
            || ApplicationId::parse(super::new_id("app")).map_err(StoreError::corrupt),
            |app| Ok(app.application.id().clone()),
        )
    }

    /// Renames an idle application that is not repository-managed. A real name
    /// change bumps the generation (and each environment's resolved generation
    /// when it was current), updates the display name of every environment's
    /// resolved and latest targets, and records an `application_renamed` event
    /// in each environment's history. Never wakes the controller.
    async fn rename_on(
        tx: &mut Transaction<'_, Sqlite>,
        current: StoredApplication,
        name: String,
        now: i64,
    ) -> Result<RenamedApplication, StoreError> {
        if current.application.spec().manifest.is_some() {
            return Err(StoreError::RepositoryManaged);
        }
        let id = current.application.id().as_str().to_owned();
        let busy = sqlx::query_scalar!("SELECT EXISTS(SELECT 1 FROM environments e WHERE e.application_id=?1 AND (e.delete_intent=1 OR (SELECT state FROM operations o WHERE o.environment_id=e.id ORDER BY o.created_at_ms DESC,o.id DESC LIMIT 1) IN ('requested','running')))",id)
            .fetch_one(&mut **tx).await.map_err(StoreError::database)?;
        if current.delete_intent || busy != 0 {
            return Err(StoreError::Busy);
        }
        let old_name = current.application.metadata().name.clone();
        let generation = if old_name.as_str() == name {
            current.generation
        } else {
            current
                .generation
                .checked_add(1)
                .ok_or(StoreError::InvalidInput)?
        };
        let previous = i64::try_from(current.generation).map_err(StoreError::invalid_input)?;
        let revision = i64::try_from(generation).map_err(StoreError::invalid_input)?;
        let application = current.application.with_name(
            piqueld_core::ApplicationName::parse(name.clone())
                .map_err(StoreError::invalid_input)?,
        );
        let desired = serde_json::to_string(&application).map_err(StoreError::corrupt)?;
        sqlx::query!("UPDATE applications SET name=?1,desired_json=?2,generation=?3,updated_at_ms=?4 WHERE id=?5",name,desired,revision,now,id)
            .execute(&mut **tx).await.map_err(StoreError::constraint)?;
        // Resolved and latest targets may be retried after a rename. Only their display metadata changes.
        sqlx::query!("UPDATE environments SET resolved_json=CASE WHEN resolved_json IS NULL THEN NULL ELSE json_set(resolved_json,'$.name',?1) END,resolved_generation=CASE WHEN resolved_generation=?2 THEN ?3 ELSE resolved_generation END WHERE application_id=?4",name,previous,revision,id)
            .execute(&mut **tx).await.map_err(StoreError::database)?;
        sqlx::query!("UPDATE operations SET target_json=json_set(target_json,'$.name',?1) WHERE target_json IS NOT NULL AND id IN (SELECT (SELECT o.id FROM operations o WHERE o.environment_id=e.id ORDER BY o.created_at_ms DESC,o.id DESC LIMIT 1) FROM environments e WHERE e.application_id=?2)",name,id)
            .execute(&mut **tx).await.map_err(StoreError::database)?;
        if old_name.as_str() != name {
            let message = format!("renamed {old_name} to {name}");
            sqlx::query!("INSERT INTO events(environment_id,generation,kind,message,created_at_ms) SELECT id,?2,'application_renamed',?3,?4 FROM environments WHERE application_id=?1",id,revision,message,now).execute(&mut **tx).await.map_err(StoreError::database)?;
        }
        Ok(RenamedApplication {
            application_id: id,
            name,
            generation,
        })
    }

    /// Requests deletion of an application and every environment. With several
    /// environments, `confirmed` must name each of them. Environments already
    /// being deleted keep their pending operation. An application without
    /// environments is removed immediately.
    async fn delete_application_on(
        tx: &mut Transaction<'_, Sqlite>,
        current: StoredApplication,
        mut confirmed: Vec<EnvironmentName>,
        now: i64,
    ) -> Result<Accepted, StoreError> {
        let id = current.application.id().as_str().to_owned();
        let environments = Self::environments_on(tx, &id).await?;
        let names = environments
            .iter()
            .map(|environment| environment.name.clone())
            .collect::<Vec<_>>();
        confirmed.sort();
        confirmed.dedup();
        if names.len() > 1 && confirmed != names {
            return Err(StoreError::ConfirmationRequired {
                environments: names,
            });
        }
        let mut generation = current.generation;
        if !current.delete_intent {
            generation = generation.checked_add(1).ok_or(StoreError::InvalidInput)?;
            let revision = i64::try_from(generation).map_err(StoreError::invalid_input)?;
            sqlx::query!(
                "UPDATE applications SET delete_intent=1,generation=?1,updated_at_ms=?2 WHERE id=?3",
                revision,
                now,
                id
            )
            .execute(&mut **tx)
            .await
            .map_err(StoreError::database)?;
        }
        let mut operations = Vec::with_capacity(environments.len());
        for environment in &environments {
            let pending = Self::latest_operation_on(tx, &environment.id)
                .await?
                .filter(|operation| {
                    environment.delete_intent && operation.kind == OperationKind::Delete
                });
            let operation = match pending {
                Some(operation) => operation,
                None => Self::request_delete_on(tx, &environment.id).await?,
            };
            operations.push(AcceptedOperation::from(&operation));
        }
        Self::finish_application_delete_on(tx, &id).await?;
        Ok(Accepted {
            wake: !operations.is_empty(),
            response: MutationResponse::Deleted(DeletedApplication {
                application_id: id,
                generation,
                operations,
            }),
            environments: environments
                .into_iter()
                .map(|environment| environment.id)
                .collect(),
        })
    }

    /// Deletes expired idempotency receipts.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Receipts recorded before deploy overrides existed must still replay.
    #[test]
    fn deploys_without_a_revision_keep_their_original_fingerprint() {
        let deploy = Mutation::deploy(EnvironmentId::parse("input-app").unwrap());
        let original = Sha256::digest(br#"[{"kind":"deploy","id":"input-app"},null,false]"#);
        assert_eq!(
            Store::mutation_fingerprint(&deploy, None, false).unwrap(),
            format!("{original:x}")
        );
    }
}
