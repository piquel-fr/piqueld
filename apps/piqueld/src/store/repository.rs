//! Durable candidate manifests for manual deployments.
//!
//! `deployment_inputs` starts with the configuration captured by Deploy. After
//! fetching, it holds the validated manifest and repository commit; `fetched`
//! tells retries to reuse that snapshot instead of reading a moving branch again.
//! Deployments without repository backing also become fetched, with no commit.
//! The candidate, its rendering, and any warnings are copied to deployment
//! history when fetched, but the candidate only becomes its environment's last
//! fetched manifest after source preparation succeeds.
use super::{Operation, Store, StoreError, now_ms};
use piqueld_core::{
    EnvironmentKind,
    api::{DeploymentOrigin, DiagnosticView},
    manifest::{ApplicationTemplate, Rendering},
};
use sqlx::{Sqlite, Transaction};

/// A deployment's candidate manifest and whether it is the fetched snapshot.
pub(crate) struct DeploymentInput {
    /// The candidate manifest, with references unresolved.
    pub(crate) template: ApplicationTemplate,
    pub(crate) fetched: bool,
    /// Commit the fetched manifest was read from.
    pub(crate) commit: Option<String>,
    /// The branch its environment tracked, unpinned, when it was requested.
    pub(crate) tracked_branch: Option<String>,
    /// Where the deployment comes from: a local snapshot is fetched from
    /// its own repository instead of the branch.
    pub(crate) origin: DeploymentOrigin,
}

impl Store {
    /// Captures the configuration a Deploy started from, still unfetched, with
    /// the branch its environment tracks unpinned now.
    pub(crate) async fn insert_deployment_on(
        tx: &mut Transaction<'_, Sqlite>,
        operation: &Operation,
        application: &ApplicationTemplate,
    ) -> Result<(), StoreError> {
        let json = application.canonical_json().map_err(StoreError::corrupt)?;
        let environment = operation.environment_id.as_str();
        sqlx::query!(
            "INSERT INTO deployment_inputs(operation_id,application_json,tracked_branch) VALUES(?1,?2,(SELECT branch FROM environments WHERE id=?3 AND pinned_commit IS NULL))",
            operation.id,
            json,
            environment
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
            r#"SELECT i.application_json,i.fetched,i.repository_commit,i.tracked_branch,COALESCE(d.origin_json,'{"type":"build"}') AS "origin_json!: String" FROM deployment_inputs i LEFT JOIN deployments d ON d.id=i.operation_id WHERE i.operation_id=?1"#,
            operation.id
        )
        .fetch_optional(&self.pool)
        .await
        .map_err(StoreError::database)?
        .map(|row| {
            Ok(DeploymentInput {
                template: serde_json::from_str(&row.application_json)
                    .map_err(StoreError::corrupt)?,
                fetched: row.fetched != 0,
                commit: row.repository_commit,
                tracked_branch: row.tracked_branch,
                origin: serde_json::from_str(&row.origin_json).map_err(StoreError::corrupt)?,
            })
        })
        .transpose()
    }

