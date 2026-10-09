//! Application and environment queries and previews shared by every transport.
use super::views::{application_view, detail_diagnostics, observed_view, status_view};
use super::{ApplicationError, ApplicationService, BoundaryError};
use crate::store::{StoreError, StoredEnvironment};
use piqueld_core::{
    ApplicationId, EnvironmentId, EnvironmentName, NormalizedApplication, ObservedApplication,
    Plan, PlanRequest, ResolutionSet,
    access::{AppPermission, Grants, Scope, Target},
    api::{
        ApplicationSummary, ApplicationView, DiagnosticView, EnvironmentDetailView,
        EnvironmentStatusView, EnvironmentView, MAX_APPLICATION_PAGE_SIZE, ManifestChange, Page,
        PlanView, ServiceRolloutView,
    },
    compile_application,
    manifest::{RenderContext, Rendering, RepositoryManifest, ValidatedTemplate},
    preview_resolution,
};

impl ApplicationService {
    /// Lists saved application summaries and their environments without
    /// observing the runtime.
    /// # Errors
    /// Returns invalid pagination or storage errors.
    pub async fn applications(
        &self,
        visible: &Scope,
        cursor: Option<&str>,
        limit: Option<u16>,
    ) -> Result<Page<ApplicationSummary>, ApplicationError> {
        let limit = limit.unwrap_or(MAX_APPLICATION_PAGE_SIZE);
        if !(1..=MAX_APPLICATION_PAGE_SIZE).contains(&limit) {
            return Err(ApplicationError::InvalidPagination);
        }
        self.store
            .list_summaries(visible, cursor, usize::from(limit))
            .await
            .map_err(|error| match error {
                StoreError::InvalidInput | StoreError::InvalidInputSource(_) => {
                    ApplicationError::InvalidPagination
                }
                error => error.into(),
            })
    }
    /// Reads saved application intent and its environments.
    /// # Errors
    /// Returns absence or storage errors.
    pub async fn application(
        &self,
        id: &ApplicationId,
    ) -> Result<ApplicationView, ApplicationError> {
        let application = self.store.application(id).await?;
        let environments = self.store.environments(id).await?;
        Ok(application_view(application, environments))
    }
    /// Reads an environment's metadata.
    /// # Errors
    /// Returns absence or storage errors.
    pub async fn environment(
        &self,
        id: &EnvironmentId,
    ) -> Result<EnvironmentView, ApplicationError> {
        Ok(self.store.get(id).await?.environment)
    }
    /// The application owning an environment, if the environment exists.
    /// # Errors
    /// Returns storage errors.
    pub async fn environment_application(
        &self,
        id: &EnvironmentId,
    ) -> Result<Option<ApplicationId>, ApplicationError> {
        Ok(self.store.environment_application(id).await?)
    }
    /// Reads persisted deployment progress and runtime health.
    /// # Errors
    /// Returns absence or storage errors.
    pub async fn environment_status(
        &self,
        id: &EnvironmentId,
    ) -> Result<EnvironmentStatusView, ApplicationError> {
        Ok(status_view(self.store.status(id).await?))
    }
    /// Combines saved intent, observed state, history, and bounded diagnostics.
    /// Runtime outages are returned as diagnostics so saved intent remains readable.
    /// # Errors
    /// Returns absence or storage errors.
    pub async fn environment_detail(
        &self,
        id: &EnvironmentId,
    ) -> Result<EnvironmentDetailView, ApplicationError> {
        let (stored, status) = self.store.get_with_status(id).await?;
        let (observed, observation_error) = if stored.resolved.is_none() {
            (ObservedApplication::default(), None)
        } else {
            match self.runtime.observe(&stored).await {
                Ok(observed) => (observed, None),
                Err(error) => {
                    tracing::warn!(%error,"environment detail observation failed");
                    (ObservedApplication::default(),Some(DiagnosticView{code:"runtime_unavailable".into(),message:"Runtime observation is unavailable. Saved configuration and deployment history are still available.".into()}))
                }
            }
        };
        let observed_view = observed_view(
            &stored,
            &observed,
            observation_error.is_none() && status.state == piqueld_core::ApplicationState::Ready,
        );
        let status = status_view(status);
        let latest_operation = self.store.latest_operation_for_environment(id).await?;
        let mut diagnostics =
            detail_diagnostics(&status, &observed_view, latest_operation.as_ref());
        diagnostics.extend(observation_error);
        let environments = self
            .store
            .environments(&stored.environment.application_id)
            .await?;
        let release = self.store.current_release(id).await?;
        Ok(EnvironmentDetailView {
            manifest: stored.manifest().cloned(),
            release,
            environment: stored.environment,
            application: application_view(stored.application, environments),
            status,
            observed: observed_view,
            latest_operation,
            diagnostics,
        })
    }
    /// Checks a preview's preconditions: `grants` may save `current` (or
    /// create it when absent), and the optional `expected` generation and
    /// `expected_id` match it.
    fn check_plan(
        grants: &Grants,
        current: Option<&crate::store::StoredApplication>,
        expected: Option<u64>,
        expected_id: Option<String>,
    ) -> Result<(), ApplicationError> {
        let target = current.map_or(Target::New, |current| {
            Target::Named(current.application.id())
        });
        grants
            .require_change(&[AppPermission::Write], target)
            .map_err(StoreError::Denied)?;
        crate::store::Store::check_generation(expected, current.map_or(0, |app| app.generation))?;
        if let Some(expected_id) = expected_id
            && current.is_none_or(|app| app.application.id().as_str() != expected_id)
        {
            return Err(StoreError::IdentityConflict.into());
        }
        Ok(())
    }
    /// Previews validated intent without pulling images or saving configuration.
    ///
    /// Checks the generation and identity preconditions when supplied (unlike
    /// apply, they are optional). For `environment`, or the application's only
    /// environment when it is omitted, renders the manifest for it at its
    /// branch (see [`RenderContext::preview`]), builds a runtime plan against its current
    /// observation, and diffs it against its latest deployment's rendered
    /// manifest (no baseline after a delete). Without an environment and with
    /// several, diffs the saved manifest without rendering or a runtime plan,
    /// since apply deploys none of them. A new application renders for its
    /// first environment. An `environment` of another application is
    /// `NotFound`. `[spec.environments.<name>]` blocks naming no environment
    /// are warnings. Like a deployment, the rendering fails with
    /// `secret_access_denied` when it mounts a stored secret the environment
    /// may not use.
    /// # Errors
    /// Returns precondition, rendering, secret access, storage, or runtime errors.
    /// # Panics
    /// Panics if the built-in preview application ID is invalid.
    pub async fn plan(
        &self,
        grants: &Grants,
        manifest: ValidatedTemplate,
        expected: Option<u64>,
        expected_id: Option<String>,
        environment: Option<&EnvironmentId>,
    ) -> Result<PlanView, ApplicationError> {
        let current = self.store.find_by_name(manifest.name().as_str()).await?;
        Self::check_plan(grants, current.as_ref(), expected, expected_id)?;
        // New applications are previewed under a placeholder ID.
        let id = current.as_ref().map_or_else(
            || ApplicationId::parse("preview-application").expect("valid preview ID"),
            |app| app.application.id().clone(),
        );
        let template = manifest.normalize(id.clone());
        let environments = match &current {
            Some(_) => self.store.environments(&id).await?,
            None => Vec::new(),
        };
        let environment = self
            .plan_environment(environment, current.is_some(), &id, &environments)
            .await?;
        let render = |environment: EnvironmentName,
                      repository: Option<&RepositoryManifest>|
         -> Result<Rendering, StoreError> {
            let repository = repository.or(template.spec().manifest.as_ref());
            Ok(template.render(&RenderContext::preview(environment, repository))?)
        };
        let (operation, identical, changes, rendering, mut plan) = if let Some(environment) =
            &environment
        {
            let repository = environment.repository();
            let rendering = render(environment.environment.name.clone(), repository.as_ref())?;
            let (operation, baseline) = self.latest_deployment(environment.id()).await?;
            let plan = self
                .preview_plan(&rendering.application, environment.id(), Some(environment))
                .await?;
            let proposed = rendering.application.spec();
            (
                operation,
                baseline.as_ref().is_some_and(|app| app.spec() == proposed),
                ManifestChange::between(
                    baseline.map(|app| app.spec().to_input()).as_ref(),
                    &proposed.to_input(),
                ),
                Some(rendering),
                plan,
            )
        } else if let Some(current) = &current {
            let saved = current.application.spec();
            (
                None,
                saved == template.spec(),
                ManifestChange::between(Some(saved), template.spec()),
                None,
                Plan::default(),
            )
        } else {
            let rendering = render(EnvironmentName::default_name(), None)?;
            let environment = EnvironmentId::default_for(&id);
            let plan = self
                .preview_plan(&rendering.application, &environment, None)
                .await?;
            let changes = ManifestChange::between(None, &rendering.application.spec().to_input());
            (None, false, changes, Some(rendering), plan)
        };
        let names = match &current {
            Some(_) => environments
                .into_iter()
                .map(|environment| environment.name)
                .collect(),
            None => vec![EnvironmentName::default_name()],
        };
        plan.warn_environments(&template, &names);
        let (rollouts, variables) = match rendering {
            Some(rendering) => {
                plan.warn_rollouts(&rendering.application);
                (
                    ServiceRolloutView::for_application(&rendering.application),
                    rendering.values,
                )
            }
            None => (Vec::new(), std::collections::BTreeMap::new()),
        };
        Ok(PlanView {
            application_id: id.to_string(),
            generation: current.as_ref().map_or(0, |app| app.generation),
            identical,
            operation,
            changes,
            plan,
            rollouts,
            variables,
        })
    }
    /// The environment's latest operation and, unless it deletes, the
    /// manifest it rendered.
    async fn latest_deployment(
        &self,
        environment: &EnvironmentId,
    ) -> Result<
        (
            Option<piqueld_core::Operation>,
            Option<NormalizedApplication>,
        ),
        StoreError,
    > {
        let operation = self
            .store
            .latest_operation_for_environment(environment)
            .await?;
        let baseline = match &operation {
            Some(op) if op.kind != piqueld_core::OperationKind::Delete => self
                .store
                .deployment_snapshot(&op.id)
                .await?
                .rendering
                .map(|rendering| rendering.application),
            _ => None,
        };
        Ok((operation, baseline))
    }

