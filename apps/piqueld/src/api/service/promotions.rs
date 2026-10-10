//! Promoting releases into promoted environments, and planning a release
//! for an environment. Neither builds nor fetches: a release's images are
//! deployed as recorded.
use super::{ApplicationError, ApplicationService, Mutation, MutationResponse, PromotionMutation};
use crate::store::{Promotion, PromotionCandidate, PromotionError, StoreError, StoredEnvironment};
use piqueld_core::{
    EnvironmentId, Plan, PlanRequest, ResolvedApplication, ValidationErrors,
    api::{
        AcceptedPromotion, ManifestChange, PlanView, PromotionSelection, ReleasePlan,
        SecretProblem, ServiceRolloutView,
    },
    compile_release,
    manifest::{PREVIEW_DEPLOYMENT_ID, Rendering},
};

impl ApplicationService {
    /// Promotes `selection` into the promoted environment `id`, pinning its
    /// release at acceptance. Every precondition is checked before anything
    /// is captured:
    ///
    /// 1. When promoting from the source, its current deployment is the one
    ///    requested, succeeded, and is observed healthy now.
    /// 2. The release renders for the environment and its build inputs render
    ///    as they did for it (`release_incompatible`).
    /// 3. Its images are present, pulled again by digest when they can be;
    ///    cleanup is held off from then until the target is saved.
    /// 4. Every secret it mounts exists and allows the environment, all
    ///    reported at once (`secrets_unavailable`).
    ///
    /// Acceptance checks the source again in its transaction, so a source
    /// that moves on meanwhile fails with `promotion_source_changed` rather
    /// than retargeting the request.
    ///
    /// # Errors
    /// Returns those failures, precondition, conflict, runtime, or storage errors.
    pub async fn promote(
        &self,
        actor: super::Actor<'_>,
        id: &EnvironmentId,
        selection: PromotionSelection,
        expected_generation: Option<u64>,
        force: bool,
        request_id: Option<&str>,
    ) -> Result<AcceptedPromotion, ApplicationError> {
        // A repeated request returns what it got, whatever changed since.
        let request = |promotion| {
            Mutation::Promotion(PromotionMutation::Promote {
                id: id.clone(),
                selection: selection.clone(),
                promotion,
            })
        };
        let replayed = self
            .store
            .replay(
                actor,
                &request(None),
                expected_generation,
                force,
                request_id,
            )
            .await?;
        if let Some(response) = replayed {
            return Self::promotion_response(response);
        }
        let candidate = self.store.promotion_candidate(id, &selection).await?;
        self.check_source_health(&candidate).await?;
        let (rendering, _, secrets) = self.instantiate(&candidate).await?;
        if !secrets.is_empty() {
            return Err(StoreError::from(PromotionError::Secrets {
                environment: candidate.target.environment.name.clone(),
                secrets,
            })
            .into());
        }
        let _in_use = self
            .runtime
            .reuse_images(candidate.release.sources())
            .await?;
        // Generated off the writer lock, stored only if accepted.
        let generated = self
            .store
            .generated_secrets(id, &rendering.application)
            .await?;
        let promotion = Promotion {
            generated,
            ..candidate.promotion
        };
        // Boxed: acceptance would otherwise make every promotion future large.
        let response = Box::pin(self.accept(
            actor,
            request(Some(promotion)),
            expected_generation,
            force,
            request_id,
        ))
        .await?;
        Self::promotion_response(response)
    }

    /// The accepted promotion a promotion request responded with.
    fn promotion_response(
        response: MutationResponse,
    ) -> Result<AcceptedPromotion, ApplicationError> {
        match response {
            MutationResponse::Promotion(accepted) => Ok(accepted),
            _ => Err(StoreError::Corrupt.into()),
        }
    }

