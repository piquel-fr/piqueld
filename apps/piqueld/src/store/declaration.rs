//! Environments the manifest manages (see `piqueld_core::manifest::declaration`):
//! converging an application's environments to the ones its manifest
//! declares, and refusing imperative changes to declared ones.
use super::{Store, StoreError};
use piqueld_core::manifest::{
    ApplicationSpec, ApplicationTemplate, Declaration, DeclaredSource, EnvironmentChange,
};
use piqueld_core::{
    ApplicationId, EnvironmentId, EnvironmentName, EnvironmentSource, PromotedFrom,
    api::EnvironmentView,
};
use sqlx::{Sqlite, Transaction};
use std::collections::BTreeMap;

impl Store {
    /// Refuses an imperative change to environment `name` while `application`'s
    /// saved manifest declares it: creating, deleting, or changing its source
    /// or sync belongs to that `[spec.environments.<name>]` block.
    pub(super) fn check_undeclared(
        application: &ApplicationTemplate,
        name: &EnvironmentName,
    ) -> Result<(), StoreError> {
        if application.spec().environments.contains_key(name.as_str()) {
            return Err(StoreError::EnvironmentDeclared {
                environment: name.clone(),
            });
        }
        Ok(())
    }

    /// Converges `application`'s environments to `declarations` (see
    /// [`EnvironmentChange::between`]): creates missing ones, not deployed,
    /// and changes the source and sync of the others, deploying nothing.
    /// Environments it no longer declares are left alone. A change that
    /// can't apply yet waits for the next convergence: making an environment
    /// promoted while a deployment of it is in progress (except `accepting`'s,
    /// past fetching and building), and creating one whose name or promotion
    /// source is still held by an environment being deleted. When `creating`
    /// the application, its `production` shares its ID, as the one created
    /// for applications that declare none does. Never advances the
    /// application revision; saving the manifest does.
    pub(super) async fn converge_environments_on(
        tx: &mut Transaction<'_, Sqlite>,
        application: &ApplicationId,
        declarations: &[Declaration],
        (creating, accepting): (bool, Option<&EnvironmentId>),
        now: i64,
    ) -> Result<(), StoreError> {
        let environments = Self::environments_on(tx, application.as_str()).await?;
        // Live environments by name, including those created here, which
        // later declarations may promote from.
        let mut live = environments
            .iter()
            .filter(|environment| !environment.delete_intent)
            .map(|environment| (environment.name.clone(), environment.id.clone()))
            .collect::<BTreeMap<_, _>>();
        for change in EnvironmentChange::between(declarations, &environments) {
            match change {
                EnvironmentChange::Create {
                    environment,
                    source,
                    sync,
                } => {
                    let Some(source) = Self::declared_source(&live, &source) else {
                        continue;
                    };
                    if Self::named(&environments, &environment).is_some() {
                        continue;
                    }
                    let id = if creating && environment == EnvironmentName::default_name() {
                        EnvironmentId::default_for(application)
                    } else {
                        EnvironmentId::parse(super::new_id("env")).map_err(StoreError::corrupt)?
                    };
                    Self::insert_environment_on(
                        tx,
                        application,
                        (&id, &environment),
                        &source,
                        sync,
                        now,
                    )
                    .await?;
                    live.insert(environment, id);
                }
                EnvironmentChange::Source {
                    environment, to, ..
                } => {
                    let Some(source) = Self::declared_source(&live, &to) else {
                        continue;
                    };
                    let current = Self::live_on(tx, &live, &environment).await?;
                    if let Some(from) = source.promoted_from() {
                        // Declarations are acyclic, so a cycle only goes through
                        // a change still waiting: this one waits too.
                        match Self::check_promotion_source_on(
                            tx,
                            application,
                            (current.id(), &environment),
                            from,
                        )
                        .await
                        {
                            Err(StoreError::Promotion(super::PromotionError::Cycle { .. })) => {
                                continue;
                            }
                            checked => checked?,
                        }
                    }
                    let accepting = accepting == Some(current.id());
                    match Self::write_source_on(tx, &current, source, accepting, now).await {
                        Err(StoreError::Busy) => {}
                        changed => {
                            changed?;
                        }
                    }
                }
                EnvironmentChange::Sync { environment, sync } => {
                    let current = Self::live_on(tx, &live, &environment).await?;
                    Self::write_sync_on(tx, &current.environment, sync, now).await?;
                }
                EnvironmentChange::Undeclared { .. } => {}
            }
        }
        Ok(())
    }

    /// The source `declared` names, with its promotion source among `live`,
    /// or `None` while that is missing.
    fn declared_source(
        live: &BTreeMap<EnvironmentName, EnvironmentId>,
        declared: &DeclaredSource,
    ) -> Option<EnvironmentSource> {
        Some(match declared {
            DeclaredSource::Saved => EnvironmentSource::Saved,
            DeclaredSource::Branch(branch) => EnvironmentSource::Branch(branch.clone()),
            DeclaredSource::Promoted { from } => EnvironmentSource::Promoted(PromotedFrom {
                environment: live.get(from)?.clone(),
            }),
        })
    }

    /// The environment named `name` among `environments`, if any.
    fn named<'a>(
        environments: &'a [EnvironmentView],
        name: &EnvironmentName,
    ) -> Option<&'a EnvironmentView> {
        environments
            .iter()
            .find(|environment| environment.name == *name)
    }

    /// Reads the live environment named `name`.
    async fn live_on(
        tx: &mut Transaction<'_, Sqlite>,
        live: &BTreeMap<EnvironmentName, EnvironmentId>,
        name: &EnvironmentName,
    ) -> Result<super::StoredEnvironment, StoreError> {
        let id = live.get(name).ok_or(StoreError::NotFound)?;
        Self::environment_on(tx, id.as_str())
            .await?
            .ok_or(StoreError::NotFound)
    }

    /// Blocks declare environments, so an edit through the CLI or dashboard
    /// that adds one for an existing environment declares that environment
    /// as it is, and one that declares the application's first environment
    /// declares every existing one as it is: editing configuration never
    /// changes where environments deploy from, or which exist. `before` is
    /// the saved manifest, `after` the edited one.
    pub(super) async fn declare_as_they_are_on(
        tx: &mut Transaction<'_, Sqlite>,
        application: &ApplicationId,
        before: &ApplicationSpec,
        after: &mut ApplicationSpec,
    ) -> Result<(), StoreError> {
        let adds = |name: &str, after: &ApplicationSpec| {
            !before.environments.contains_key(name) && after.environments.contains_key(name)
        };
        if after.environments.keys().all(|name| !adds(name, after)) {
            return Ok(());
        }
        let first = before.environments.is_empty();
        let environments = Self::environments_on(tx, application.as_str()).await?;
        for environment in environments
            .iter()
            .filter(|environment| !environment.delete_intent)
        {
            let name = environment.name.as_str();
            if !(adds(name, after) || first) {
                continue;
            }
            let config = after.environments.entry(name.to_owned()).or_default();
            match &environment.source {
                EnvironmentSource::Saved => {}
                EnvironmentSource::Branch(branch) => {
                    config.branch = Some(branch.branch().to_owned());
                    config.commit = branch.commit().map(str::to_owned);
                    config.sync = environment.sync && branch.commit().is_none();
                }
                EnvironmentSource::Promoted(from) => {
                    config.promote_from = environments
                        .iter()
                        .find(|source| source.id == from.environment)
                        .map(|source| source.name.to_string());
                }
            }
        }
        Ok(())
    }
}
