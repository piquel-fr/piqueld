//! Application queries and previews shared by every transport.
use super::views::{application_view, detail_diagnostics, observed_view, status_view};
use super::{ApplicationError, ApplicationService, BoundaryError};
use crate::store::{StoreError, StoredApplication};
use piqueld_core::{
    ApplicationId, NormalizedApplication, ObservedApplication, Plan, PlanRequest, ResolutionSet,
    ValidatedApplication,
    api::{
        ApplicationDetailView, ApplicationStatusView, ApplicationSummary, ApplicationView,
        DiagnosticView, MAX_APPLICATION_PAGE_SIZE, ManifestChange, Page, PlanView,
    },
    compile_application, preview_resolution,
};

impl ApplicationService {
    /// Lists saved application summaries without observing the runtime.
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
        let page = self
            .store
            .list_summaries(cursor, usize::from(limit))
            .await
            .map_err(|error| match error {
                StoreError::InvalidInput | StoreError::InvalidInputSource(_) => {
                    ApplicationError::InvalidPagination
                }
                error => error.into(),
            })?;
        Ok(Page {
            items: page
                .items
                .into_iter()
                .map(|stored| ApplicationSummary {
                    id: stored.id,
                    name: stored.name,
                    generation: stored.generation,
                    resolved_generation: stored.resolved_generation,
                    delete_intent: stored.delete_intent,
                    created_at_ms: stored.created_at_ms,
                    updated_at_ms: stored.updated_at_ms,
                })
                .collect(),
            next_cursor: page.next_cursor,
        })
    }
    /// Reads saved application intent.
    /// # Errors
    /// Returns absence or storage errors.
    pub async fn application(
        &self,
        id: &ApplicationId,
    ) -> Result<ApplicationView, ApplicationError> {
        Ok(application_view(self.store.get(id).await?))
    }
    /// Reads persisted deployment progress and runtime health.
    /// # Errors
    /// Returns absence or storage errors.
    pub async fn application_status(
        &self,
        id: &ApplicationId,
    ) -> Result<ApplicationStatusView, ApplicationError> {
        Ok(status_view(self.store.status(id).await?))
    }
    /// Combines saved intent, observed state, history, and bounded diagnostics.
    /// Runtime outages are returned as diagnostics so saved intent remains readable.
    /// # Errors
    /// Returns absence or storage errors.
    pub async fn application_detail(
        &self,
        id: &ApplicationId,
    ) -> Result<ApplicationDetailView, ApplicationError> {
        let (stored, status) = self.store.get_with_status(id).await?;
        let (observed, observation_error) = if stored.resolved.is_none() {
            (ObservedApplication::default(), None)
        } else {
            match self.runtime.observe(&stored).await {
                Ok(observed) => (observed, None),
                Err(error) => {
                    tracing::warn!(%error,"application detail observation failed");
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
        let latest_operation = self.store.latest_operation_for_application(id).await?;
        let mut diagnostics =
            detail_diagnostics(&status, &observed_view, latest_operation.as_ref());
        diagnostics.extend(observation_error);
        Ok(ApplicationDetailView {
            application: application_view(stored),
            status,
            observed: observed_view,
            latest_operation,
            diagnostics,
        })
    }
    /// Previews validated intent without pulling images or saving configuration.
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
        let id = current.as_ref().map_or_else(
            || ApplicationId::parse("preview-application").expect("valid preview ID"),
            |app| app.application.id().clone(),
        );
        let application = manifest.normalize(id.clone());
        let plan = self.preview_plan(&application, current.as_ref()).await?;
        let operation = if let Some(current) = &current {
            self.store
                .latest_operation_for_application(current.application.id())
                .await?
        } else {
            None
        };
        let baseline = if let Some(op) = &operation {
            if op.kind == piqueld_core::OperationKind::Delete {
                None
            } else {
                Some(self.store.deployment_manifest(&op.id).await?)
            }
        } else {
            None
        };
        Ok(PlanView {
            application_id: id.to_string(),
            generation: current.as_ref().map_or(0, |app| app.generation),
            identical: baseline
                .as_ref()
                .is_some_and(|app| app.spec() == application.spec()),
            operation,
            changes: ManifestChange::between(baseline.as_ref(), &application),
            plan,
        })
    }
    async fn preview_plan(
        &self,
        app: &NormalizedApplication,
        current: Option<&StoredApplication>,
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
                    piqueld_core::InstanceId::parse(self.store.instance_id())
                        .map_err(StoreError::corrupt)?,
                    &resolutions,
                )
                .map_err(BoundaryError::Compilation)?,
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