    /// Stores the fetched manifest and commit for the latest running operation,
    /// copying it, its rendering, and `warnings` into deployment history,
    /// pinning its secret versions, recording the `branch_head` sync follows
    /// from (see `record_branch_head_on`), and re-checking hostname
    /// reservations. Fetching happens once: a second save, or one for
    /// superseded work, fails with `StoreError::IllegalTransition`.
    pub(crate) async fn save_deployment_input(
        &self,
        operation: &Operation,
        template: &ApplicationTemplate,
        rendering: &Rendering,
        (commit, branch_head): (Option<&str>, Option<&super::BranchHead>),
        warnings: &[DiagnosticView],
    ) -> Result<(), StoreError> {
        let json = template.canonical_json().map_err(StoreError::corrupt)?;
        let (manifest, variables) = Self::snapshot_json(Some(rendering))?;
        let warnings = serde_json::to_string(warnings).map_err(StoreError::corrupt)?;
        let application = &rendering.application;
        self.generate_secrets(&operation.environment_id, application)
            .await?;
        let (_writer, mut tx) = self.begin_immediate().await?;
        let app_id = operation.environment_id.as_str();
        let changed = sqlx::query!("UPDATE deployment_inputs SET application_json=?1,repository_commit=?2,fetched=1 WHERE operation_id=?3 AND fetched=0 AND operation_id=(SELECT id FROM operations WHERE environment_id=?4 ORDER BY created_at_ms DESC,id DESC LIMIT 1) AND EXISTS(SELECT 1 FROM operations WHERE id=?3 AND state='running')", json,commit,operation.id,app_id).execute(&mut *tx).await.map_err(StoreError::database)?.rows_affected();
        if changed != 1 {
            return Err(StoreError::IllegalTransition);
        }
        sqlx::query!(
            "UPDATE deployments SET manifest_json=?1,template_json=?2,variables_json=?3,warnings_json=?4 WHERE id=?5",
            manifest,
            json,
            variables,
            warnings,
            operation.id
        )
        .execute(&mut *tx)
        .await
        .map_err(StoreError::database)?;
        let now = now_ms();
        Self::operation_event(&mut tx, &operation.id, "manifest_fetched", commit, now).await?;
        if let Some(head) = branch_head {
            Self::record_branch_head_on(&mut tx, &operation.environment_id, head, now).await?;
        }
        Self::pin_secrets_on(&mut tx, operation, application).await?;
        Self::commit_environment_changes(tx, [app_id]).await
    }

