//! Atomic acceptance: compare current intent, write the change and its replay receipt together.
use super::access::Holder;
use super::{Actor, Operation, OperationKind, Store, StoreError, StoredApplication, now_ms};
use crate::api::{Mutation, MutationResponse, PreviewMutation};
use piqueld_core::access::{Grants, Target};
use piqueld_core::api::{
    AcceptedOperation, CreatedPreview, DeletedApplication, RenamedApplication,
};
use piqueld_core::{ApplicationId, EnvironmentId, EnvironmentName};
use sha2::{Digest, Sha256};
use sqlx::{Sqlite, SqliteConnection, Transaction};

/// Application deletion as serialized before environments existed, in its
/// original field order, so its request fingerprint still matches.
#[derive(serde::Serialize)]
struct LegacyDelete<'a> {
    kind: &'static str,
    id: &'a ApplicationId,
}

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
    /// 1. Checks that `actor` may submit the mutation against the current
    ///    application, before any replay. Replaying a save may also pass the
    ///    check a creation passes, since it may have created that application.
    /// 2. Returns the stored response when `request_id` replays an identical
    ///    request from the same account.
    /// 3. With `force`, drops the generation and identity preconditions.
    /// 4. Executes the mutation against the current application or environment,
    ///    refusing manifest changes to repository-managed applications, and
    ///    gives an account that created an application through an unscoped
    ///    credential its matching grants there.
    /// 5. Attributes the operations and events it wrote to the caller.
    /// 6. Stores the replay receipt for 24 hours and commits through hostname
    ///    reservation checks of every affected environment.
    ///
    /// The returned flag asks the controller to wake up; replays never wake it.
    pub(crate) async fn accept(
        &self,
        actor: Actor<'_>,
        mut mutation: Mutation,
        mut expected_generation: Option<u64>,
        force: bool,
        request_id: Option<&str>,
    ) -> Result<(MutationResponse, bool), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        let now = now_ms();
        let fingerprint = Self::mutation_fingerprint(&mutation, expected_generation, force)?;
        let legacy = Self::legacy_fingerprint(&mutation, expected_generation, force)?;
        let caller = actor.load(&mut tx).await?;
        let first_event =
            sqlx::query_scalar!(r#"SELECT COALESCE(MAX(id),0)+1 AS "id!: i64" FROM events"#)
                .fetch_one(&mut *tx)
                .await
                .map_err(StoreError::database)?;
        let owner = caller.as_ref().map(|authority| authority.user_id.as_str());
        let replay = Self::replay_on(
            &mut tx,
            request_id,
            owner,
            [Some(&fingerprint), legacy.as_ref()],
            now,
        )
        .await;
        let replaying = matches!(replay, Ok(Some(_)));
        let creating = match &caller {
            Some(authority) => {
                Self::authorize_mutation_on(&mut tx, &authority.grants, &mutation, replaying)
                    .await?
            }
            None => false,
        };
        if let Some(response) = replay? {
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
        // Scoped credentials hand out no lasting access, so an application
        // they create gives their account no creator grants.
        if creating
            && let Some(authority) = caller.as_ref().filter(|authority| !authority.scoped)
            && let MutationResponse::Saved(saved) = &accepted.response
        {
            let id =
                ApplicationId::parse(saved.application_id.as_str()).map_err(StoreError::corrupt)?;
            let creator = authority.grants.for_created_application(&id);
            if !creator.is_empty() {
                Holder::User(&authority.user_id)
                    .extend(&mut tx, &creator)
                    .await?;
            }
        }
        if !matches!(actor, Actor::Daemon) {
            Self::attribute_on(&mut tx, actor.attribution(), first_event).await?;
        }
        if let Some(request_id) = request_id {
            let response_json =
                serde_json::to_string(&accepted.response).map_err(StoreError::corrupt)?;
            let expires = now.saturating_add(86_400_000);
            sqlx::query!("INSERT INTO request_receipts(request_id,fingerprint,response_json,expires_at_ms,user_id) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(request_id) DO UPDATE SET fingerprint=excluded.fingerprint,response_json=excluded.response_json,expires_at_ms=excluded.expires_at_ms,user_id=excluded.user_id",request_id,fingerprint,response_json,expires,owner)
                .execute(&mut *tx).await.map_err(StoreError::database)?;
        }
        Self::commit_environment_changes(
            tx,
            accepted.environments.iter().map(EnvironmentId::as_str),
        )
        .await?;
        Ok((accepted.response, accepted.wake))
    }

    /// Records who requested a mutation (an account and its credential, or
    /// the host operator) on the events it wrote, those since `first_event`
    /// in this transaction, and on the operations it created or restarted,
    /// whose later events inherit it. Those are left `requested` with new
    /// events; operations it superseded or returned unchanged, e.g. by
    /// repeating a request, keep their actor.
    async fn attribute_on(
        tx: &mut Transaction<'_, Sqlite>,
        by: super::Attribution<'_>,
        first_event: i64,
    ) -> Result<(), StoreError> {
        let operator = by.operator_uid();
        sqlx::query!(
            "UPDATE operations SET actor_user_id=?1,actor_credential_id=?2,actor_operator_uid=?3
             WHERE state='requested'
             AND id IN (SELECT operation_id FROM events WHERE id>=?4 AND operation_id IS NOT NULL)",
            by.user_id,
            by.credential_id,
            operator,
            first_event
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        sqlx::query!(
            "UPDATE events SET actor_user_id=?1,actor_credential_id=?2,actor_operator_uid=?3 WHERE id>=?4",
            by.user_id,
            by.credential_id,
            operator,
            first_event
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        Ok(())
    }

    /// Checks that a caller holding `grants` may submit `mutation` against the
    /// current application (see `Grants::require_change`), and returns whether
    /// it creates one. Changes to an environment are checked on its application;
    /// an unknown environment is hidden unless the caller could act on any.
    /// When `replaying`, a save naming an existing application also passes with
    /// the permissions that would create it, as that may be what it did.
    async fn authorize_mutation_on(
        tx: &mut Transaction<'_, Sqlite>,
        grants: &Grants,
        mutation: &Mutation,
        replaying: bool,
    ) -> Result<bool, StoreError> {
        let application = match mutation {
            Mutation::Save { application, .. } => {
                Self::application_id_by_name_on(tx, application.metadata().name.as_str()).await?
            }
            Mutation::Edit { id, .. }
            | Mutation::Rename { id, .. }
            | Mutation::DeleteApplication { id, .. }
            | Mutation::CreateEnvironment {
                application: id, ..
            }
            | Mutation::Preview(PreviewMutation::Create {
                application: id, ..
            }) => Some(id.clone()),
            Mutation::RenameEnvironment { id, .. }
            | Mutation::SetBranch { id, .. }
            | Mutation::Deploy { id, .. }
            | Mutation::Delete { id }
            | Mutation::Reconcile { id }
            | Mutation::Preview(
                PreviewMutation::Deploy { id }
                | PreviewMutation::Delete { id }
                | PreviewMutation::Prune { id, .. },
            ) => Self::environment_application_on(tx, id).await?,
        };
        let target = match (&application, mutation) {
            (None, Mutation::Save { .. }) => Target::New,
            (Some(id), Mutation::Save { .. }) => Target::Named(id),
            (Some(id), _) => Target::Id(id),
            (None, _) => Target::Unknown,
        };
        let required = mutation.required();
        match grants.require_change(required, target) {
            Err(_) if replaying && matches!(target, Target::Named(_)) => {
                grants.require_change(required, Target::New)
            }
            checked => checked,
        }
        .map_err(StoreError::Denied)?;
        Ok(matches!(target, Target::New))
    }

    /// Hashes the full request (mutation, precondition, and `force`) so a reused
    /// request ID can be told apart from an exact retry.
    fn mutation_fingerprint(
        mutation: &impl serde::Serialize,
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

    /// Fingerprints the form a request had before environments existed, when
    /// it differs, so receipts accepted by an older daemon still replay.
    /// Deleting an application was then `{"kind":"delete","id":...}`.
    fn legacy_fingerprint(
        mutation: &Mutation,
        expected_generation: Option<u64>,
        force: bool,
    ) -> Result<Option<String>, StoreError> {
        let Mutation::DeleteApplication { id, environments } = mutation else {
            return Ok(None);
        };
        if !environments.is_empty() {
            return Ok(None);
        }
        let legacy = LegacyDelete { kind: "delete", id };
        Self::mutation_fingerprint(&legacy, expected_generation, force).map(Some)
    }

    /// Returns the stored response for an unexpired receipt of `owner` (the
    /// account, or `None` for the daemon and the host operator, which may act
    /// on everything) with one of the request's
    /// `fingerprints`, `ReplayConflict` for another owner's or a mismatched
    /// one, or `None` without a receipt.
    async fn replay_on(
        connection: &mut SqliteConnection,
        request_id: Option<&str>,
        owner: Option<&str>,
        fingerprints: [Option<&String>; 2],
        now: i64,
    ) -> Result<Option<MutationResponse>, StoreError> {
        let Some(request_id) = request_id else {
            return Ok(None);
        };
        let receipt = sqlx::query!("SELECT fingerprint,response_json,user_id FROM request_receipts WHERE request_id=?1 AND expires_at_ms>?2",request_id,now)
            .fetch_optional(connection).await.map_err(StoreError::database)?;
        receipt
            .map(|receipt| {
                if receipt.user_id.as_deref() != owner
                    || !fingerprints.contains(&Some(&receipt.fingerprint))
                {
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
                Self::deploy_saved(tx, application.id(), &mut saved, deploy).await?;
                Ok(Accepted {
                    environments: Self::environment_ids_on(tx, application.id()).await?,
                    response: MutationResponse::Saved(saved),
                    wake: deploy,
                })
            }
            Mutation::Edit { id, edit, deploy } => {
                let current = Self::checked_application_on(tx, &id, expected).await?;
                // Boxed: the largest arm would otherwise size every acceptance.
                Box::pin(Self::accept_edit(tx, current, *edit, deploy, now)).await
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
            Mutation::CreateEnvironment {
                application,
                name,
                branch,
            } => {
                let current = Self::checked_application_on(tx, &application, expected).await?;
                let id = Self::create_environment_on(tx, &current, &name, branch, now).await?;
                Self::environment_accepted(tx, id).await
            }
            Mutation::RenameEnvironment { id, name } => {
                let current = Self::checked_environment_on(tx, &id, expected).await?;
                Self::rename_environment_on(tx, &current, &name, now).await?;
                Self::environment_accepted(tx, id).await
            }
            Mutation::SetBranch { id, branch } => {
                let current = Self::checked_environment_on(tx, &id, expected).await?;
                Self::set_branch_on(tx, &current, branch, now).await?;
                Self::environment_accepted(tx, id).await
            }
            Mutation::Deploy { id, revision } => {
                let current = Self::checked_environment_on(tx, &id, expected).await?;
                // Only the captured snapshot changes; the fetched manifest replaces it.
                let application = current.candidate(revision.as_ref())?;
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
                    _ => Self::delete_environment_on(tx, &current).await?,
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
            // Boxed: preview changes would otherwise grow every acceptance future.
            Mutation::Preview(mutation) => {
                Box::pin(Self::execute_preview_on(tx, mutation, now)).await
            }
        }
    }

    /// Creates, deploys, or deletes a preview. Previews are found by ID only
    /// among previews, and never check or advance the application revision.
    async fn execute_preview_on(
        tx: &mut Transaction<'_, Sqlite>,
        mutation: PreviewMutation,
        now: i64,
    ) -> Result<Accepted, StoreError> {
        match mutation {
            PreviewMutation::Create {
                application,
                branch,
                slot,
            } => {
                let current = Self::checked_application_on(tx, &application, None).await?;
                let (id, created) =
                    Self::create_preview_on(tx, &current, &branch, slot.as_ref(), now).await?;
                let preview = Self::preview_on(tx, &id).await?;
                let operation = match Self::latest_operation_on(tx, &id).await? {
                    Some(operation) if !created => operation,
                    _ => Self::deploy_preview_on(tx, &preview).await?,
                };
                Ok(Accepted {
                    response: MutationResponse::Preview(Box::new(CreatedPreview {
                        preview: preview.environment,
                        operation: AcceptedOperation::from(&operation),
                        created,
                    })),
                    wake: created,
                    environments: vec![id],
                })
            }
            PreviewMutation::Deploy { id } => {
                let preview = Self::preview_on(tx, &id).await?;
                let operation = Self::deploy_preview_on(tx, &preview).await?;
                Ok(Self::operation_accepted(&operation))
            }
            PreviewMutation::Delete { id } => {
                let preview = Self::preview_on(tx, &id).await?;
                Self::delete_preview_on(tx, &preview).await
            }
            PreviewMutation::Prune { id, repository } => {
                let preview = Self::preview_on(tx, &id).await?;
                // A branch gone from one repository says nothing about another.
                if preview
                    .repository()
                    .is_none_or(|current| current.repository.url != repository)
                {
                    return Err(StoreError::IdentityConflict);
                }
                Self::delete_preview_on(tx, &preview).await
            }
        }
    }

    /// Requests a preview's deletion, or returns the one already pending.
    async fn delete_preview_on(
        tx: &mut Transaction<'_, Sqlite>,
        preview: &super::StoredEnvironment,
    ) -> Result<Accepted, StoreError> {
        let id = preview.id();
        let operation = match Self::latest_operation_on(tx, id).await? {
            Some(operation)
                if preview.delete_intent() && operation.kind == OperationKind::Delete =>
            {
                operation
            }
            _ => Self::request_delete_on(tx, id).await?,
        };
        Ok(Self::operation_accepted(&operation))
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
    /// its application's revision. Previews are `NotFound`: environment
    /// changes never act on them.
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
        current
            .filter(|current| current.environment.preview().is_none())
            .ok_or(StoreError::NotFound)
    }

    /// Advances the application revision past `current` after an environment
    /// is created, renamed, or deleted, so a caller that inspected the
    /// application before the change fails its precondition. Environments
    /// whose resolved target was current stay current, except `stale`: a
    /// renamed environment whose configuration renders differently.
    pub(super) async fn bump_generation_on(
        tx: &mut Transaction<'_, Sqlite>,
        application: &ApplicationId,
        current: u64,
        stale: Option<&EnvironmentId>,
    ) -> Result<(), StoreError> {
        let previous = i64::try_from(current).map_err(StoreError::invalid_input)?;
        let generation = previous.checked_add(1).ok_or(StoreError::InvalidInput)?;
        let id = application.as_str();
        sqlx::query!(
            "UPDATE applications SET generation=?1 WHERE id=?2",
            generation,
            id
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        let stale = stale.map(EnvironmentId::as_str);
        sqlx::query!(
            "UPDATE environments SET resolved_generation=?1 WHERE application_id=?2 AND resolved_generation=?3 AND id IS NOT ?4",
            generation,
            id,
            previous,
            stale
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        Ok(())
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
    /// edited field and resource once, in the application's history.
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
        let application = manifest.validate_template()?.normalize(id.clone());
        let field = match &edit {
            ApplicationEdit::Name(_) => "name",
            ApplicationEdit::Repository(_)
            | ApplicationEdit::RepositoryUrl(_)
            | ApplicationEdit::RepositoryPath(_) => "repository",
            ApplicationEdit::AddService(_) | ApplicationEdit::RemoveService(_) => "services",
            ApplicationEdit::Service { .. } => "service",
            ApplicationEdit::AddVolume(_)
            | ApplicationEdit::Volumes(_)
            | ApplicationEdit::RemoveVolume(_) => "volumes",
            ApplicationEdit::Routes(_) => "routes",
            ApplicationEdit::Jobs(_) => "jobs",
            ApplicationEdit::Variables(_) => "variables",
            ApplicationEdit::EnvironmentVisibility { .. } => "environments",
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
            sqlx::query!("INSERT INTO events(application_id,generation,kind,message,phase,resource,created_at_ms) VALUES(?1,?2,'application_edited','Saved application configuration',?3,?4,?5)",app_id,generation,field,resource,now).execute(&mut **tx).await.map_err(StoreError::database)?;
            saved
        };
        Self::deploy_saved(tx, &id, &mut saved, deploy).await?;
        Ok(Accepted {
            environments: Self::environment_ids_on(tx, &id).await?,
            response: MutationResponse::Saved(saved),
            wake: deploy,
        })
    }

    /// When `deploy` is set, starts a deployment of the just-saved configuration
    /// to the application's only environment, from its branch when
    /// repository-backed, and records its operation ID on `saved`.
    async fn deploy_saved(
        tx: &mut Transaction<'_, Sqlite>,
        id: &ApplicationId,
        saved: &mut piqueld_core::api::SavedApplication,
        deploy: bool,
    ) -> Result<(), StoreError> {
        if deploy {
            let environment = Self::sole_environment_on(tx, id).await?;
            let environment = Self::environment_on(tx, environment.as_str())
                .await?
                .ok_or(StoreError::NotFound)?;
            let operation = Self::request_deploy_on(tx, &environment).await?;
            Self::insert_deployment_on(tx, &operation, &environment.candidate(None)?).await?;
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
    /// when it was current and its configuration does not render the name,
    /// e.g. `${{ app.name }}`), updates the display name of every environment's
    /// resolved and latest targets, and records an `application_renamed` event
    /// once, in the application's history. Never wakes the controller.
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
        let application = current.application.clone().with_name(
            piqueld_core::ApplicationName::parse(name.clone())
                .map_err(StoreError::invalid_input)?,
        );
        let desired = serde_json::to_string(&application).map_err(StoreError::corrupt)?;
        sqlx::query!("UPDATE applications SET name=?1,desired_json=?2,generation=?3,updated_at_ms=?4 WHERE id=?5",name,desired,revision,now,id)
            .execute(&mut **tx).await.map_err(StoreError::constraint)?;
        // Resolved and latest targets may be retried after a rename. Only their display metadata changes.
        sqlx::query!("UPDATE environments SET resolved_json=CASE WHEN resolved_json IS NULL THEN NULL ELSE json_set(resolved_json,'$.name',?1) END WHERE application_id=?2",name,id)
            .execute(&mut **tx).await.map_err(StoreError::database)?;
        for environment in Self::deployables_on(tx, &id).await? {
            let target = environment.target();
            if current
                .application
                .renders_like(&target, &application, &target)
            {
                let environment = environment.id.as_str();
                sqlx::query!("UPDATE environments SET resolved_generation=?1 WHERE id=?2 AND resolved_generation=?3",revision,environment,previous)
                    .execute(&mut **tx).await.map_err(StoreError::database)?;
            }
        }
        sqlx::query!("UPDATE operations SET target_json=json_set(target_json,'$.name',?1) WHERE target_json IS NOT NULL AND id IN (SELECT (SELECT o.id FROM operations o WHERE o.environment_id=e.id ORDER BY o.created_at_ms DESC,o.id DESC LIMIT 1) FROM environments e WHERE e.application_id=?2)",name,id)
            .execute(&mut **tx).await.map_err(StoreError::database)?;
        if old_name.as_str() != name {
            let message = format!("renamed {old_name} to {name}");
            sqlx::query!("INSERT INTO events(application_id,generation,kind,message,created_at_ms) VALUES(?1,?2,'application_renamed',?3,?4)",id,revision,message,now).execute(&mut **tx).await.map_err(StoreError::database)?;
        }
        Ok(RenamedApplication {
            application_id: id,
            name,
            generation,
        })
    }

    /// Requests deletion of an application, every environment and every
    /// preview. With several environments, `confirmed` must name each of them;
    /// previews need no confirmation. Those already being deleted keep their
    /// pending operation. An application without any is removed immediately.
    async fn delete_application_on(
        tx: &mut Transaction<'_, Sqlite>,
        current: StoredApplication,
        mut confirmed: Vec<EnvironmentName>,
        now: i64,
    ) -> Result<Accepted, StoreError> {
        let id = current.application.id().as_str().to_owned();
        let environments = Self::deployables_on(tx, &id).await?;
        let names = environments
            .iter()
            .filter(|environment| environment.preview().is_none())
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

    /// Application deletions accepted before environments existed must still replay.
    #[tokio::test]
    async fn application_deletions_accepted_before_environments_replay() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("db")).await.unwrap();
        let id = ApplicationId::parse("app-legacy-01").unwrap();
        let original = Sha256::digest(br#"[{"kind":"delete","id":"app-legacy-01"},3,false]"#);
        let fingerprint = format!("{original:x}");
        let response = r#"{"Operation":{"operation_id":"operation-1","application_id":"app-legacy-01","generation":4}}"#;
        sqlx::query!(
            "INSERT INTO request_receipts(request_id,fingerprint,response_json,expires_at_ms) VALUES('legacy-delete',?1,?2,?3)",
            fingerprint,
            response,
            i64::MAX
        )
        .execute(&store.pool)
        .await
        .unwrap();
        let delete = || Mutation::DeleteApplication {
            id: id.clone(),
            environments: Vec::new(),
        };
        let (MutationResponse::Operation(replayed), wake) = store
            .accept(
                crate::store::Actor::Daemon,
                delete(),
                Some(3),
                false,
                Some("legacy-delete"),
            )
            .await
            .unwrap()
        else {
            panic!("legacy operation response")
        };
        assert!(!wake);
        assert_eq!(replayed.operation_id, "operation-1");
        assert_eq!(replayed.environment_id, "app-legacy-01");
        assert!(matches!(
            store
                .accept(
                    crate::store::Actor::Daemon,
                    delete(),
                    Some(2),
                    false,
                    Some("legacy-delete")
                )
                .await,
            Err(StoreError::ReplayConflict)
        ));
    }

    /// Saves a manifest configuring `staging` and `preview`, then creates the
    /// environments `staging` and `qa`.
    async fn configured_environments(store: &Store) -> (EnvironmentId, EnvironmentId) {
        let (_, [staging, qa]) = environments(
            store,
            "[spec.environments.staging.variables]\nlevel='debug'\n[spec.environments.preview.variables]\nlevel='trace'",
            ["staging", "qa"],
        )
        .await;
        (staging, qa)
    }

    /// Saves the application `notes` with `spec`, then creates the environments `names`.
    async fn environments<const N: usize>(
        store: &Store,
        spec: &str,
        names: [&str; N],
    ) -> (ApplicationId, [EnvironmentId; N]) {
        let template = piqueld_core::manifest::parse_template_toml(&format!(
            "api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='notes'\n{spec}"
        ))
        .unwrap();
        let (MutationResponse::Saved(saved), _) = store
            .accept(
                Actor::Daemon,
                Mutation::save(template, None, false),
                Some(0),
                false,
                None,
            )
            .await
            .unwrap()
        else {
            panic!("saved response")
        };
        let application = ApplicationId::parse(saved.application_id).unwrap();
        let mut ids = Vec::new();
        for name in names {
            let create = Mutation::CreateEnvironment {
                application: application.clone(),
                name: EnvironmentName::parse(name).unwrap(),
                branch: None,
            };
            let (MutationResponse::Environment(environment), _) = store
                .accept(Actor::Daemon, create, None, true, None)
                .await
                .unwrap()
            else {
                panic!("environment response")
            };
            ids.push(environment.id);
        }
        (application, ids.try_into().unwrap())
    }

    fn rename(id: &EnvironmentId, name: &str) -> Mutation {
        Mutation::RenameEnvironment {
            id: id.clone(),
            name: EnvironmentName::parse(name).unwrap(),
        }
    }

    /// Names select `[spec.environments.<name>]`, so a rename that would change
    /// the applied block is refused.
    #[tokio::test]
    async fn renames_never_switch_the_configuration_an_environment_deploys() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("db")).await.unwrap();
        let (staging, qa) = configured_environments(&store).await;
        for (id, name, configured) in [
            (&staging, "testing", "staging"),
            (&qa, "preview", "preview"),
        ] {
            assert!(matches!(
                store.accept(Actor::Daemon, rename(id, name), None, true, None).await,
                Err(StoreError::EnvironmentConfigured { environment })
                    if environment.as_str() == configured
            ));
        }
        store
            .accept(Actor::Daemon, rename(&qa, "testing"), None, true, None)
            .await
            .unwrap();
    }

    /// Renames keep a resolved environment resolved unless its configuration
    /// renders the changed name, so it reports the redeploy it needs.
    #[tokio::test]
    async fn renames_unresolve_environments_that_render_the_name() {
        for (value, keeps_environment_rename, keeps_application_rename) in [
            ("${{ vars.level }}", true, true),
            ("${{ env.name }}", false, true),
            ("${{ app.name }}", true, false),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let store = Store::open(directory.path().join("db")).await.unwrap();
            let (application, [id]) = environments(
                &store,
                &format!("[spec.variables]\nlevel='info'\n[[spec.services]]\nname='web'\n[spec.services.source]\ntype='image'\nimage='ghcr.io/example/notes:1.4.0'\n[spec.services.environment]\nVALUE='{value}'"),
                ["staging"],
            )
            .await;
            let resolved_after = async |mutation| {
                sqlx::query("UPDATE environments SET resolved_generation=(SELECT generation FROM applications)")
                    .execute(&store.pool)
                    .await
                    .unwrap();
                store
                    .accept(Actor::Daemon, mutation, None, false, None)
                    .await
                    .unwrap();
                let current = store.get(&id).await.unwrap();
                current.environment.resolved_generation == Some(current.application.generation)
            };
            assert_eq!(
                resolved_after(rename(&id, "qa")).await,
                keeps_environment_rename,
                "{value}"
            );
            let application = Mutation::Rename {
                id: application,
                name: "journal".into(),
            };
            assert_eq!(
                resolved_after(application).await,
                keeps_application_rename,
                "{value}"
            );
        }
    }

    /// Lifecycle changes advance the application revision, so a second change
    /// based on the same inspection fails instead of acting on a renamed environment.
    #[tokio::test]
    async fn environment_changes_from_a_stale_inspection_conflict() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("db")).await.unwrap();
        let (_, qa) = configured_environments(&store).await;
        let inspected = store.get(&qa).await.unwrap().application.generation;
        store
            .accept(
                Actor::Daemon,
                rename(&qa, "testing"),
                Some(inspected),
                false,
                None,
            )
            .await
            .unwrap();
        for stale in [rename(&qa, "review"), Mutation::Delete { id: qa.clone() }] {
            assert!(matches!(
                store.accept(Actor::Daemon, stale, Some(inspected), false, None).await,
                Err(StoreError::GenerationConflict { expected, actual })
                    if expected == inspected && actual == inspected + 1
            ));
        }
        assert_eq!(
            store.get(&qa).await.unwrap().environment.name.as_str(),
            "testing"
        );
    }

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