    /// Plans deploying `selection` into environment `id` without changing
    /// anything: the release and its provenance and image availability, the
    /// rendered diff against the environment's current deployment, the
    /// runtime plan, new (empty) volumes, and every missing or inaccessible
    /// secret. The source must be promotable, as for [`Self::promote`]; an
    /// earlier release may be planned for any environment.
    ///
    /// # Errors
    /// Returns the source, rendering, `release_incompatible`, runtime, or
    /// storage errors of a promotion.
    pub async fn plan_release(
        &self,
        id: &EnvironmentId,
        selection: &PromotionSelection,
    ) -> Result<PlanView, ApplicationError> {
        let candidate = self.store.promotion_candidate(id, selection).await?;
        self.check_source_health(&candidate).await?;
        let target = &candidate.target;
        let (rendering, desired, secrets) = self.instantiate(&candidate).await?;
        let app = &rendering.application;
        let observed = self.runtime.observe(target).await?;
        let new_volumes = desired
            .volumes
            .iter()
            .filter(|volume| {
                !observed
                    .volumes
                    .iter()
                    .any(|observed| observed.name == volume.name.as_str())
            })
            .map(|volume| volume.logical_name.to_string())
            .collect();
        let ingress = self.ingress.as_ref().is_some_and(|ingress| ingress.enabled);
        let mut plan = Plan::from_request(
            &PlanRequest::Preview {
                unresolved: Vec::new(),
                desired: Some(desired.with_ingress(ingress)),
            },
            &observed,
        );
        plan.redact_configuration();
        plan.warn_rollouts(app);
        // Compare with what runs, not with a later attempt that never published.
        let operation = self
            .store
            .latest_operation_for_environment(target.id())
            .await?;
        let baseline = match self.store.current_deployment(target.id()).await? {
            Some(current) => self
                .store
                .deployment_snapshot(&current.id)
                .await?
                .rendering
                .map(|rendering| rendering.application),
            None => None,
        };
        let application = &target.environment.application_id;
        let mut release = self
            .store
            .release(application, &candidate.promotion.release)
            .await?;
        match self.runtime.local_images().await {
            Ok(present) => {
                release.availability = Some(present.availability(release.release.sources()));
            }
            Err(error) => tracing::warn!(?error, "release availability unknown"),
        }
        let proposed = app.spec();
        Ok(PlanView {
            application_id: application.to_string(),
            generation: target.application.generation,
            identical: baseline.as_ref().is_some_and(|app| app.spec() == proposed),
            operation,
            changes: ManifestChange::between(
                baseline.map(|app| app.spec().to_input()).as_ref(),
                &proposed.to_input(),
            ),
            plan,
            rollouts: ServiceRolloutView::for_application(app),
            variables: rendering.values.clone(),
            release: Some(ReleasePlan {
                release,
                origin: candidate.promotion.origin,
                new_volumes,
                secrets,
            }),
        })
    }

    /// Requires the source of a promotion, when there is one, to run every
    /// service of its current deployment, converged, right now.
    async fn check_source_health(
        &self,
        candidate: &PromotionCandidate,
    ) -> Result<(), ApplicationError> {
        let Some(source) = &candidate.source else {
            return Ok(());
        };
        let observed = self.runtime.observe(source).await?;
        // Network attachments include ingress's, projected as the controller does.
        let ingress = self.ingress.as_ref().is_some_and(|ingress| ingress.enabled);
        let routes = self.store.applied_routes(source.id()).await?;
        if source.resolved.as_ref().is_some_and(|target| {
            target
                .clone()
                .with_ingress_routes(ingress, &routes)
                .converged_in(&observed)
        }) {
            return Ok(());
        }
        Err(StoreError::from(PromotionError::SourceNotReady {
            environment: source.environment.name.clone(),
            reason: "not every service of its current deployment is running and healthy",
        })
        .into())
    }

    /// Instantiates the candidate's release for its target as a plan does,
    /// under the application's current name: renders it with the target's
    /// block and variables from the release's own manifest, compiles it with
    /// the release's images (`release_incompatible` when a build input
    /// renders differently), and lists the secrets it mounts that are
    /// unusable. Nothing is pinned: each secret is compiled with the Swarm
    /// secret of its current version, which acceptance would pin, or by
    /// name when it has none yet.
    async fn instantiate(
        &self,
        candidate: &PromotionCandidate,
    ) -> Result<(Rendering, ResolvedApplication, Vec<SecretProblem>), StoreError> {
        let target: &StoredEnvironment = &candidate.target;
        let rendering = target.render_release(&candidate.release, PREVIEW_DEPLOYMENT_ID.into())?;
        let app = &rendering.application;
        // Unchanged versions keep the Swarm secret services run; new ones
        // stand in by name.
        let current = self.store.current_secret_names(target.id()).await?;
        let pins = app
            .spec()
            .mounted_secret_names()
            .into_iter()
            .map(|name| {
                let pinned = current.get(name).map_or(name, String::as_str);
                (name.to_owned(), pinned.to_owned())
            })
            .collect();
        let instance = piqueld_core::InstanceId::parse(self.store.instance_id())
            .map_err(StoreError::corrupt)?;
        let resolved = compile_release(app, target.id(), instance, &candidate.release, pins)
            .map_err(ValidationErrors::from)?;
        let secrets = self.store.secret_problems(&target.environment, app).await?;
        Ok((rendering, resolved, secrets))
    }
}
