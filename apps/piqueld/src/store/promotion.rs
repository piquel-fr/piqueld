//! Promotions: deploying a release into a promoted environment, which never
//! builds or fetches. The release is pinned when the promotion is accepted,
//! rendered for the environment from its own manifest, and compiled with its
//! own images, so the prepared target is saved in the accepting transaction.
use super::{
    EnvironmentId, Operation, OperationKind, Store, StoreError, StoredEnvironment, now_ms,
};
use piqueld_core::{
    EnvironmentKind, EnvironmentName, EnvironmentSource, InstanceId, NormalizedApplication,
    Release, ReleaseId, ValidationErrors,
    api::{DeploymentOrigin, EnvironmentView, PromotionSelection, SecretProblem},
    compile_release,
    manifest::SecretSource,
};
use sqlx::{Sqlite, SqliteConnection, Transaction};
use std::collections::BTreeSet;

/// Why a promotion, or making an environment promoted, is refused.
#[derive(Debug, thiserror::Error)]
pub enum PromotionError {
    /// A promoted environment never builds or fetches.
    #[error(
        "environment {environment} receives releases promoted from another environment and never builds; deploy it with `piquelctl env promote`"
    )]
    Promoted {
        /// The promoted environment.
        environment: EnvironmentName,
    },
    /// Only promoted environments receive promoted releases.
    #[error(
        "environment {environment} builds from its own source; make it promoted with `piquelctl env source --promote-from` first"
    )]
    NotPromoted {
        /// The tracking environment.
        environment: EnvironmentName,
    },
    /// The environment would, directly or through others, promote from itself.
    #[error("promotion cycle: {}", environments.iter().map(EnvironmentName::as_str).collect::<Vec<_>>().join(" ← "))]
    Cycle {
        /// The chain of sources, from the environment back to itself.
        environments: Vec<EnvironmentName>,
    },
    /// Sources are live environments of the same application, never previews.
    #[error("promotion source {environment}: {reason}")]
    SourceInvalid {
        /// The requested source ID.
        environment: EnvironmentId,
        /// Why it can't be one.
        reason: &'static str,
    },
    /// Deleting a source would leave promoted environments without one.
    #[error(
        "{} promote from this environment; make them promote from another environment, or track their own source, first",
        super::environment_list(environments)
    )]
    InUse {
        /// The live environments that promote from it, in name order.
        environments: Vec<EnvironmentName>,
    },
    /// The source has nothing that may be promoted right now.
    #[error("environment {environment} can't be promoted from: {reason}")]
    SourceNotReady {
        /// The source environment.
        environment: EnvironmentName,
        /// What it lacks.
        reason: &'static str,
    },
    /// The source's current deployment is not the one requested.
    #[error(
        "deployment {deployment} is no longer the current deployment of environment {environment}; review the new one and promote again"
    )]
    SourceChanged {
        /// The source environment.
        environment: EnvironmentName,
        /// The deployment that was requested or checked.
        deployment: String,
    },
    /// Secrets the rendered release mounts are missing or not allowed.
    #[error(
        "environment {environment} can't use every secret the release mounts: {}",
        secret_list(secrets)
    )]
    Secrets {
        /// The environment being promoted into.
        environment: EnvironmentName,
        /// Every problem, by secret name.
        secrets: Vec<SecretProblem>,
    },
}

