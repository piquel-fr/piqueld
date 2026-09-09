//! Accept manifest intent immediately; Docker preparation belongs to operation execution.

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

/// Resolves accepted intent during execution and observes runtime for API reads.
#[async_trait]
pub trait RuntimeBoundary: Send + Sync + 'static {
    /// Wakes the reconciler after a mutation requests an immediate scan.
    fn trigger_reconciliation(&self) {}
    /// Resolves all mutable inputs into a complete immutable target.
    async fn prepare(
        &self,
        application: &NormalizedApplication,
    ) -> Result<ResolvedApplication, BoundaryError>;
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

    /// Accepts normalized intent without waiting for Docker or image resolution.
    /// # Errors
    /// Returns storage or generation errors.
    pub async fn apply(
        &self,
        manifest: ValidatedApplication,
        expected: Option<u64>,
    ) -> Result<Operation, ApplicationError> {
        let _guard = self.mutations.lock().await;
        let current = self.store.find_by_name(manifest.name()).await?;
        SqliteStore::check_generation(expected, current.as_ref().map_or(0, |app| app.generation))?;
        let id = current
            .as_ref()
            .map_or_else(Self::new_id, |app| app.application.id.clone());
        let application = manifest.normalize(id);
        if let Some(current) = current
            && !current.delete_intent
            && current.application == application
            && let Some(operation) = self
                .store
                .latest_operation_for_application(&application.id)
                .await?
        {
            return self.reuse(operation).await;
        }
        let operation = self
            .store
            .save_application(&application, None, expected)
            .await?;
        self.runtime.trigger_reconciliation();
        Ok(operation)
    }

    /// Requests deletion, optionally conditioned on an intent revision.
    /// # Errors
    /// Returns storage, absence, or generation errors.
    pub async fn delete(
        &self,
        id: &ApplicationId,
        expected: Option<u64>,
    ) -> Result<Operation, ApplicationError> {
        let _guard = self.mutations.lock().await;
        // Repeated deletion remains inspectable after the application disappears.
        if let Some(operation) = self.store.latest_operation_for_application(id).await?
            && operation.kind == piqueld_core::OperationKind::Delete
        {
            SqliteStore::check_generation(expected, operation.generation)?;
            return self.reuse(operation).await;
        }
        let operation = self.store.request_delete(id, expected).await?;
        self.runtime.trigger_reconciliation();
        Ok(operation)
    }

    /// Repairs latest intent, retrying preparation only if it never completed.
    /// # Errors
    /// Returns storage, absence, or generation errors.
    pub async fn reconcile(
        &self,
        id: &ApplicationId,
        expected: Option<u64>,
    ) -> Result<Operation, ApplicationError> {
        let _guard = self.mutations.lock().await;
        let app = self.store.get(id).await?;
        SqliteStore::check_generation(expected, app.generation)?;
        let operation = self
            .store
            .latest_operation_for_application(id)
            .await?
            .ok_or(StoreError::NotFound)?;
        let operation = if operation.state.terminal() || operation.error_code.is_some() {
            self.store.retry_operation(&operation).await?
        } else {
            operation
        };
        self.runtime.trigger_reconciliation();
        Ok(operation)
    }

    /// Explicitly refreshes images without changing manifest generation.
    /// # Errors
    /// Returns storage, deletion-intent, or generation errors.
    pub async fn refresh(
        &self,
        id: &ApplicationId,
        expected: Option<u64>,
    ) -> Result<Operation, ApplicationError> {
        let _guard = self.mutations.lock().await;
        let app = self.store.get(id).await?;
        SqliteStore::check_generation(expected, app.generation)?;
        if app.delete_intent {
            return Err(StoreError::IllegalTransition.into());
        }
        if let Some(operation) = self.store.latest_operation_for_application(id).await?
            && operation.kind == piqueld_core::OperationKind::Refresh
            && operation.state != OperationState::Succeeded
        {
            return self.reuse(operation).await;
        }
        let operation = self.store.request_refresh(id, expected).await?;
        self.runtime.trigger_reconciliation();
        Ok(operation)
    }

    fn new_id() -> ApplicationId {
        ApplicationId::parse(format!("app-{}", uuid::Uuid::now_v7().simple()))
            .expect("UUID application ID is valid")
    }

    async fn reuse(&self, operation: Operation) -> Result<Operation, ApplicationError> {
        if operation.state == OperationState::Failed {
            let operation = self.store.retry_operation(&operation).await?;
            self.runtime.trigger_reconciliation();
            Ok(operation)
        } else {
            Ok(operation)
        }
    }
}