    /// The environment a plan compares with: `selected`, which must belong to
    /// application `id`, or its only environment. `None` with several
    /// environments, or for a new application.
    async fn plan_environment(
        &self,
        selected: Option<&EnvironmentId>,
        exists: bool,
        id: &ApplicationId,
        environments: &[EnvironmentView],
    ) -> Result<Option<StoredEnvironment>, ApplicationError> {
        Ok(match (selected, exists) {
            (Some(environment), true) => {
                let environment = self.store.get(environment).await?;
                if environment.environment.application_id != *id {
                    return Err(StoreError::NotFound.into());
                }
                Some(environment)
            }
            (Some(_), false) => return Err(StoreError::NotFound.into()),
            (None, true) => match environments {
                [environment] => Some(self.store.get(&environment.id).await?),
                _ => None,
            },
            (None, false) => None,
        })
    }

    /// Plans the runtime changes for `app` in `environment` against the
    /// current observation of `current`, or of nothing for a new environment.
    ///
    /// Images are not resolved, so the desired state is only compiled when no
    /// references need resolution; otherwise the plan lists them as unresolved.
    /// New environments still require a reachable runtime. Configuration values
    /// are redacted from the returned plan. Like a deployment, an existing
    /// environment fails with `secret_access_denied` when `app` mounts a
    /// stored secret it may not use.
    async fn preview_plan(
        &self,
        app: &NormalizedApplication,
        environment: &EnvironmentId,
        current: Option<&StoredEnvironment>,
    ) -> Result<piqueld_core::Plan, ApplicationError> {
        let observed = if let Some(current) = current {
            self.store
                .check_secret_access(&current.environment, app)
                .await?;
            self.runtime.observe(current).await?
        } else {
            self.runtime.check_available().await?;
            ObservedApplication::default()
        };
        let resolutions = ResolutionSet::default();
        let unresolved = preview_resolution(app, &resolutions);
        let desired = if unresolved.is_empty() {
            Some(
                compile_application(
                    app,
                    environment,
                    piqueld_core::InstanceId::parse(self.store.instance_id())
                        .map_err(StoreError::corrupt)?,
                    &resolutions,
                )
                .map_err(BoundaryError::Compilation)?
                .with_ingress(self.ingress.as_ref().is_some_and(|ingress| ingress.enabled)),
            )
        } else {
            None
        };
        let mut plan = Plan::from_request(
            &PlanRequest::Preview {
                unresolved,
                desired,
            },
            &observed,
        );
        plan.redact_configuration();
        Ok(plan)
    }
}
