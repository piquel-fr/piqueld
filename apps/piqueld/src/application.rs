//! Accept application intent after resolution and conflict checks; deduplicate targets on the server.

mod runtime;
pub use runtime::DockerRuntime;

use crate::{
    docker::DockerError,
    store::{SqliteStore, StoreError, StoredApplication},
};
use async_trait::async_trait;
use piqueld_core::{
    ApplicationId, CompileError, NormalizedApplication, ObservedApplication, Operation,
    OperationState, ValidatedApplication, resource::ResolvedApplication,
};
use std::sync::Arc;
use tokio::sync::Mutex;

#[derive(Clone, Debug)]
/// Resolved desired state paired with the initial runtime observation.
pub struct PreparedApplication {
    /// Immutable desired application state.
    pub resolved: ResolvedApplication,
    /// Runtime resources observed before planning.
    pub observed: ObservedApplication,
}

#[derive(Debug, thiserror::Error)]
/// Errors crossing the runtime boundary.
pub enum BoundaryError {
    /// A Docker runtime request failed.
    #[error("runtime request failed")]
    Runtime(#[from] DockerError),
    /// Resolved inputs could not be compiled into desired runtime resources.
    #[error("application compilation failed")]
    Compilation(Vec<CompileError>),
}

/// Resolves application inputs and reads the runtime before accepting desired state.
#[async_trait]
pub trait RuntimeBoundary: Send + Sync + 'static {
    /// Wakes the reconciler after a mutation requests an immediate scan.
    fn trigger_reconciliation(&self) {}
    /// Resolves mutable inputs and captures an initial runtime observation.
    async fn prepare(
        &self,
        application: &NormalizedApplication,
    ) -> Result<PreparedApplication, BoundaryError>;
    /// Captures current runtime state for a stored application.
    async fn observe(
        &self,
        application: &StoredApplication,
    ) -> Result<ObservedApplication, BoundaryError>;
}

/// Errors while accepting an application target.
#[derive(Debug, thiserror::Error)]
pub enum ApplicationError {
    /// Persistence failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// The runtime plan contains a known conflict.
    #[error("runtime plan contains blocking conflicts")]
    PlanBlocked(Vec<piqueld_core::PlanDiagnostic>),
    /// Image resolution or compilation failed.
    #[error(transparent)]
    Runtime(#[from] BoundaryError),
}

/// Application use cases. HTTP only decodes requests; the store only persists decisions.
#[derive(Clone)]
pub struct Applications {
    pub(crate) store: Arc<SqliteStore>,
    pub(crate) runtime: Arc<dyn RuntimeBoundary>,
    mutations: Arc<Mutex<()>>,
}

impl Applications {
    /// Creates the application service. Clones share mutation serialization.
    #[must_use]
    pub fn new(store: Arc<SqliteStore>, runtime: Arc<dyn RuntimeBoundary>) -> Self {
        Self {
            store,
            runtime,
            mutations: Arc::new(Mutex::new(())),
        }
    }

    /// Resolves a manifest and accepts only a changed target, or retries failed work.
    ///
    /// # Errors
    /// Returns a resolution, compilation or persistence error.
    pub async fn apply(
        &self,
        manifest: ValidatedApplication,
    ) -> Result<Operation, ApplicationError> {
        // Serialize name lookup, resolution and acceptance. This intentionally keeps
        // the prototype's mutation ordering straightforward, including concurrent creates.
        let _guard = self.mutations.lock().await;
        let current = self.store.find_by_name(manifest.name()).await?;
        let id = current
            .as_ref()
            .map_or_else(Self::new_id, |current| current.application.id.clone());
        let application = manifest.normalize(id);
        let prepared = self.runtime.prepare(&application).await?;
        if let Some(current) = current
            && !current.delete_intent
            && current.resolved == prepared.resolved
            && let Some(operation) = self
                .store
                .latest_operation_for_application(&application.id)
                .await?
        {
            return self.reuse(operation).await;
        }
        let plan = piqueld_core::Plan::from_request(
            &piqueld_core::PlanRequest::Reconcile {
                desired: prepared.resolved.clone(),
            },
            &prepared.observed,
        );
        if plan.is_blocked() {
            return Err(ApplicationError::PlanBlocked(plan.diagnostics));
        }
        let operation = self
            .store
            .save_application(&application, &prepared.resolved)
            .await?;
        self.runtime.trigger_reconciliation();
        Ok(operation)
    }

    /// Requests absence of services and networks; repeated deletion reuses its operation.
    ///
    /// # Errors
    /// Returns a persistence error or `NotFound`.
    pub async fn delete(&self, id: &ApplicationId) -> Result<Operation, ApplicationError> {
        let _guard = self.mutations.lock().await;
        if let Some(operation) = self.store.latest_operation_for_application(id).await?
            && operation.kind == piqueld_core::OperationKind::Delete
        {
            return self.reuse(operation).await;
        }
        let operation = self.store.request_delete(id).await?;
        self.runtime.trigger_reconciliation();
        Ok(operation)
    }

    fn new_id() -> ApplicationId {
        ApplicationId::parse(format!("app-{}", uuid::Uuid::now_v7().simple()))
            .expect("UUID application ID is valid")
    }

    async fn reuse(&self, operation: Operation) -> Result<Operation, ApplicationError> {
        if matches!(
            operation.state,
            OperationState::Failed | OperationState::Cancelled
        ) {
            let operation = self.store.retry_operation(&operation).await?;
            self.runtime.trigger_reconciliation();
            Ok(operation)
        } else {
            Ok(operation)
        }
    }
}
