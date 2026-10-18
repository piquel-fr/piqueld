//! Environments the manifest manages (see `piqueld_core::manifest::declaration`):
//! converging an application's environments to the ones its manifest
//! declares, and refusing imperative changes to declared ones.
use crate::store::{Store, StoreError, environment::SourceWrite};
use piqueld_core::manifest::{
    ApplicationSpec, ApplicationTemplate, Declaration, DeclaredSource, EnvironmentChange,
    EnvironmentConfig,
};
use piqueld_core::{
    ApplicationId, EnvironmentId, EnvironmentName, EnvironmentSource, PromotedFrom,
    api::EnvironmentView,
};
use sqlx::{Sqlite, Transaction};
use std::collections::BTreeMap;

impl Store {
    /// Refuses to create or delete environment `name` while `application`'s
    /// saved manifest declares it: its `[spec.environments.<name>]` block
    /// decides whether it exists.
    pub(in crate::store) fn check_undeclared(
        application: &ApplicationTemplate,
        name: &EnvironmentName,
    ) -> Result<(), StoreError> {
        Self::check_declared_by(application, name, |_| true)
    }

    /// Refuses to change the source or sync of environment `name` while its
    /// block in `application`'s saved manifest sets them (see
    /// `EnvironmentConfig::sets_source`).
    pub(in crate::store) fn check_source_undeclared(
        application: &ApplicationTemplate,
        name: &EnvironmentName,
    ) -> Result<(), StoreError> {
        Self::check_declared_by(application, name, EnvironmentConfig::sets_source)
    }

    /// `EnvironmentDeclared` when `name`'s block in `application`'s saved
    /// manifest is `declares`.
    fn check_declared_by(
        application: &ApplicationTemplate,
        name: &EnvironmentName,
        declares: impl Fn(&EnvironmentConfig) -> bool,
    ) -> Result<(), StoreError> {
        if application
            .spec()
            .environments
            .get(name.as_str())
            .is_some_and(declares)
        {
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
    /// application revision; returns whether it changed the source of an
    /// environment with a deployment in progress, for which the caller must
    /// (see `write_source_on`). Saving the manifest always does.
    pub(in crate::store) async fn converge_environments_on(
        tx: &mut Transaction<'_, Sqlite>,
        application: &ApplicationId,
        declarations: &[Declaration],
        (creating, accepting): (bool, Option<&EnvironmentId>),
        now: i64,
    ) -> Result<bool, StoreError> {
        let mut deploying = false;
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
                        EnvironmentId::parse(crate::store::new_id("env"))
                            .map_err(StoreError::corrupt)?
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
                            Err(StoreError::Promotion(crate::store::PromotionError::Cycle {
                                ..
                            })) => {
                                continue;
                            }
                            checked => checked?,
                        }
                    }
                    let accepting = accepting == Some(current.id());
                    match Self::write_source_on(tx, &current, source, accepting, now).await {
                        Err(StoreError::Busy) => {}
                        written => {
                            deploying |= written? == SourceWrite::Changed { deploying: true };
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
        Ok(deploying)
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
    ) -> Result<crate::store::StoredEnvironment, StoreError> {
        let id = live.get(name).ok_or(StoreError::NotFound)?;
        Self::environment_on(tx, id.as_str())
            .await?
            .ok_or(StoreError::NotFound)
    }

    /// Blocks declare environments, so an edit through the CLI or dashboard
    /// configures existing environments only: one adding a block for a name no
    /// environment has fails with `EnvironmentUnknown`, rather than creating
    /// an environment no edit could remove. An edit that declares the
    /// application's first environment declares every existing one too, with
    /// a block that leaves its source as it is: editing configuration never
    /// changes which environments the manifest reports, or where they deploy
    /// from. `before` is the saved manifest, `after` the edited one.
    pub(in crate::store) async fn declare_edited_on(
        tx: &mut Transaction<'_, Sqlite>,
        application: &ApplicationId,
        before: &ApplicationSpec,
        after: &mut ApplicationSpec,
    ) -> Result<(), StoreError> {
        let live = Self::environments_on(tx, application.as_str())
            .await?
            .into_iter()
            .filter(|environment| !environment.delete_intent)
            .map(|environment| environment.name.to_string())
            .collect::<Vec<_>>();
        if let Some(environment) = after
            .environments
            .keys()
            .find(|name| !before.environments.contains_key(*name) && !live.contains(name))
        {
            return Err(StoreError::EnvironmentUnknown {
                environment: environment.clone(),
            });
        }
        if before.environments.is_empty() && !after.environments.is_empty() {
            for name in live {
                after.environments.entry(name).or_default();
            }
        }
        Ok(())
    }
}
