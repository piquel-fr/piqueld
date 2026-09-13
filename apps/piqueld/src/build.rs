//! Durable attempt lifecycle and output sink, independent of the executor.
use crate::store::{Store, StoreError};
use piqueld_core::{ApplicationId, api::BuildState, manifest::Source};
use std::sync::Arc;

/// A bounded persistent output sink usable by any build executor.
#[derive(Clone)]
pub struct BuildLog {
    store: Arc<Store>,
    id: i64,
}
impl BuildLog {
    /// Appends output, retaining the configured prefix and recording truncation.
    /// # Errors
    /// Returns a persistence error rather than silently losing output.
    pub async fn append(&self, bytes: &[u8]) -> Result<(), StoreError> {
        self.store.append_build_log(self.id, bytes).await
    }
    pub(crate) async fn commit(&self, commit: &str) -> Result<(), StoreError> {
        self.store.build_commit(self.id, commit).await
    }
}

pub(crate) struct BuildAttempt {
    pub(crate) log: BuildLog,
    finished: bool,
}
impl BuildAttempt {
    pub(crate) async fn start(
        store: Arc<Store>,
        application: &ApplicationId,
        operation: &str,
        service: &str,
        source: &Source,
    ) -> Result<Self, StoreError> {
        let id = store
            .start_build(application, operation, service, source)
            .await?;
        Ok(Self {
            log: BuildLog { store, id },
            finished: false,
        })
    }
    pub(crate) async fn finish(
        mut self,
        state: BuildState,
        image: Option<&str>,
    ) -> Result<(), StoreError> {
        self.log
            .store
            .finish_build(self.log.id, state, image)
            .await?;
        self.finished = true;
        Ok(())
    }
}
impl Drop for BuildAttempt {
    fn drop(&mut self) {
        if !self.finished {
            let log = self.log.clone();
            tokio::spawn(async move {
                if let Err(error) = log
                    .store
                    .finish_build(log.id, BuildState::Interrupted, None)
                    .await
                {
                    tracing::error!(build_id=log.id,error=?error,"failed to record interrupted build; startup recovery will retry");
                }
            });
        }
    }
}
