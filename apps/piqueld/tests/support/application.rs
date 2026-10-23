//! Controller-test commands that inspect identity before submitting intent.

use piqueld::api::{ApplicationError, ApplicationService, Mutation, MutationResponse};
use piqueld::application::RuntimeBoundary;
use piqueld::store::{Store, StoreError};
use piqueld_core::api::SavedApplication;
use piqueld_core::manifest::ValidatedTemplate;
use piqueld_core::{EnvironmentId, Operation};
use std::sync::Arc;

/// An [`ApplicationService`] with shorthands for the daemon's own mutations.
pub struct TestApplications {
    service: ApplicationService,
    store: Arc<Store>,
}

impl std::ops::Deref for TestApplications {
    type Target = ApplicationService;

    fn deref(&self) -> &Self::Target {
        &self.service
    }
}

impl TestApplications {
    pub fn new(store: Arc<Store>, runtime: Arc<dyn RuntimeBoundary>) -> Self {
        Self {
            service: ApplicationService::new(Arc::clone(&store), runtime),
            store,
        }
    }

    // Controller scenarios explicitly save and deploy in one acceptance.
    pub async fn apply(
        &self,
        manifest: ValidatedTemplate,
        expected_generation: Option<u64>,
    ) -> Result<Operation, ApplicationError> {
        let saved = self
            .save_configuration(manifest, expected_generation, true)
            .await?;
        let operation_id = saved.operation_id.ok_or(StoreError::Corrupt)?;
        self.service.operation(&operation_id).await
    }

    pub async fn save(
        &self,
        manifest: ValidatedTemplate,
        expected_generation: Option<u64>,
    ) -> Result<SavedApplication, ApplicationError> {
        self.save_configuration(manifest, expected_generation, false)
            .await
    }

    /// Saves `manifest` over the application of the same name, if any.
    async fn save_configuration(
        &self,
        manifest: ValidatedTemplate,
        expected_generation: Option<u64>,
        deploy: bool,
    ) -> Result<SavedApplication, ApplicationError> {
        let identity = self
            .store
            .find_by_name(manifest.name().as_str())
            .await?
            .map(|stored| stored.application.id().to_string());
        let mutation = Mutation::save(manifest, identity, deploy);
        let MutationResponse::Saved(saved) = self.accept(mutation, expected_generation).await?
        else {
            return Err(StoreError::Corrupt.into());
        };
        Ok(saved)
    }

    pub async fn deploy(
        &self,
        id: &EnvironmentId,
        expected_generation: Option<u64>,
    ) -> Result<Operation, ApplicationError> {
        self.submit(Mutation::deploy(id.clone()), expected_generation)
            .await
    }

    /// Deletes the application of environment `id`, its only environment.
    pub async fn delete(
        &self,
        id: &EnvironmentId,
        expected_generation: Option<u64>,
    ) -> Result<Operation, ApplicationError> {
        let application = self.store.get(id).await?.environment.application_id;
        let mutation = Mutation::DeleteApplication {
            id: application,
            environments: Vec::new(),
        };
        let MutationResponse::Deleted(deleted) = self.accept(mutation, expected_generation).await?
        else {
            return Err(StoreError::Corrupt.into());
        };
        let operation = deleted.operations.first().ok_or(StoreError::Corrupt)?;
        self.service.operation(&operation.operation_id).await
    }

    pub async fn reconcile(
        &self,
        id: &EnvironmentId,
        expected_generation: Option<u64>,
    ) -> Result<Operation, ApplicationError> {
        self.submit(Mutation::Reconcile { id: id.clone() }, expected_generation)
            .await
    }

    /// Stops environment `id`, returning its latest operation.
    pub async fn stop(&self, id: &EnvironmentId) -> Result<Operation, ApplicationError> {
        self.submit(Mutation::Stop { id: id.clone() }, None).await
    }

    /// Accepts `mutation` and returns the operation it requested.
    async fn submit(
        &self,
        mutation: Mutation,
        expected_generation: Option<u64>,
    ) -> Result<Operation, ApplicationError> {
        let MutationResponse::Operation(accepted) =
            self.accept(mutation, expected_generation).await?
        else {
            return Err(StoreError::Corrupt.into());
        };
        self.service.operation(&accepted.operation_id).await
    }

    /// Accepts `mutation` as the daemon. Controller scenarios that omit a
    /// revision deliberately replace current intent.
    async fn accept(
        &self,
        mutation: Mutation,
        expected_generation: Option<u64>,
    ) -> Result<MutationResponse, ApplicationError> {
        self.service
            .accept(
                piqueld::api::Actor::Daemon,
                mutation,
                expected_generation,
                expected_generation.is_none(),
                None,
            )
            .await
    }
}
