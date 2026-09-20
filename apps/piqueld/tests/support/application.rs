//! Controller-test commands that inspect identity before submitting intent.

use piqueld::application::{
    ApplicationError, ApplicationService, Mutation, MutationResponse, RuntimeBoundary,
};
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

    pub async fn apply(
        &self,
        manifest: ValidatedApplication,
        expected_generation: Option<u64>,
    ) -> Result<Operation, ApplicationError> {
        let identity = self
            .store
            .find_by_name(manifest.name().as_str())
            .await?
            .map(|stored| stored.application.id().to_string());
        self.submit(Mutation::apply(manifest, identity), expected_generation)
            .await
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
