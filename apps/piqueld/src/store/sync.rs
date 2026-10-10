//! What sync needs and records: the applications that sync, the branch head
//! each environment and preview last synced, each application's last branch
//! listing, and webhook secrets.
use super::acceptance::Accepted;
use super::{Actor, Store, StoreError, StoredEnvironment, now_ms};
use crate::api::{ListedHead, MutationResponse, SyncOutcome};
use crate::secrets::{Envelope, Generate, SecretCipher, SecretOwner};
use piqueld_core::{
    ApplicationId, EnvironmentId,
    access::{AppPermission, Permission},
    api::{AcceptedOperation, EnvironmentView},
    manifest::{
        ApplicationTemplate, ManifestRevision, RepositoryManifest, SecretEncoding, SecretGenerator,
    },
    sync::SyncCheck,
};
use sqlx::{Sqlite, Transaction};
use zeroize::Zeroizing;

/// The head of the branch an environment or preview tracks, as one of its
/// deployments found it while fetching: what sync follows from.
pub(crate) struct BranchHead {
    /// URL of the repository it was read from.
    pub(crate) url: String,
    /// The tracked branch.
    pub(crate) branch: String,
    /// Full commit hash at its head.
    pub(crate) commit: String,
}

/// An application that syncs, with everything following its pushes.
pub(crate) struct SyncApplication {
    /// Stable application ID.
    pub(crate) id: ApplicationId,
    /// Its repository connection, whose `sync` is not off.
    pub(crate) connection: RepositoryManifest,
    /// Its environments and previews that follow pushes.
    pub(crate) following: Vec<EnvironmentView>,
}

impl Store {
    /// Lists every application that syncs and is not being deleted, with the
    /// environments and previews that follow its pushes.
    ///
    /// # Errors
    /// Returns a storage or decoding error.
    pub(crate) async fn sync_applications(&self) -> Result<Vec<SyncApplication>, StoreError> {
        let mut connection = self.pool.acquire().await.map_err(StoreError::database)?;
        let rows = sqlx::query!(
            r#"SELECT id AS "id!",desired_json FROM applications WHERE delete_intent=0 AND json_extract(desired_json,'$.spec.manifest.sync.mode') IS NOT NULL ORDER BY id"#
        )
        .fetch_all(&mut *connection)
        .await
        .map_err(StoreError::database)?;
        let mut applications = Vec::new();
        for row in rows {
            let template: ApplicationTemplate =
                serde_json::from_str(&row.desired_json).map_err(StoreError::corrupt)?;
            let Some(connection_settings) = template.spec().manifest.clone() else {
                continue;
            };
            if connection_settings.sync.is_off() {
                continue;
            }
            let following = Self::deployables_on(&mut connection, &row.id)
                .await?
                .into_iter()
                .filter(|deployable| {
                    !deployable.delete_intent
                        && deployable.sync_state(Some(&connection_settings)).listed()
                })
                .collect();
            applications.push(SyncApplication {
                id: template.id().clone(),
                connection: connection_settings,
                following,
            });
        }
        Ok(applications)
    }

    /// Deploys `head`, the commit sync found at `branch` of `repository`, to
    /// environment or preview `id` (see [`ListedHead`]), once the branch
    /// moved past the head of its last deployment:
    ///
    /// 1. Anything not following that branch of that repository (re-pointed,
    ///    reconnected, pinned, not opted in, its application's sync off, or
    ///    being deleted) is `Current`.
    /// 2. While a deployment runs without failing, or once one recorded a head
    ///    since the listing started, `Busy`, since it may still record, or
    ///    recorded, a newer head. The caller tries again once it finished, so
    ///    pushes during a deployment coalesce into one follow-up that deploys
    ///    the head of then. A failing deployment is superseded instead, since
    ///    the push may fix it.
    /// 3. The head of its last deployment (see [`Self::save_deployment_input`]),
    ///    and anything before its first, is `Current`. A one-off deployment
    ///    records the branch's head then, so it stays until the branch moves;
    ///    a rollback is a move.
    /// 4. Otherwise deploys exactly `head` and records it: `Deployed`,
    ///    waking the controller. A failed one is not retried until the
    ///    branch moves again.
    pub(super) async fn sync_on(
        tx: &mut Transaction<'_, Sqlite>,
        listed: ListedHead,
        now: i64,
    ) -> Result<Accepted, StoreError> {
        let outcome = Self::sync_outcome_on(tx, &listed, now).await?;
        Ok(Accepted {
            wake: matches!(outcome, SyncOutcome::Deployed(_)),
            response: MutationResponse::Synced(outcome),
            environments: vec![listed.id],
        })
    }