/// Lists secret problems in an error message.
fn secret_list(problems: &[SecretProblem]) -> String {
    problems
        .iter()
        .map(|problem| match problem {
            SecretProblem::Missing { secret } => format!("{secret} (missing)"),
            SecretProblem::AccessDenied { secret } => format!("{secret} (access denied)"),
            SecretProblem::Unavailable { secret } => format!("{secret} (value discarded)"),
            SecretProblem::Deleting { secret } => format!("{secret} (being deleted)"),
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// A release pinned for a promotion before it is accepted, and where it
/// came from.
#[derive(Clone, Debug)]
pub struct Promotion {
    /// The release it deploys.
    pub release: ReleaseId,
    /// Where the release came from: never `build`.
    pub origin: DeploymentOrigin,
    /// Values generated for the declared secrets it mounts that the
    /// environment lacks, stored only if it is accepted.
    pub(crate) generated: Vec<super::secret::GeneratedSecret>,
}

/// What promoting a selection into an environment would deploy, read before
/// acceptance so the caller can check the source's health and the release's
/// images outside the database.
pub struct PromotionCandidate {
    /// The environment receiving the release.
    pub target: StoredEnvironment,
    /// The source environment, when promoting its current deployment.
    pub source: Option<StoredEnvironment>,
    /// The pinned release and its origin.
    pub promotion: Promotion,
    /// The release's content.
    pub release: Release,
}

impl Store {
    /// Checks that `environment` may receive releases from `source`: another
    /// live environment of its application, never a preview, whose own
    /// sources never lead back to it. Chains are allowed.
    pub(super) async fn check_promotion_source_on(
        connection: &mut SqliteConnection,
        environment: &StoredEnvironment,
        source: &EnvironmentId,
    ) -> Result<(), StoreError> {
        let invalid = |reason| PromotionError::SourceInvalid {
            environment: source.clone(),
            reason,
        };
        let mut chain = vec![environment.environment.name.clone()];
        let mut visited = BTreeSet::new();
        let mut next = Some(source.clone());
        while let Some(id) = next {
            if id == *environment.id() {
                chain.push(environment.environment.name.clone());
                return Err(PromotionError::Cycle {
                    environments: chain,
                }
                .into());
            }
            if !visited.insert(id.clone()) {
                // Sources are checked on every change, so chains never loop.
                return Err(StoreError::Corrupt);
            }
            let current = Self::environment_on(connection, id.as_str())
                .await?
                .filter(|current| {
                    current.environment.application_id == environment.environment.application_id
                })
                .ok_or_else(|| invalid("no such environment in this application"))?;
            if id == *source {
                if let EnvironmentKind::Preview(_) = current.environment.kind {
                    return Err(invalid("previews are never promotion sources").into());
                }
                if current.delete_intent() {
                    return Err(invalid("it is being deleted").into());
                }
            }
            chain.push(current.environment.name.clone());
            next = current.environment.source.promoted_from().cloned();
        }
        Ok(())
    }

    /// Refuses to delete `environment` while live environments promote from it.
    pub(super) async fn check_promotion_dependents_on(
        connection: &mut SqliteConnection,
        environment: &EnvironmentId,
    ) -> Result<(), StoreError> {
        let id = environment.as_str();
        let dependents = sqlx::query_scalar!(
            "SELECT name FROM environments WHERE promoted_from=?1 AND delete_intent=0 ORDER BY name",
            id
        )
        .fetch_all(connection)
        .await
        .map_err(StoreError::database)?;
        if dependents.is_empty() {
            return Ok(());
        }
        Err(PromotionError::InUse {
            environments: dependents
                .into_iter()
                .map(|name| EnvironmentName::parse(name).map_err(StoreError::corrupt))
                .collect::<Result<_, _>>()?,
        }
        .into())
    }

    /// Reads what promoting `selection` into `target` would deploy. For the
    /// source's current deployment, it must be the one requested, succeeded,
    /// and have recorded a release; the caller checks its health. Only
    /// promoted environments receive a source's deployment; plans of an
    /// earlier release may target any environment.
    ///
    /// # Errors
    /// `NotFound` for previews and releases of other applications, and the
    /// promotion errors above.
    pub async fn promotion_candidate(
        &self,
        target: &EnvironmentId,
        selection: &PromotionSelection,
    ) -> Result<PromotionCandidate, StoreError> {
        let mut snapshot = self.pool.begin().await.map_err(StoreError::database)?;
        let target = Self::environment_on(&mut snapshot, target.as_str())
            .await?
            .filter(|target| target.environment.preview().is_none())
            .ok_or(StoreError::NotFound)?;
        let (source, release, origin) = match selection {
            PromotionSelection::Source { deployment } => {
                let from = target.environment.source.promoted_from().ok_or_else(|| {
                    PromotionError::NotPromoted {
                        environment: target.environment.name.clone(),
                    }
                })?;
                let source = Self::environment_on(&mut snapshot, from.as_str())
                    .await?
                    .ok_or(StoreError::Corrupt)?;
                let (deployment, release) =
                    Self::promotable_on(&mut snapshot, &source.environment, deployment.as_deref())
                        .await?;
                let origin = DeploymentOrigin::Promotion {
                    environment: from.clone(),
                    deployment,
                };
                (Some(source), release, origin)
            }
            PromotionSelection::Release { release } => {
                (None, release.clone(), DeploymentOrigin::Release)
            }
        };
        let content =
            Self::release_on(&mut snapshot, &target.environment.application_id, &release).await?;
        Ok(PromotionCandidate {
            promotion: Promotion {
                release,
                origin,
                generated: Vec::new(),
            },
            target,
            source,
            release: content,
        })
    }

    /// The deployment of `source` a promotion may take, and its release: its
    /// current deployment, which must be `expected` when given, must have
    /// succeeded, and must have recorded a release.
    async fn promotable_on(
        connection: &mut SqliteConnection,
        source: &EnvironmentView,
        expected: Option<&str>,
    ) -> Result<(String, ReleaseId), StoreError> {
        let not_ready = |reason| PromotionError::SourceNotReady {
            environment: source.name.clone(),
            reason,
        };
        let current = Self::current_deployment_on(connection, &source.id)
            .await?
            .ok_or_else(|| not_ready("it has no current deployment"))?;
        if let Some(expected) = expected
            && expected != current.id
        {
            return Err(PromotionError::SourceChanged {
                environment: source.name.clone(),
                deployment: expected.into(),
            }
            .into());
        }
        if !current.succeeded {
            return Err(not_ready("its current deployment has not succeeded").into());
        }
        let release = current
            .release
            .ok_or_else(|| not_ready("its current deployment recorded no release"))?;
        Ok((current.id, release))
    }

    /// Reads release `id` of `application`; `NotFound` for another's.
    async fn release_on(
        connection: &mut SqliteConnection,
        application: &piqueld_core::ApplicationId,
        id: &ReleaseId,
    ) -> Result<Release, StoreError> {
        let (application, id) = (application.as_str(), id.as_str());
        let json = sqlx::query_scalar!(
            "SELECT release_json FROM releases WHERE id=?1 AND application_id=?2",
            id,
            application
        )
        .fetch_optional(connection)
        .await
        .map_err(StoreError::database)?
        .ok_or(StoreError::NotFound)?;
        serde_json::from_str(&json).map_err(StoreError::corrupt)
    }

    /// Every secret `app`, rendered for `environment`, mounts that it can't
    /// use: from the application's store, missing, being deleted, excluded by
    /// its access list, or discarded by key recovery; generated, being
    /// deleted. Fails with `secret_name_conflict` when a declared secret is
    /// also stored.
    pub(super) async fn secret_problems_on(
        connection: &mut SqliteConnection,
        environment: &EnvironmentView,
        app: &NormalizedApplication,
    ) -> Result<Vec<SecretProblem>, StoreError> {
        let stored = Self::stored_secrets_on(connection, &environment.application_id).await?;
        Self::check_declared_names(
            &app.spec().secrets,
            stored.iter().map(|secret| secret.metadata.name.as_str()),
        )?;
        let id = environment.id.as_str();
        let deleting = sqlx::query_scalar!(
            "SELECT name FROM environment_secrets WHERE environment_id=?1 AND deletion_id IS NOT NULL",
            id
        )
        .fetch_all(connection)
        .await
        .map_err(StoreError::database)?;
        Ok(app
            .spec()
            .mounted_secrets()
            .into_iter()
            .filter_map(|(name, source)| {
                let secret = name.to_owned();
                let stored = stored.iter().find(|stored| stored.metadata.name == name);
                match (source, stored) {
                    (SecretSource::Generated, _) => deleting
                        .iter()
                        .any(|deleting| deleting == name)
                        .then_some(SecretProblem::Deleting { secret }),
                    (SecretSource::Stored, None) => Some(SecretProblem::Missing { secret }),
                    (SecretSource::Stored, Some(stored)) if stored.metadata.deleting => {
                        Some(SecretProblem::Deleting { secret })
                    }
                    (SecretSource::Stored, Some(stored)) if !stored.access.allows(environment) => {
                        Some(SecretProblem::AccessDenied { secret })
                    }
                    (SecretSource::Stored, Some(stored)) if stored.metadata.unavailable => {
                        Some(SecretProblem::Unavailable { secret })
                    }
                    (SecretSource::Stored, Some(_)) => None,
                }
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect())
    }

    /// The Swarm secret each secret `environment` has now goes by: the
    /// version a deployment would pin unless it changes before then. Plans
    /// read it without pinning.
    /// # Errors
    /// Returns database errors.
    pub async fn current_secret_names(
        &self,
        environment: &EnvironmentId,
    ) -> Result<std::collections::BTreeMap<String, String>, StoreError> {
        let id = environment.as_str();
        Ok(sqlx::query!(
            r#"SELECT s.name AS "name!",v.swarm_name AS "swarm_name!" FROM environment_secrets s JOIN secret_versions v USING(environment_id,name,generation) WHERE s.environment_id=?1
            UNION ALL SELECT c.name,c.swarm_name FROM application_secrets a JOIN application_secret_copies c ON c.application_id=a.application_id AND c.name=a.name AND c.generation=a.generation WHERE c.environment_id=?1"#,
            id
        )
        .fetch_all(&self.pool)
        .await
        .map_err(StoreError::database)?
        .into_iter()
        .map(|row| (row.name, row.swarm_name))
        .collect())
    }

    /// See [`Self::secret_problems_on`].
    /// # Errors
    /// Returns `secret_name_conflict` or database errors.
    pub async fn secret_problems(
        &self,
        environment: &EnvironmentView,
        app: &NormalizedApplication,
    ) -> Result<Vec<SecretProblem>, StoreError> {
        Self::secret_problems_on(
            &mut *self.pool.acquire().await.map_err(StoreError::database)?,
            environment,
            app,
        )
        .await
    }

    /// Accepts `promotion` in `tx`. Everything checked before acceptance is
    /// checked again here, where nothing can change underneath: the target
    /// still promotes from the source, and the source deployment is still
    /// current and succeeded. Then it starts a deployment, renders the
    /// release with the target's block and variables from the release's own
    /// manifest, refuses it while any secret it mounts is unusable (listing
    /// them all), stores the values generated for its declared secrets,
    /// pins secret versions, compiles the
    /// release with its own images (`release_incompatible` when a build
    /// input renders differently), and saves the prepared target with the
    /// deployment. Its images are then retention roots, so callers holding
    /// cleanup off may let go once this commits.
    pub(super) async fn accept_promotion_on(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        target: &StoredEnvironment,
        promotion: &Promotion,
        instance: InstanceId,
    ) -> Result<Operation, StoreError> {
        let EnvironmentSource::Promoted(from) = &target.environment.source else {
            return Err(PromotionError::NotPromoted {
                environment: target.environment.name.clone(),
            }
            .into());
        };
        if target.delete_intent() {
            return Err(StoreError::IllegalTransition);
        }
        match &promotion.origin {
            DeploymentOrigin::Promotion {
                environment,
                deployment,
            } => {
                let source = Self::environment_on(tx, environment.as_str())
                    .await?
                    .filter(|_| *environment == from.environment)
                    .ok_or_else(|| PromotionError::SourceChanged {
                        environment: target.environment.name.clone(),
                        deployment: deployment.clone(),
                    })?;
                let (_, release) =
                    Self::promotable_on(tx, &source.environment, Some(deployment)).await?;
                if release != promotion.release {
                    return Err(StoreError::Corrupt);
                }
            }
            DeploymentOrigin::Release => {}
            DeploymentOrigin::Build => return Err(StoreError::InvalidInput),
        }
        let release =
            Self::release_on(tx, &target.environment.application_id, &promotion.release).await?;
        let now = now_ms();
        let operation = Self::start_operation(tx, target.id(), OperationKind::Refresh, now).await?;
        Self::write_status(tx, target.id().as_str(), "pending", None, now).await?;
        let rendering = target.render_release(&release, operation.id.clone())?;
        let secrets =
            Self::secret_problems_on(tx, &target.environment, &rendering.application).await?;
        if !secrets.is_empty() {
            return Err(PromotionError::Secrets {
                environment: target.environment.name.clone(),
                secrets,
            }
            .into());
        }
        self.store_generated_on(tx, target.id(), &promotion.generated)
            .await?;
        let pins = Self::pin_secrets_on(tx, &operation, &rendering.application).await?;
        let resolved = compile_release(
            &rendering.application,
            target.id(),
            instance,
            &release,
            pins,
        )
        .map_err(ValidationErrors::from)?;
        let (manifest, variables) = Self::snapshot_json(Some(&rendering))?;
        let template = release
            .template()
            .canonical_json()
            .map_err(StoreError::corrupt)?;
        let (release_id, origin, target_json) = (
            promotion.release.as_str(),
            serde_json::to_string(&promotion.origin).map_err(StoreError::corrupt)?,
            serde_json::to_string(&resolved).map_err(StoreError::corrupt)?,
        );
        sqlx::query!("INSERT INTO deployments(id,environment_id,manifest_json,template_json,variables_json,generation,created_at_ms,release_id,origin_json) SELECT id,environment_id,?2,?3,?4,generation,created_at_ms,?5,?6 FROM operations WHERE id=?1",operation.id,manifest,template,variables,release_id,origin)
            .execute(&mut **tx).await.map_err(StoreError::database)?;
        sqlx::query!(
            "UPDATE operations SET target_json=?1 WHERE id=?2",
            target_json,
            operation.id
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        let id = target.id().as_str();
        sqlx::query!(
            "UPDATE environments SET manifest_json=?1,updated_at_ms=?2 WHERE id=?3",
            template,
            now,
            id
        )
        .execute(&mut **tx)
        .await
        .map_err(StoreError::database)?;
        Self::operation_event(tx, &operation.id, "target_resolved", None, now).await?;
        Ok(operation)
    }
}

#[cfg(test)]
mod tests;
