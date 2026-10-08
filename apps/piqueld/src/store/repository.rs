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
    api::DiagnosticView,
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
}

impl Store {
    /// Captures the configuration a Deploy started from, still unfetched.
    pub(crate) async fn insert_deployment_on(
        tx: &mut Transaction<'_, Sqlite>,
        operation: &Operation,
        application: &ApplicationTemplate,
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
                template: serde_json::from_str(&row.application_json)
                    .map_err(StoreError::corrupt)?,
                fetched: row.fetched != 0,
                commit: row.repository_commit,
            })
        })
        .transpose()
    }

    /// Stores the fetched manifest and commit for the latest running operation,
    /// copying it, its rendering, and `warnings` into deployment history,
    /// pinning its secret versions, and re-checking hostname reservations.
    /// Fetching happens once: a second save, or one for superseded work, fails
    /// with `StoreError::IllegalTransition`.
    pub(crate) async fn save_deployment_input(
        &self,
        operation: &Operation,
        template: &ApplicationTemplate,
        rendering: &Rendering,
        commit: Option<&str>,
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
        Self::operation_event(&mut tx, &operation.id, "manifest_fetched", commit, now_ms()).await?;
        Self::pin_secrets_on(&mut tx, operation, application).await?;
        Self::commit_environment_changes(tx, [app_id]).await
    }

    /// Records a fetched repository manifest, under the application's current
    /// name, as its environment's last fetched one, while the environment
    /// still follows a branch. Other environments keep their own.
    ///
    /// While the application is still repository-backed, a manifest an
    /// environment fetched, with the application's own connection, also
    /// becomes its saved manifest: what application-wide views show and what
    /// environments deploy after disconnecting. A preview's branch never does. Records `application_applied` when either changed;
    /// neither advances the application revision, since repository-backed
    /// configuration cannot be edited.
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
        let application = Self::application_on(tx, fetched.id().as_str())
            .await?
            .ok_or(StoreError::NotFound)?
            .application;
        // A secret stored since the manifest was fetched may now share a name
        // with one of its declarations.
        Self::check_declared_against_store_on(
            tx,
            application.id().as_str(),
            &fetched.spec().secrets,
        )
        .await?;
        // A retry may accept a manifest fetched before a rename.
        let fetched = fetched.with_name(application.metadata().name.clone());
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
        {
            let saved = fetched
                .with_manifest(Some(connection))
                .canonical_json()
                .map_err(StoreError::corrupt)?;
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::Actor::Daemon;
    use crate::api::{Mutation, MutationResponse};
    use piqueld_core::{ApplicationId, EnvironmentId, InstanceId, OperationState, ResolutionSet};

    #[tokio::test]
    async fn fetched_snapshot_preserves_intervening_saved_configuration() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("db")).await.unwrap();
        let initial = piqueld_core::manifest::parse_template_toml("api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='test'\n[spec.manifest]\npath='app.toml'\n[spec.manifest.repository]\nurl='https://example.com/app.git'\nbranch='main'").unwrap().normalize(ApplicationId::parse("test-app").unwrap());
        let (MutationResponse::Saved(saved), _) = store
            .accept(
                crate::api::Actor::Daemon,
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
            .save_deployment_input(&op, &fetched, &rendering, Some(&"a".repeat(40)), &[])
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
                .save_deployment_input(&op, &edited, &rendering, None, &[])
                .await,
            Err(StoreError::IllegalTransition)
        ));
    }

    /// A repository-backed application `notes` whose saved manifest is only
    /// its connection to `main`.
    fn connection() -> ApplicationTemplate {
        piqueld_core::manifest::parse_template_toml("api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='notes'\n[spec.manifest]\npath='app.toml'\n[spec.manifest.repository]\nurl='https://example.com/notes.git'\nbranch='main'").unwrap().normalize(ApplicationId::parse("pending-application").unwrap())
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
        let op = store.operation(&accepted.operation_id).await.unwrap();
        store
            .transition_operation(
                &op.id,
                OperationState::Requested,
                OperationState::Running,
                None,
            )
            .await
            .unwrap();
        let current = store.get(environment).await.unwrap();
        let fetched = piqueld_core::manifest::parse_template_toml(&format!(
            "api_version='piqueld.dev/v1alpha1'\nkind='Application'\n[metadata]\nname='notes'\n{spec}"
        ))
        .unwrap()
        .normalize(current.application.application.id().clone())
        .with_manifest(current.repository());
        let rendering = current.render(&fetched, &op.id).unwrap();
        store
            .save_deployment_input(&op, &fetched, &rendering, Some(&"a".repeat(40)), &[])
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
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("db")).await.unwrap();
        let (MutationResponse::Saved(saved), _) = store
            .accept(
                crate::api::Actor::Daemon,
                Mutation::Save {
                    application: Box::new(connection()),
                    expected_application_id: None,
                    deploy: false,
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
            store
                .accept(crate::api::Actor::Daemon, mutation, None, true, None)
                .await
                .unwrap();
        }
        let op = store.retry_operation(&op).await.unwrap();
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
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("db")).await.unwrap();
        let (MutationResponse::Saved(saved), _) = store
            .accept(
                crate::api::Actor::Daemon,
                Mutation::Save {
                    application: Box::new(connection()),
                    expected_application_id: None,
                    deploy: false,
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
        let application = ApplicationId::parse(&saved.application_id).unwrap();
        let production = EnvironmentId::default_for(&application);
        let (MutationResponse::Environment(staging), _) = store
            .accept(
                crate::api::Actor::Daemon,
                Mutation::CreateEnvironment {
                    application,
                    name: piqueld_core::EnvironmentName::parse("staging").unwrap(),
                    branch: Some(piqueld_core::TrackedBranch::new("release".into(), None).unwrap()),
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
            &format!("{service}\n[[spec.routes]]\nhostname='notes.example.com'\nservice='web'\nport=80\n[spec.environments.qa.variables]\nlevel='debug'"),
        )
        .await;
        // Staging has fetched nothing yet, so it reserves nothing.
        assert_eq!(reserved(&production).await, ["notes.example.com"]);
        assert_eq!(reserved(&staging.id).await, Vec::<String>::new());
        let before = store.get(&production).await.unwrap().fetched;
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
        assert_eq!(store.get(&production).await.unwrap().fetched, before);
        // Only production's own manifest configures `qa`.
        let rename = |id: &EnvironmentId| Mutation::RenameEnvironment {
            id: id.clone(),
            name: piqueld_core::EnvironmentName::parse("qa").unwrap(),
        };
        assert!(matches!(
            store
                .accept(
                    crate::api::Actor::Daemon,
                    rename(&production),
                    None,
                    true,
                    None
                )
                .await,
            Err(StoreError::EnvironmentConfigured { .. })
        ));
        store
            .accept(
                crate::api::Actor::Daemon,
                rename(&staging.id),
                None,
                true,
                None,
            )
            .await
            .unwrap();
    }

    /// Saves the repository-backed application `notes` (see `connection`).
    async fn saved_connection(store: &Store) -> ApplicationId {
        let (MutationResponse::Saved(saved), _) = store
            .accept(
                Daemon,
                Mutation::Save {
                    application: Box::new(connection()),
                    expected_application_id: None,
                    deploy: false,
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

    /// A secret stored after a manifest was fetched but before it is accepted
    /// may not share a name with one of its declarations.
    #[tokio::test]
    async fn accepting_a_fetched_manifest_rejects_declarations_stored_meanwhile() {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("db")).await.unwrap();
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
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(directory.path().join("db")).await.unwrap();
        let application = saved_connection(&store).await;
        let (MutationResponse::Environment(staging), _) = store
            .accept(
                Daemon,
                Mutation::CreateEnvironment {
                    application: application.clone(),
                    name: piqueld_core::EnvironmentName::parse("staging").unwrap(),
                    branch: Some(piqueld_core::TrackedBranch::new("release".into(), None).unwrap()),
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