    /// What [`Self::sync_on`] does with `head`.
    async fn sync_outcome_on(
        tx: &mut Transaction<'_, Sqlite>,
        listed: &ListedHead,
        now: i64,
    ) -> Result<SyncOutcome, StoreError> {
        let ListedHead {
            id,
            repository,
            branch,
            head,
            since,
        } = listed;
        let (repository, head) = (repository.as_str(), head.as_str());
        let current = Self::environment_on(tx, id.as_str())
            .await?
            .ok_or(StoreError::NotFound)?;
        let connection = current.application.application.spec().manifest.as_ref();
        let following = !current.delete_intent()
            && current.environment.sync_state(connection).listed()
            && connection.is_some_and(|connection| connection.repository.url == repository)
            && current
                .environment
                .source
                .branch()
                .is_some_and(|tracked| tracked.branch() == branch.as_str());
        if !following {
            return Ok(SyncOutcome::Current);
        }
        // A running deployment may still record another head, so a push
        // waits for it, even before its first deployment.
        // A deployment that finished since the listing started, even of the
        // same commit, may be newer than the head listed: list again.
        if current.environment.synced != *since
            || Self::latest_operation_on(tx, id)
                .await?
                .is_some_and(|operation| {
                    !operation.state.terminal() && operation.error_code.is_none()
                })
        {
            return Ok(SyncOutcome::Busy);
        }
        if current
            .environment
            .synced
            .as_ref()
            .is_none_or(|synced| synced.commit == head)
        {
            return Ok(SyncOutcome::Current);
        }
        let operation = Self::deploy_revision_on(
            tx,
            &current,
            Some(&ManifestRevision::Commit(head.to_owned())),
        )
        .await?;
        let message = format!("branch {branch} moved to {head}");
        Self::operation_event(tx, &operation.id, "branch_synced", Some(&message), now).await?;
        Self::record_synced_on(tx, id, head, now).await?;
        Ok(SyncOutcome::Deployed(AcceptedOperation::from(&operation)))
    }

    /// Starts a deployment of an environment or preview from its source, at
    /// `revision` when given.
    pub(super) async fn deploy_revision_on(
        tx: &mut Transaction<'_, Sqlite>,
        environment: &StoredEnvironment,
        revision: Option<&ManifestRevision>,
    ) -> Result<super::Operation, StoreError> {
        let application = environment.candidate(revision)?;
        let operation = Self::request_deploy_on(tx, environment).await?;
        Self::insert_deployment_on(tx, &operation, &application).await?;
        Ok(operation)
    }

    /// Records `head` as the one environment or preview `id` follows from,
    /// while it still follows that branch, unpinned, of that repository. A
    /// fetch from a branch or repository it left records nothing.
    pub(super) async fn record_branch_head_on(
        tx: &mut Transaction<'_, Sqlite>,
        id: &EnvironmentId,
        head: &BranchHead,
        now: i64,
    ) -> Result<(), StoreError> {
        let Some(current) = Self::environment_on(tx, id.as_str()).await? else {
            return Ok(());
        };
        let tracks = current.repository().is_some_and(|repository| {
            repository.repository.url == head.url
                && repository.repository.branch == head.branch
                && repository.repository.commit.is_none()
        });
        if tracks {
            Self::record_synced_on(tx, id, &head.commit, now).await?;
        }
        Ok(())
    }