    /// Records a fetched repository manifest, under the application's current
    /// name, as its environment's last fetched one, while the environment
    /// still follows a branch. Other environments keep their own.
    ///
    /// While the application is still repository-backed, a manifest an
    /// environment fetched from the branch and commit `spec.manifest` names,
    /// with the application's own connection, also becomes its saved manifest:
    /// what application-wide views show, what environments deploy after
    /// disconnecting, and what declares its environments, to which they are
    /// converged (see `converge_environments_on`). Other branches, one-off
    /// revisions and previews never do. Records `application_applied` when
    /// either manifest changed; neither advances the application revision,
    /// since repository-backed configuration cannot be edited, unless
    /// converging changed the source of an environment with a deployment in
    /// progress, which then leaves it unresolved.
    /// Fails with `secret_name_conflict` when the application's store now holds
    /// a name the fetched manifest declares.
    /// Called in the same transaction that saves the fully prepared runtime target.
    pub(super) async fn accept_deployment_on(
        tx: &mut Transaction<'_, Sqlite>,
        operation: &Operation,
        fetching: &EnvironmentKind,
    ) -> Result<(), StoreError> {
        let row = sqlx::query!(
            "SELECT application_json FROM deployment_inputs WHERE operation_id=?1 AND fetched=1 AND repository_commit IS NOT NULL",
            operation.id
        )
        .fetch_optional(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        let Some(row) = row else {
            return Ok(());
        };
        let fetched: ApplicationTemplate =
            serde_json::from_str(&row.application_json).map_err(StoreError::corrupt)?;
        let stored = Self::application_on(tx, fetched.id().as_str())
            .await?
            .ok_or(StoreError::NotFound)?;
        let application = stored.application;
        // A secret stored since the manifest was fetched may now share a name
        // with one of its declarations.
        Self::check_declared_against_store_on(
            tx,
            application.id().as_str(),
            &fetched.spec().secrets,
        )
        .await?;
        // A retry may accept a manifest fetched before a rename. The
        // deployment keeps the files it captured.
        let fetched = fetched
            .with_name(application.metadata().name.clone())
            .without_files();
        let manifest = fetched.canonical_json().map_err(StoreError::corrupt)?;
        let now = now_ms();
        let environment = operation.environment_id.as_str();
        let mut changed = sqlx::query!(
            "UPDATE environments SET manifest_json=?1,updated_at_ms=?2 WHERE id=?3 AND branch IS NOT NULL AND manifest_json IS NOT ?1",
            manifest,
            now,
            environment
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?
        .rows_affected();
        // Even when this environment's own manifest is unchanged, another
        // environment may have fetched since.
        if let (EnvironmentKind::Environment, Some(connection)) =
            (fetching, application.spec().manifest.clone())
            && Self::fetched_connection_on(tx, operation, &fetched, &connection).await?
        {
            let saved = fetched.with_manifest(Some(connection));
            if Self::converge_environments_on(
                tx,
                application.id(),
                &saved
                    .spec()
                    .declared_environments_since(Some(application.spec()))?,
                (false, Some(&operation.environment_id)),
                now,
            )
            .await?
            {
                Self::bump_generation_on(tx, application.id(), stored.generation, None).await?;
            }
            let saved = saved.canonical_json().map_err(StoreError::corrupt)?;
            let id = application.id().as_str();
            changed += sqlx::query!(
                "UPDATE applications SET desired_json=?1,updated_at_ms=?2 WHERE id=?3 AND desired_json IS NOT ?1",
                saved,
                now,
                id
            )
            .execute(&mut **tx)
            .await
            .map_err(StoreError::database)?
            .rows_affected();
        }
        if changed > 0 {
            Self::operation_event(tx, &operation.id, "application_applied", None, now).await?;
        }
        Ok(())
    }

    /// Whether `fetched`, the manifest `operation` fetched, came from the
    /// revision `connection` names. Its own connection is the revision it was
    /// fetched from; sync pins the head it listed of a branch it follows, so
    /// its deployments of the connection's unpinned branch count too.
    async fn fetched_connection_on(
        tx: &mut Transaction<'_, Sqlite>,
        operation: &Operation,
        fetched: &ApplicationTemplate,
        connection: &piqueld_core::manifest::RepositoryManifest,
    ) -> Result<bool, StoreError> {
        let Some(fetched) = fetched
            .spec()
            .manifest
            .as_ref()
            .map(|fetched| &fetched.repository)
        else {
            return Ok(false);
        };
        let wanted = &connection.repository;
        if fetched == wanted {
            return Ok(true);
        }
        if fetched.url != wanted.url || fetched.branch != wanted.branch || wanted.commit.is_some() {
            return Ok(false);
        }
        Self::requested_by_sync_on(tx, operation).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Actor::Daemon;
    use crate::api::{Mutation, MutationResponse};
    use crate::store::testing::template;
    use piqueld_core::{ApplicationId, EnvironmentId, InstanceId, OperationState, ResolutionSet};

    #[tokio::test]
    async fn fetched_snapshot_preserves_intervening_saved_configuration() {
        let (_directory, store) = Store::temporary().await;
        let initial = template(
            "test",
            "[spec.manifest]\npath='app.toml'\n[spec.manifest.repository]\nurl='https://example.com/app.git'\nbranch='main'",
        );
        let saved = store.save_template(initial, true).await;
        let op = store.operation(&saved.operation_id.unwrap()).await.unwrap();
        store.mark_running(&op.id).await;
        let captured = store.deployment_snapshot(&op.id).await.unwrap().template;
        let mut fetched = captured.to_manifest();
        fetched.spec.manifest = None;
        let fetched = fetched
            .validate_template()
            .unwrap()
            .normalize(captured.id().clone());
        let environment = store.get(&op.environment_id).await.unwrap();
        let rendering = environment.render(&fetched, &op.id).unwrap();
        store
            .save_deployment_input(
                &op,
                &fetched,
                &rendering,
                (Some(&"a".repeat(40)), None),
                &[],
            )
            .await
            .unwrap();
        let mut edited = captured.to_manifest();
        edited.spec.manifest.as_mut().unwrap().path = "fixed.toml".into();
        let edited = edited
            .validate_template()
            .unwrap()
            .normalize(captured.id().clone());
        store
            .accept(
                crate::api::Actor::Daemon,
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
            &rendering.application,
            &op.environment_id,
            InstanceId::parse(store.instance_id()).unwrap(),
            &ResolutionSet::default(),
        )
        .unwrap();
        store.save_prepared(&op, &target).await.unwrap();
        let saved = store.get(&op.environment_id).await.unwrap().application;
        assert_eq!((saved.application, saved.generation), (edited.clone(), 2));
        assert_eq!(store.operation(&op.id).await.unwrap().generation, 1);
        let snapshot = store.deployment_snapshot(&op.id).await.unwrap();
        assert_eq!(snapshot.template, fetched);
        assert_eq!(snapshot.rendering, Some(rendering.clone()));
        assert_eq!(
            store
                .deployments(&op.environment_id, None, 3)
                .await
                .unwrap()
                .items[0]
                .application,
            Some(rendering.application.clone())
        );
        assert!(matches!(
            store
                .save_deployment_input(&op, &edited, &rendering, (None, None), &[])
                .await,
            Err(StoreError::IllegalTransition)
        ));
    }

    /// A repository-backed application `notes` whose saved manifest is only
    /// its connection to `main`.
    fn connection() -> ApplicationTemplate {
        template(
            "notes",
            "[spec.manifest]\npath='app.toml'\n[spec.manifest.repository]\nurl='https://example.com/notes.git'\nbranch='main'",
        )
    }

    /// Deploys `environment` from a fetched manifest with `spec`, as the
    /// controller does, through preparation.
    async fn fetch(store: &Store, environment: &EnvironmentId, spec: &str) {
        let (op, target) = fetched(store, environment, spec).await;
        store.save_prepared(&op, &target).await.unwrap();
    }

    /// Starts a deployment of `environment` and fetches a manifest with
    /// `spec`, returning the operation and the target it prepares.
    async fn fetched(
        store: &Store,
        environment: &EnvironmentId,
        spec: &str,
    ) -> (Operation, piqueld_core::ResolvedApplication) {
        let (MutationResponse::Operation(accepted), _) = store
            .accept(
                crate::api::Actor::Daemon,
                Mutation::deploy(environment.clone()),
                None,
                true,
                None,
            )
            .await
            .unwrap()
        else {
            panic!("operation")
        };
        prepared(store, environment, &accepted.operation_id, spec).await
    }

    /// Runs operation `id` of `environment` through fetching a manifest with
    /// `spec`, returning it and the target it prepares.
    async fn prepared(
        store: &Store,
        environment: &EnvironmentId,
        id: &str,
        spec: &str,
    ) -> (Operation, piqueld_core::ResolvedApplication) {
        let op = store.operation(id).await.unwrap();
        store.mark_running(&op.id).await;
        let current = store.get(environment).await.unwrap();
        // As fetching does, the connection replaces the file's own before validation.
        let mut fetched = piqueld_core::manifest::ApplicationManifest::decode_toml(&format!(
            "api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='notes'\n{spec}"
        ))
        .unwrap();
        fetched.spec.manifest = current.tracking().unwrap().repository();
        let fetched = fetched
            .validate_template()
            .unwrap()
            .normalize(current.application.application.id().clone());
        let rendering = current.render(&fetched, &op.id).unwrap();
        store
            .save_deployment_input(
                &op,
                &fetched,
                &rendering,
                (Some(&"a".repeat(40)), None),
                &[],
            )
            .await
            .unwrap();
        let resolutions = ResolutionSet {
            sources: [(
                piqueld_core::ServiceName::parse("web").unwrap(),
                piqueld_core::ResolvedSource::parse_image(
                    "ghcr.io/example/notes:1.4.0",
                    format!("ghcr.io/example/notes@sha256:{}", "a".repeat(64)),
                )
                .unwrap(),
            )]
            .into(),
            ..ResolutionSet::default()
        };
        let target = piqueld_core::compile_application(
            &rendering.application,
            environment,
            InstanceId::parse(store.instance_id()).unwrap(),
            &resolutions,
        )
        .unwrap();
        (op, target)
    }

    /// A manifest fetched before a rename is accepted under the new name, as
    /// when a deployment is retried.
    #[tokio::test]
    async fn fetched_manifests_keep_the_current_name() {
        let (_directory, store) = Store::temporary().await;
        let saved = store.save_template(connection(), false).await;
        let application = ApplicationId::parse(&saved.application_id).unwrap();
        let production = EnvironmentId::default_for(&application);
        let service = "[[spec.services]]\nname='web'\n[spec.services.source]\ntype='image'\nimage='ghcr.io/example/notes:1.4.0'";
        let (op, target) = fetched(&store, &production, service).await;
        store
            .transition_operation(
                &op.id,
                OperationState::Running,
                OperationState::Failed,
                Some(("build_failed", "build failed")),
            )
            .await
            .unwrap();
        // Repository-backed applications are renamed while disconnected.
        let repository = |repository| Mutation::Edit {
            id: application.clone(),
            edit: Box::new(piqueld_core::edit::ApplicationEdit::Repository(repository)),
            deploy: false,
        };
        for mutation in [
            repository(None),
            Mutation::Rename {
                id: application.clone(),
                name: "journal".into(),
            },
            repository(connection().spec().manifest.clone()),
        ] {
            store.force(mutation).await.unwrap();
        }
        let op = store.retry_operation(&op).await.unwrap();
        store.mark_running(&op.id).await;
        store.save_prepared(&op, &target).await.unwrap();
        let current = store.get(&production).await.unwrap();
        assert_eq!(
            current.application.application.metadata().name.as_str(),
            "journal"
        );
        assert_eq!(
            current.manifest().unwrap().metadata().name.as_str(),
            "journal"
        );
    }

    /// Each environment's hostname reservations and rename checks read its
    /// own last fetched manifest, so a fetch for one never changes the other.
    #[tokio::test]
    async fn environments_reserve_and_rename_by_their_own_fetched_manifest() {
        let (_directory, store) = Store::temporary().await;
        let saved = store.save_template(connection(), false).await;
        let application = ApplicationId::parse(&saved.application_id).unwrap();
        let production = EnvironmentId::default_for(&application);
        let (MutationResponse::Environment(staging), _) = store
            .accept(
                crate::api::Actor::Daemon,
                Mutation::CreateEnvironment {
                    application,
                    name: piqueld_core::EnvironmentName::parse("staging").unwrap(),
                    source: Some(crate::api::SourceChoice::Branch(
                        piqueld_core::TrackedBranch::new("release".into(), None).unwrap(),
                    )),
                },
                None,
                true,
                None,
            )
            .await
            .unwrap()
        else {
            panic!("environment")
        };
        let reserved = async |environment: &EnvironmentId| {
            let id = environment.as_str();
            sqlx::query_scalar!(
                "SELECT hostname FROM hostname_reservations WHERE environment_id=?1 ORDER BY hostname",
                id
            )
            .fetch_all(&store.pool)
            .await
            .unwrap()
        };
        let service = "[[spec.services]]\nname='web'\n[spec.services.source]\ntype='image'\nimage='ghcr.io/example/notes:1.4.0'";
        fetch(
            &store,
            &production,
            &format!("{service}\n[[spec.routes]]\nhostname='notes.example.com'\nservice='web'\nport=80\n[spec.environments.production.variables]\nlevel='debug'"),
        )
        .await;
        // Staging has fetched nothing yet, so it reserves nothing.
        assert_eq!(reserved(&production).await, ["notes.example.com"]);
        assert_eq!(reserved(&staging.id).await, Vec::<String>::new());
        let before = store.get(&production).await.unwrap().own;
        fetch(
            &store,
            &staging.id,
            &format!(
                "{service}\n[[spec.routes]]\nhostname='staging.example.com'\nservice='web'\nport=80"
            ),
        )
        .await;
        assert_eq!(reserved(&production).await, ["notes.example.com"]);
        assert_eq!(reserved(&staging.id).await, ["staging.example.com"]);
        assert_eq!(store.get(&production).await.unwrap().own, before);
        // Only production's own manifest configures `production`.
        let rename = |id: &EnvironmentId| Mutation::RenameEnvironment {
            id: id.clone(),
            name: piqueld_core::EnvironmentName::parse("qa").unwrap(),
        };
        assert!(matches!(
            store.force(rename(&production)).await,
            Err(StoreError::EnvironmentConfigured { .. })
        ));
        store.force(rename(&staging.id)).await.unwrap();
    }

    /// Saves the repository-backed application `notes` (see `connection`).
    async fn saved_connection(store: &Store) -> ApplicationId {
        let saved = store.save_template(connection(), false).await;
        ApplicationId::parse(&saved.application_id).unwrap()
    }

    /// A manifest with the image service `web` that declares the generated
    /// secret `token` when `declared`.
    fn token_spec(declared: bool) -> String {
        let secret = "[[spec.secrets]]\nname='token'\n[spec.secrets.generate]\ntype='random'\nbytes=16\nencoding='hex'\n";
        format!(
            "{}[[spec.services]]\nname='web'\n[spec.services.source]\ntype='image'\nimage='ghcr.io/example/notes:1.4.0'",
            if declared { secret } else { "" }
        )
    }

    /// Fetching a manifest from the branch `spec.manifest` names converges
    /// the environments it declares, including the fetching one, whose own
    /// deployment is past building; other branches never do.
    #[tokio::test]
    async fn fetches_of_the_connection_branch_converge_declared_environments() {
        use piqueld_core::{EnvironmentSource, TrackedBranch};
        let (_directory, store) = Store::temporary().await;
        let polled = template(
            "notes",
            "[spec.manifest]\npath='app.toml'\nsync={mode='poll'}\n[spec.manifest.repository]\nurl='https://example.com/notes.git'\nbranch='main'",
        );
        let saved = store.save_template(polled, false).await;
        let application = ApplicationId::parse(&saved.application_id).unwrap();
        let production = EnvironmentId::default_for(&application);
        let declared = format!(
            "[spec.environments.staging]\nsync=true\n[spec.environments.production]\npromote_from='staging'\n[spec.environments.canary]\nbranch='next'\n{}",
            token_spec(false)
        );
        fetch(&store, &production, &declared).await;
        let environments = store.environments(&application).await.unwrap();
        let named = |name: &str| {
            environments
                .iter()
                .find(|environment| environment.name.as_str() == name)
                .unwrap()
                .clone()
        };
        let (staging, canary) = (named("staging"), named("canary"));
        assert_eq!(
            staging.source,
            EnvironmentSource::Branch(TrackedBranch::new("main".into(), None).unwrap())
        );
        assert!(staging.sync);
        assert_eq!(
            canary.source,
            EnvironmentSource::Branch(TrackedBranch::new("next".into(), None).unwrap())
        );
        assert_eq!(
            named("production").source.promoted_from(),
            Some(&staging.id)
        );

        // A manifest on another branch declares nothing.
        fetch(&store, &canary.id, &token_spec(false)).await;
        assert_eq!(store.environments(&application).await.unwrap().len(), 3);
        // Sync pins the head it listed, and still fetches `main`.
        let id = staging.id.as_str();
        let first = "a".repeat(40);
        sqlx::query!(
            "UPDATE environments SET synced_commit=?1,synced_at_ms=1 WHERE id=?2",
            first,
            id
        )
        .execute(&store.pool)
        .await
        .unwrap();
        let (MutationResponse::Synced(crate::api::SyncOutcome::Deployed(synced)), _) = store
            .accept(
                crate::api::Actor::System(piqueld_core::sync::SystemActor::SyncPoll),
                Mutation::Sync(crate::api::ListedHead {
                    id: staging.id.clone(),
                    repository: "https://example.com/notes.git".into(),
                    branch: piqueld_core::GitBranch::parse("main").unwrap(),
                    head: "b".repeat(40),
                    since: store.get(&staging.id).await.unwrap().environment.synced,
                }),
                None,
                false,
                None,
            )
            .await
            .unwrap()
        else {
            panic!("sync deployment expected")
        };
        let (op, target) = prepared(
            &store,
            &staging.id,
            &synced.operation_id,
            &format!("{declared}\n[spec.environments.qa]\n"),
        )
        .await;
        store.save_prepared(&op, &target).await.unwrap();
        assert_eq!(store.environments(&application).await.unwrap().len(), 4);
        // The next one from `main` stops staging syncing, and leaves canary
        // and qa, no longer declared, as they are.
        fetch(
            &store,
            &staging.id,
            &format!(
                "[spec.environments.staging]\n[spec.environments.production]\npromote_from='staging'\n{}",
                token_spec(false)
            ),
        )
        .await;
        let environments = store.environments(&application).await.unwrap();
        assert_eq!(environments.len(), 4);
        assert!(!environments.iter().any(|environment| environment.sync));
    }

    /// A fetch that changes an environment's source leaves it unresolved, and
    /// advances the revision when a deployment of it in progress would
    /// resolve it again, as changing it with `env branch` does.
    #[tokio::test]
    async fn fetched_source_changes_leave_environments_unresolved() {
        let (_directory, store) = Store::temporary().await;
        let application = saved_connection(&store).await;
        let production = EnvironmentId::default_for(&application);
        let canary = |branch: &str| {
            format!(
                "[spec.environments.canary]\nbranch='{branch}'\n{}",
                token_spec(false)
            )
        };
        fetch(&store, &production, &canary("next")).await;
        let id = store
            .environments(&application)
            .await
            .unwrap()
            .into_iter()
            .find(|environment| environment.name.as_str() == "canary")
            .unwrap()
            .id;
        let canary_id = id.as_str();
        sqlx::query!(
            "UPDATE environments SET resolved_generation=(SELECT generation FROM applications) WHERE id=?1",
            canary_id
        )
        .execute(&store.pool)
        .await
        .unwrap();
        let generation = store.get(&id).await.unwrap().application.generation;
        fetch(&store, &production, &canary("release")).await;
        let current = store.get(&id).await.unwrap();
        assert_eq!(current.environment.resolved_generation, None);
        assert_eq!(current.application.generation, generation);

        store
            .accept(Daemon, Mutation::deploy(id.clone()), None, false, None)
            .await
            .unwrap();
        fetch(&store, &production, &canary("beta")).await;
        assert_eq!(
            store.get(&id).await.unwrap().application.generation,
            generation + 1
        );
    }

    /// The saved manifest's declarations hold their names even for an
    /// environment whose own fetched manifest, from another branch, has no
    /// block: renaming it would leave the declaration to recreate it.
    #[tokio::test]
    async fn declared_environments_keep_their_names() {
        let (_directory, store) = Store::temporary().await;
        let application = saved_connection(&store).await;
        let production = EnvironmentId::default_for(&application);
        fetch(
            &store,
            &production,
            &format!(
                "[spec.environments.staging]\nbranch='release'\n{}",
                token_spec(false)
            ),
        )
        .await;
        let staging = store
            .environments(&application)
            .await
            .unwrap()
            .into_iter()
            .find(|environment| environment.name.as_str() == "staging")
            .unwrap()
            .id;
        fetch(&store, &staging, &token_spec(false)).await;
        let rename = Mutation::RenameEnvironment {
            id: staging,
            name: piqueld_core::EnvironmentName::parse("qa").unwrap(),
        };
        assert!(matches!(
            store.force(rename).await,
            Err(StoreError::EnvironmentDeclared { environment }) if environment.as_str() == "staging"
        ));
    }

    /// A secret stored after a manifest was fetched but before it is accepted
    /// may not share a name with one of its declarations.
    #[tokio::test]
    async fn accepting_a_fetched_manifest_rejects_declarations_stored_meanwhile() {
        let (_directory, store) = Store::temporary().await;
        let application = saved_connection(&store).await;
        let production = EnvironmentId::default_for(&application);
        let (op, target) = fetched(&store, &production, &token_spec(true)).await;
        // The saved manifest does not declare `token` yet, so storing it succeeds.
        store
            .put_stored_secret(Daemon, &application, "token", 0, b"value".to_vec(), None)
            .await
            .unwrap();
        let Err(StoreError::Validation(errors)) = store.save_prepared(&op, &target).await else {
            panic!("secret name conflict expected")
        };
        assert_eq!(errors.0[0].code, piqueld_core::codes::SECRET_NAME_CONFLICT);
        assert!(store.get(&production).await.unwrap().manifest().is_none());
    }

    /// A name an environment's last fetched manifest declares cannot be
    /// stored, even once another environment's fetch replaced the saved one.
    #[tokio::test]
    async fn fetched_declarations_of_every_environment_reserve_their_names() {
        let (_directory, store) = Store::temporary().await;
        let application = saved_connection(&store).await;
        let (MutationResponse::Environment(staging), _) = store
            .accept(
                Daemon,
                Mutation::CreateEnvironment {
                    application: application.clone(),
                    name: piqueld_core::EnvironmentName::parse("staging").unwrap(),
                    source: Some(crate::api::SourceChoice::Branch(
                        piqueld_core::TrackedBranch::new("release".into(), None).unwrap(),
                    )),
                },
                None,
                true,
                None,
            )
            .await
            .unwrap()
        else {
            panic!("environment")
        };
        let production = EnvironmentId::default_for(&application);
        fetch(&store, &production, &token_spec(true)).await;
        fetch(&store, &staging.id, &token_spec(false)).await;
        let Err(StoreError::Validation(errors)) = store
            .put_stored_secret(Daemon, &application, "token", 0, b"value".to_vec(), None)
            .await
        else {
            panic!("secret name conflict expected")
        };
        assert_eq!(errors.0[0].code, piqueld_core::codes::SECRET_NAME_CONFLICT);
    }
}
