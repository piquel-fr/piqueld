//! Application and environment queries and previews shared by every transport.
use super::views::{application_view, detail_diagnostics, observed_view, status_view};
use super::{ApplicationError, ApplicationService, BoundaryError};
use crate::store::{StoreError, StoredEnvironment};
use piqueld_core::{
    ApplicationId, EnvironmentId, NormalizedApplication, ObservedApplication, Plan, PlanRequest,
    ResolutionSet, ValidatedApplication,
    api::{
        ApplicationSummary, ApplicationView, DiagnosticView, EnvironmentDetailView,
        EnvironmentStatusView, EnvironmentView, MAX_APPLICATION_PAGE_SIZE, ManifestChange, Page,
        PlanView, ServiceRolloutView,
    },
    compile_application, preview_resolution,
};

impl ApplicationService {
    /// Lists saved application summaries and their environments without
    /// observing the runtime.
    /// # Errors
    /// Returns invalid pagination or storage errors.
    pub async fn applications(
        &self,
        cursor: Option<&str>,
        limit: Option<u16>,
    ) -> Result<Page<ApplicationSummary>, ApplicationError> {
        let limit = limit.unwrap_or(MAX_APPLICATION_PAGE_SIZE);
        if !(1..=MAX_APPLICATION_PAGE_SIZE).contains(&limit) {
            return Err(ApplicationError::InvalidPagination);
        }
        self.store
            .list_summaries(cursor, usize::from(limit))
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
        Ok(EnvironmentDetailView {
            environment: stored.environment,
            application: application_view(stored.application, environments),
            status,
            observed: observed_view,
            latest_operation,
            diagnostics,
        })
    }
    /// Previews validated intent without pulling images or saving configuration.
    ///
    /// Checks the generation and identity preconditions when supplied (unlike
    /// apply, they are optional). For an application with one environment,
    /// builds a runtime plan against its current observation and diffs the
    /// manifest against its latest deployment's captured input (no baseline
    /// after a delete). With several environments (or none), diffs against the
    /// saved configuration without a runtime plan, since apply deploys none of them.
    /// # Errors
    /// Returns precondition, storage, or runtime errors.
    /// # Panics
    /// Panics if the built-in preview application ID is invalid.
    pub async fn plan(
        &self,
        manifest: ValidatedApplication,
        expected: Option<u64>,
        expected_id: Option<String>,
    ) -> Result<PlanView, ApplicationError> {
        let current = self.store.find_by_name(manifest.name().as_str()).await?;
        crate::store::Store::check_generation(
            expected,
            current.as_ref().map_or(0, |app| app.generation),
        )?;
        if let Some(expected_id) = expected_id
            && current
                .as_ref()
                .is_none_or(|app| app.application.id().as_str() != expected_id)
        {
            return Err(StoreError::IdentityConflict.into());
        }
        // New applications are previewed under a placeholder ID.
        let id = current.as_ref().map_or_else(
            || ApplicationId::parse("preview-application").expect("valid preview ID"),
            |app| app.application.id().clone(),
        );
        let application = manifest.normalize(id.clone());
        let environments = match &current {
            Some(_) => self.store.environments(&id).await?,
            None => Vec::new(),
        };
        let environment = match environments.as_slice() {
            [environment] => Some(self.store.get(&environment.id).await?),
            _ => None,
        };
        let (operation, baseline, mut plan) = if let Some(environment) = &environment {
            let operation = self
                .store
                .latest_operation_for_environment(environment.id())
                .await?;
            let baseline = match &operation {
                Some(op) if op.kind != piqueld_core::OperationKind::Delete => {
                    Some(self.store.deployment_manifest(&op.id).await?)
                }
                _ => None,
            };
            let plan = self
                .preview_plan(&application, environment.id(), Some(environment))
                .await?;
            (operation, baseline, plan)
        } else if let Some(current) = &current {
            (None, Some(current.application.clone()), Plan::default())
        } else {
            let environment = EnvironmentId::default_for(&id);
            let plan = self.preview_plan(&application, &environment, None).await?;
            (None, None, plan)
        };
        plan.warn_rollouts(&application);
        Ok(PlanView {
            application_id: id.to_string(),
            generation: current.as_ref().map_or(0, |app| app.generation),
            identical: baseline
                .as_ref()
                .is_some_and(|app| app.spec() == application.spec()),
            operation,
            changes: ManifestChange::between(baseline.as_ref(), &application),
            plan,
            rollouts: ServiceRolloutView::for_application(&application),
        })
    }
    /// Plans the runtime changes for `app` in `environment` against the
    /// current observation of `current`, or of nothing for a new environment.
    ///
    /// Images are not resolved, so the desired state is only compiled when no
    /// references need resolution; otherwise the plan lists them as unresolved.
    /// New environments still require a reachable runtime. Configuration values
    /// are redacted from the returned plan.
    async fn preview_plan(
        &self,
        app: &NormalizedApplication,
        environment: &EnvironmentId,
        current: Option<&StoredEnvironment>,
    ) -> Result<piqueld_core::Plan, ApplicationError> {
        let observed = if let Some(current) = current {
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
