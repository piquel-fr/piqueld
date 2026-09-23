//! Controller-test commands that inspect identity before submitting intent.

use piqueld::api::{ApplicationError, ApplicationService, Mutation, MutationResponse};
use piqueld::application::RuntimeBoundary;
use piqueld::store::{Store, StoreError};
use piqueld_core::{ApplicationId, Operation, ValidatedApplication};
use std::sync::Arc;

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
        manifest: ValidatedApplication,
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
        manifest: ValidatedApplication,
        expected_generation: Option<u64>,
    ) -> Result<piqueld_core::api::SavedApplication, ApplicationError> {
        self.save_configuration(manifest, expected_generation, false)
            .await
    }

    async fn save_configuration(
        &self,
        manifest: ValidatedApplication,
        expected_generation: Option<u64>,
        deploy: bool,
    ) -> Result<piqueld_core::api::SavedApplication, ApplicationError> {
        let identity = self
            .store
            .find_by_name(manifest.name().as_str())
            .await?
            .map(|stored| stored.application.id().to_string());
        let MutationResponse::Saved(saved) = self
            .service
            .accept(
                Mutation::save(manifest, identity, deploy),
                expected_generation,
                expected_generation.is_none(),
                None,
            )
            .await?
        else {
            return Err(StoreError::Corrupt.into());
        };
        Ok(saved)
    }

    pub async fn deploy(
        &self,
        id: &ApplicationId,
        expected_generation: Option<u64>,
    ) -> Result<Operation, ApplicationError> {
        self.submit(Mutation::Deploy { id: id.clone() }, expected_generation)
            .await
    }

    pub async fn delete(
        &self,
        id: &ApplicationId,
        expected_generation: Option<u64>,
    ) -> Result<Operation, ApplicationError> {
        self.submit(Mutation::Delete { id: id.clone() }, expected_generation)
            .await
    }

    pub async fn reconcile(
        &self,
        id: &ApplicationId,
        expected_generation: Option<u64>,
    ) -> Result<Operation, ApplicationError> {
        self.submit(Mutation::Reconcile { id: id.clone() }, expected_generation)
            .await
    }

    // Controller scenarios that omit a revision deliberately replace current intent.
    async fn submit(
        &self,
        mutation: Mutation,
        expected_generation: Option<u64>,
    ) -> Result<Operation, ApplicationError> {
        let MutationResponse::Operation(accepted) = self
            .service
            .accept(
                mutation,
                expected_generation,
                expected_generation.is_none(),
                None,
            )
            .await?
        else {
            return Err(StoreError::Corrupt.into());
        };
        self.service.operation(&accepted.operation_id).await
    }
}