    /// Whether sync requested `operation`, which already recorded the head
    /// it deploys.
    ///
    /// # Errors
    /// Returns a storage error.
    pub(crate) async fn requested_by_sync(
        &self,
        operation: &super::Operation,
    ) -> Result<bool, StoreError> {
        Ok(sqlx::query_scalar!(
            r#"SELECT actor_system IS NOT NULL AS "sync!: bool" FROM operations WHERE id=?1"#,
            operation.id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)?
        .unwrap_or(false))
    }

    /// Records `head` as the branch head of `id`'s last deployment.
    async fn record_synced_on(
        tx: &mut Transaction<'_, Sqlite>,
        id: &EnvironmentId,
        head: &str,
        now: i64,
    ) -> Result<(), StoreError> {
        let id = id.as_str();
        sqlx::query!(
            "UPDATE environments SET synced_commit=?1,synced_at_ms=?2 WHERE id=?3",
            head,
            now,
            id
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        Ok(())
    }

    /// Opts environment `id` in or out of its application's sync, recording
    /// `environment_sync_changed`, and responds with it. Neither changes what it deploys, so the
    /// application revision stays. Opting back in deploys the branch head at
    /// the next sync if it moved since the last deployment. Previews always
    /// sync: they are `NotFound`.
    pub(super) async fn set_sync_on(
        tx: &mut Transaction<'_, Sqlite>,
        id: EnvironmentId,
        enabled: bool,
        now: i64,
    ) -> Result<Accepted, StoreError> {
        let environment = Self::checked_environment_on(tx, &id, None).await?;
        if environment.delete_intent() {
            return Err(StoreError::Busy);
        }
        if environment.environment.sync == enabled {
            return Self::environment_accepted(tx, id).await;
        }
        let (row, value) = (id.as_str(), i64::from(enabled));
        sqlx::query!(
            "UPDATE environments SET sync=?1,updated_at_ms=?2 WHERE id=?3",
            value,
            now,
            row
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        let name = &environment.environment.name;
        let message = if enabled {
            format!("pushes deploy environment {name} again")
        } else {
            format!("pushes no longer deploy environment {name}")
        };
        sqlx::query!(
            "INSERT INTO events(application_id,environment_id,kind,message,created_at_ms) VALUES((SELECT application_id FROM environments WHERE id=?1),?1,'environment_sync_changed',?2,?3)",
            row,
            message,
            now
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        Self::environment_accepted(tx, id).await
    }

    /// Records that sync listed `application`'s branches now, and why it
    /// failed, if it did.
    ///
    /// # Errors
    /// Returns a storage error.
    pub(crate) async fn record_sync_check(
        &self,
        application: &ApplicationId,
        error: Option<&str>,
    ) -> Result<(), StoreError> {
        let _writer = self.writers.lock().await;
        let (id, now) = (application.as_str(), now_ms());
        sqlx::query!(
            "UPDATE applications SET sync_checked_at_ms=?1,sync_error=?2 WHERE id=?3",
            now,
            error,
            id
        )
        .execute(&self.pool)
        .await
        .map_err(StoreError::database)?;
        Ok(())
    }

    /// The last time sync listed `application`'s branches, if it did.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn sync_check(
        &self,
        application: &ApplicationId,
    ) -> Result<Option<SyncCheck>, StoreError> {
        let id = application.as_str();
        Ok(sqlx::query!(
            "SELECT sync_checked_at_ms,sync_error FROM applications WHERE id=?1",
            id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)?
        .and_then(|row| {
            Some(SyncCheck {
                checked_at_ms: row.sync_checked_at_ms?,
                error: row.sync_error,
            })
        }))
    }

    /// Generates a new secret for `application`'s push webhooks, replacing
    /// any previous one, and records `webhook_secret_generated`. Returns the
    /// secret, 32 random bytes as hex, and when it was generated. `actor`
    /// needs `apps:write` on the application, checked in the transaction.
    ///
    /// # Errors
    /// Returns refusal, absence, `Busy` during deletion, secret storage, or
    /// database errors.
    pub async fn generate_webhook_secret(
        &self,
        actor: Actor<'_>,
        application: &ApplicationId,
    ) -> Result<(Zeroizing<String>, i64), StoreError> {
        let (_writer, mut tx) = self.begin_immediate().await?;
        actor
            .require_on(
                &mut tx,
                Permission::App(AppPermission::Write),
                Some(application),
            )
            .await?;
        let current = Self::application_on(&mut tx, application.as_str())
            .await?
            .ok_or(StoreError::NotFound)?;
        if current.delete_intent {
            return Err(StoreError::Busy);
        }
        let mut generated = SecretGenerator::Random {
            bytes: 32,
            encoding: SecretEncoding::Hex,
        }
        .generate()
        .map_err(StoreError::SecretSource)?;
        let secret = Zeroizing::new(
            String::from_utf8(std::mem::take(&mut *generated)).map_err(StoreError::corrupt)?,
        );
        let envelope = self
            .verified_secret_cipher(&mut tx)
            .await?
            .encrypt(
                SecretOwner::Webhook(application),
                "webhook",
                1,
                secret.as_bytes(),
            )
            .map_err(StoreError::SecretSource)?;
        let (id, now) = (application.as_str(), now_ms());
        sqlx::query!(
            "INSERT INTO webhook_secrets(application_id,nonce,ciphertext,created_at_ms) VALUES(?1,?2,?3,?4) ON CONFLICT(application_id) DO UPDATE SET nonce=excluded.nonce,ciphertext=excluded.ciphertext,created_at_ms=excluded.created_at_ms",
            id,
            envelope.nonce,
            envelope.ciphertext,
            now
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        let by = actor.attribution();
        let operator = by.operator_uid();
        sqlx::query!(
            "INSERT INTO events(application_id,kind,message,created_at_ms,actor_user_id,actor_credential_id,actor_operator_uid) VALUES(?1,'webhook_secret_generated','Generated a new webhook secret; the previous one no longer verifies',?2,?3,?4,?5)",
            id,
            now,
            by.user_id,
            by.credential_id,
            operator
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        tx.commit().await.map_err(StoreError::database)?;
        Ok((secret, now))
    }

    /// When `application`'s webhook secret was generated, if it has one.
    ///
    /// # Errors
    /// Returns a storage error.
    pub async fn webhook_secret_created(
        &self,
        application: &ApplicationId,
    ) -> Result<Option<i64>, StoreError> {
        let id = application.as_str();
        sqlx::query_scalar!(
            "SELECT created_at_ms FROM webhook_secrets WHERE application_id=?1",
            id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)
    }

    /// Decrypts `application`'s webhook secret, if it has one.
    ///
    /// # Errors
    /// Returns secret storage or database errors.
    pub(crate) async fn webhook_secret(
        &self,
        application: &ApplicationId,
    ) -> Result<Option<Zeroizing<Vec<u8>>>, StoreError> {
        let id = application.as_str();
        let Some(row) = sqlx::query!(
            "SELECT nonce,ciphertext FROM webhook_secrets WHERE application_id=?1",
            id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)?
        else {
            return Ok(None);
        };
        SecretCipher::load(&self.secret_key_path, true)
            .and_then(|cipher| {
                cipher.decrypt(
                    SecretOwner::Webhook(application),
                    "webhook",
                    1,
                    &Envelope {
                        nonce: row.nonce,
                        ciphertext: row.ciphertext,
                    },
                )
            })
            .map(Some)
            .map_err(StoreError::SecretSource)
    }
}

#[cfg(test)]
impl Store {
    /// The database, for tests reading what sync recorded.
    pub(crate) fn pool(&self) -> &sqlx::SqlitePool {
        &self.pool
    }

    /// Deploys `id` from its source and marks the deployment succeeded,
    /// having fetched `commit` from its branch, as a finished deployment would.
    pub(crate) async fn deployed_at(&self, id: &EnvironmentId, commit: &str) -> super::Operation {
        let (writer, mut tx) = self.begin_immediate().await.unwrap();
        let current = Self::environment_on(&mut tx, id.as_str())
            .await
            .unwrap()
            .unwrap();
        let operation = Self::deploy_revision_on(&mut tx, &current, None)
            .await
            .unwrap();
        sqlx::query(
            "UPDATE deployment_inputs SET fetched=1,repository_commit=?1 WHERE operation_id=?2",
        )
        .bind(commit)
        .bind(&operation.id)
        .execute(&mut *tx)
        .await
        .unwrap();
        sqlx::query("UPDATE operations SET state='succeeded',finished_at_ms=1 WHERE id=?1")
            .bind(&operation.id)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
        drop(writer);
        // As fetching its own branch's head does.
        if let Some(repository) = current.repository() {
            let head = BranchHead {
                url: repository.repository.url,
                branch: repository.repository.branch,
                commit: commit.into(),
            };
            let (_writer, mut tx) = self.begin_immediate().await.unwrap();
            Self::record_branch_head_on(&mut tx, id, &head, super::now_ms())
                .await
                .unwrap();
            tx.commit().await.unwrap();
        }
        operation
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Mutation, MutationResponse};
    use piqueld_core::manifest::parse_template_toml;
    use piqueld_core::sync::{RepositorySync, SyncedHead};

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const C: &str = "cccccccccccccccccccccccccccccccccccccccc";

    /// Saves the repository-backed `notes`, polling `main`, and returns its
    /// only environment, opted in and never deployed.
    async fn notes(store: &Store) -> EnvironmentId {
        let template = parse_template_toml(
            "api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='notes'\n[spec.manifest]\npath='app.toml'\nsync={mode='poll'}\n[spec.manifest.repository]\nurl='https://example.com/notes.git'\nbranch='main'\n",
        )
        .unwrap()
        .normalize(ApplicationId::parse("app-pending-01").unwrap());
        let mutation = Mutation::Save {
            application: Box::new(template),
            expected_application_id: None,
            deploy: false,
        };
        let (MutationResponse::Saved(saved), _) = store
            .accept(Actor::Daemon, mutation, Some(0), false, None)
            .await
            .unwrap()
        else {
            panic!("saved")
        };
        let application = ApplicationId::parse(saved.application_id).unwrap();
        let id = store.environments(&application).await.unwrap()[0]
            .id
            .clone();
        let opt_in = Mutation::SetSync {
            id: id.clone(),
            enabled: true,
        };
        store
            .accept(Actor::Daemon, opt_in, None, false, None)
            .await
            .unwrap();
        id
    }

    /// Submits sync's mutation for `head` at `main`, as `sync:poll`.
    async fn sync(store: &Store, id: &EnvironmentId, head: &str) -> SyncOutcome {
        listed(store, id, "https://example.com/notes.git", head).await
    }

    /// Submits sync's mutation for `head` at `main` of `repository`, listed
    /// since the current head of its last deployment.
    async fn listed(
        store: &Store,
        id: &EnvironmentId,
        repository: &str,
        head: &str,
    ) -> SyncOutcome {
        let since = store.get(id).await.unwrap().environment.synced;
        submit(store, id, repository, head, since).await
    }

    /// Submits sync's mutation for `head` at `main` of `repository`, listed
    /// since `since`.
    async fn submit(
        store: &Store,
        id: &EnvironmentId,
        repository: &str,
        head: &str,
        since: Option<SyncedHead>,
    ) -> SyncOutcome {
        let mutation = Mutation::Sync(ListedHead {
            id: id.clone(),
            repository: repository.into(),
            branch: piqueld_core::GitBranch::parse("main").unwrap(),
            head: head.into(),
            since,
        });
        let actor = Actor::System(piqueld_core::sync::SystemActor::SyncPoll);
        match store
            .accept(actor, mutation, None, false, None)
            .await
            .unwrap()
        {
            (MutationResponse::Synced(outcome), _) => outcome,
            _ => panic!("synced"),
        }
    }

    async fn operations(store: &Store, id: &EnvironmentId) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM operations WHERE environment_id=?1")
            .bind(id.as_str())
            .fetch_one(&store.pool)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_moved_head_deploys_once_by_its_actor_and_pushes_coalesce_while_deploying() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("db")).await.unwrap();
        let id = notes(&store).await;
        store.deployed_at(&id, A).await;

        let SyncOutcome::Deployed(deployed) = sync(&store, &id, B).await else {
            panic!("deployed")
        };
        // It deploys exactly the head sync listed, not whatever the branch
        // holds when it fetches.
        let operation = store.operation(&deployed.operation_id).await.unwrap();
        let input = store.deployment_input(&operation).await.unwrap().unwrap();
        let fetched = input.template.spec().manifest.clone().unwrap().repository;
        assert_eq!(
            (fetched.branch.as_str(), fetched.commit.as_deref()),
            ("main", Some(B))
        );
        let actors: Vec<(String, Option<String>)> = sqlx::query_as(
            "SELECT kind,actor_system FROM events WHERE operation_id=?1 ORDER BY id",
        )
        .bind(&deployed.operation_id)
        .fetch_all(&store.pool)
        .await
        .unwrap();
        assert!(actors.iter().any(|(kind, _)| kind == "branch_synced"));
        assert!(
            actors
                .iter()
                .all(|(_, actor)| actor.as_deref() == Some("sync:poll")),
            "{actors:?}"
        );
        let attributed: Option<String> =
            sqlx::query_scalar("SELECT actor_system FROM operations WHERE id=?1")
                .bind(&deployed.operation_id)
                .fetch_one(&store.pool)
                .await
                .unwrap();
        assert_eq!(attributed.as_deref(), Some("sync:poll"));
        assert_eq!(
            store
                .get(&id)
                .await
                .unwrap()
                .environment
                .synced
                .unwrap()
                .commit,
            B
        );

        // A push while that deployment runs waits for it instead of
        // superseding it.
        assert!(matches!(sync(&store, &id, B).await, SyncOutcome::Busy));
        assert!(matches!(sync(&store, &id, C).await, SyncOutcome::Busy));
        assert_eq!(operations(&store, &id).await, 2);

        // A failing deployment is superseded: the push may fix it.
        sqlx::query("UPDATE operations SET state='running',error_code='image_pull_failed',error_message='failed' WHERE id=?1")
            .bind(&deployed.operation_id)
            .execute(&store.pool)
            .await
            .unwrap();
        let SyncOutcome::Deployed(retrying) = sync(&store, &id, C).await else {
            panic!("deployed")
        };
        assert_eq!(
            store.operation(&deployed.operation_id).await.unwrap().state,
            piqueld_core::OperationState::Superseded
        );

        // C fails before fetching too, then the branch is rolled back to A.
        // A deployed once, but C's retry would replace it: A deploys again.
        sqlx::query("UPDATE operations SET state='running',error_code='image_pull_failed',error_message='failed' WHERE id=?1")
            .bind(&retrying.operation_id)
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(matches!(
            sync(&store, &id, A).await,
            SyncOutcome::Deployed(_)
        ));
        assert_eq!(
            store.operation(&retrying.operation_id).await.unwrap().state,
            piqueld_core::OperationState::Superseded
        );
    }

    #[tokio::test]
    async fn a_rollback_after_a_manual_deployment_deploys_the_branch_head_again() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("db")).await.unwrap();
        let id = notes(&store).await;
        store.deployed_at(&id, A).await;
        assert!(matches!(sync(&store, &id, A).await, SyncOutcome::Current));
        // A listing that started before another deployment finished never
        // undoes it, even when that deployment recorded the same commit
        // again: it lists again.
        let before = store.get(&id).await.unwrap().environment.synced;
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        store.deployed_at(&id, A).await;
        let stale = submit(&store, &id, "https://example.com/notes.git", B, before).await;
        assert!(matches!(stale, SyncOutcome::Busy));
        // Someone deploys the branch at C by hand, then it goes back to A.
        store.deployed_at(&id, C).await;
        assert!(matches!(sync(&store, &id, C).await, SyncOutcome::Current));
        assert!(matches!(
            sync(&store, &id, A).await,
            SyncOutcome::Deployed(_)
        ));
        // Once deployed, the same head never deploys twice.
        sqlx::query("UPDATE operations SET state='succeeded',finished_at_ms=1 WHERE environment_id=?1 AND state='requested'")
            .bind(id.as_str())
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(matches!(sync(&store, &id, A).await, SyncOutcome::Current));
        // A rollback while a deployment by hand runs waits for it: that
        // deployment may still record another head.
        let deploy = Mutation::deploy(id.clone());
        store
            .accept(Actor::Daemon, deploy, None, true, None)
            .await
            .unwrap();
        assert!(matches!(sync(&store, &id, A).await, SyncOutcome::Busy));
    }

    #[tokio::test]
    async fn nothing_follows_before_its_first_deployment_nor_while_pinned_or_opted_out() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("db")).await.unwrap();
        let id = notes(&store).await;

        // Never deployed: pushes do nothing, not even record a head.
        assert!(matches!(sync(&store, &id, A).await, SyncOutcome::Current));
        assert_eq!(store.get(&id).await.unwrap().environment.synced, None);
        // While its first deployment runs, a push waits for it, since that
        // records the head it fetched; one that failed records nothing.
        let deploy = Mutation::deploy(id.clone());
        store
            .accept(Actor::Daemon, deploy, None, true, None)
            .await
            .unwrap();
        assert!(matches!(sync(&store, &id, A).await, SyncOutcome::Busy));
        sqlx::query("UPDATE operations SET state='failed',finished_at_ms=1,error_code='manifest_invalid',error_message='invalid' WHERE environment_id=?1")
            .bind(id.as_str())
            .execute(&store.pool)
            .await
            .unwrap();
        assert!(matches!(sync(&store, &id, A).await, SyncOutcome::Current));
        // A deployment's head is what it follows from.
        store.deployed_at(&id, B).await;
        assert!(matches!(sync(&store, &id, B).await, SyncOutcome::Current));
        // Heads listed from a repository it no longer reads deploy nothing.
        let elsewhere = listed(&store, &id, "https://example.com/fork.git", C).await;
        assert!(matches!(elsewhere, SyncOutcome::Current));
        assert_eq!(operations(&store, &id).await, 2);

        let opt_out = Mutation::SetSync {
            id: id.clone(),
            enabled: false,
        };
        store
            .accept(Actor::Daemon, opt_out, None, false, None)
            .await
            .unwrap();
        assert!(matches!(sync(&store, &id, C).await, SyncOutcome::Current));
        let opt_in = Mutation::SetSync {
            id: id.clone(),
            enabled: true,
        };
        store
            .accept(Actor::Daemon, opt_in, None, false, None)
            .await
            .unwrap();
        let pin = Mutation::SetBranch {
            id: id.clone(),
            branch: piqueld_core::TrackedBranch::new("main".into(), Some(B.into())).unwrap(),
        };
        store
            .accept(Actor::Daemon, pin, None, true, None)
            .await
            .unwrap();
        assert!(matches!(sync(&store, &id, C).await, SyncOutcome::Current));
        // Unpinned again, it follows only from its next deployment.
        let unpin = Mutation::SetBranch {
            id: id.clone(),
            branch: piqueld_core::TrackedBranch::new("main".into(), None).unwrap(),
        };
        store
            .accept(Actor::Daemon, unpin, None, true, None)
            .await
            .unwrap();
        assert!(matches!(sync(&store, &id, C).await, SyncOutcome::Current));
        assert_eq!(operations(&store, &id).await, 2);
    }

    #[tokio::test]
    async fn only_turning_sync_on_or_opting_in_needs_the_permission_to_deploy() {
        use piqueld_core::access::Scope;
        use piqueld_core::edit::ApplicationEdit;
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("db")).await.unwrap();
        let id = notes(&store).await;
        let application = store.get(&id).await.unwrap().environment.application_id;
        let user = piqueld_core::auth::User {
            id: "alice".into(),
            username: "alice".into(),
            display_name: String::new(),
        };
        let mut grants = piqueld_core::access::Grants::default();
        grants
            .grant(
                Permission::App(AppPermission::Write),
                &Scope::one(application.clone()),
            )
            .unwrap();
        store.seed_auth_user(&user, &grants).await;
        let credential = super::super::NewCredential {
            id: "session".into(),
            secret_hash: "session".into(),
            kind: super::super::CredentialKind::Cli,
            name: "session",
            expires_at: None,
            grants: None,
        };
        store
            .insert_credential(&user.id, &credential)
            .await
            .unwrap();
        let alice = Actor::Account(super::super::Caller {
            credential_id: "session",
            user_id: "alice",
        });
        let edit = |edit| Mutation::Edit {
            id: application.clone(),
            edit: Box::new(edit),
            deploy: false,
        };
        let opt_in = || Mutation::SetSync {
            id: id.clone(),
            enabled: true,
        };
        let poll = RepositorySync::Poll {
            interval_seconds: 60,
        };
        let refused = |result: Result<_, StoreError>| {
            assert!(
                matches!(result, Err(StoreError::Denied(_))),
                "{:?}",
                result.err()
            );
        };
        // While it syncs, opting in lets sync deploy the environment.
        refused(store.accept(alice, opt_in(), None, false, None).await);
        // Changing branches, repositories, or environments deploys nothing:
        // each follows only from its next deployment.
        let create = Mutation::CreateEnvironment {
            application: application.clone(),
            name: piqueld_core::EnvironmentName::parse("staging").unwrap(),
            branch: None,
        };
        let branch = Mutation::SetBranch {
            id: id.clone(),
            branch: piqueld_core::TrackedBranch::new("release".into(), None).unwrap(),
        };
        let url = edit(ApplicationEdit::RepositoryUrl(
            "https://example.com/fork.git".into(),
        ));
        for mutation in [
            create,
            branch,
            url,
            edit(ApplicationEdit::RepositorySync(poll)),
        ] {
            store
                .accept(alice, mutation, None, true, None)
                .await
                .unwrap();
        }
        // Turning it off needs nothing more; turning it back on does.
        let off = edit(ApplicationEdit::RepositorySync(RepositorySync::Off));
        store.accept(alice, off, None, true, None).await.unwrap();
        refused(
            store
                .accept(
                    alice,
                    edit(ApplicationEdit::RepositorySync(poll)),
                    None,
                    true,
                    None,
                )
                .await,
        );
        // Opting in while it does not sync deploys nothing yet.
        store
            .accept(alice, opt_in(), None, false, None)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn a_deployment_records_its_head_only_while_it_follows_that_branch_and_repository() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::open(temp.path().join("db")).await.unwrap();
        let id = notes(&store).await;
        store.deployed_at(&id, A).await;
        let record = |url: &str, branch: &str| {
            let head = BranchHead {
                url: url.into(),
                branch: branch.into(),
                commit: C.into(),
            };
            let store = &store;
            let id = id.clone();
            async move {
                let (_writer, mut tx) = store.begin_immediate().await.unwrap();
                Store::record_branch_head_on(&mut tx, &id, &head, 1)
                    .await
                    .unwrap();
                tx.commit().await.unwrap();
            }
        };
        // A fetch from a branch or repository it left changes nothing.
        record("https://example.com/notes.git", "release").await;
        record("https://example.com/fork.git", "main").await;
        assert_eq!(
            store
                .get(&id)
                .await
                .unwrap()
                .environment
                .synced
                .unwrap()
                .commit,
            A
        );
        record("https://example.com/notes.git", "main").await;
        assert_eq!(
            store
                .get(&id)
                .await
                .unwrap()
                .environment
                .synced
                .unwrap()
                .commit,
            C
        );
        // A deployment remembers the branch it was requested for, so one
        // queued before a branch change never records the new branch's head.
        let deploy = Mutation::deploy(id.clone());
        let (MutationResponse::Operation(queued), _) = store
            .accept(Actor::Daemon, deploy, None, true, None)
            .await
            .unwrap()
        else {
            panic!("operation")
        };
        let release = Mutation::SetBranch {
            id: id.clone(),
            branch: piqueld_core::TrackedBranch::new("release".into(), None).unwrap(),
        };
        store
            .accept(Actor::Daemon, release, None, true, None)
            .await
            .unwrap();
        let queued = store.operation(&queued.operation_id).await.unwrap();
        let input = store.deployment_input(&queued).await.unwrap().unwrap();
        assert_eq!(input.tracked_branch.as_deref(), Some("main"));
    }
}
