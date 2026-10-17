//! Keeps link networks to what the links grant now, whatever state the
//! environments they link are in.

use super::{Controller, DockerApi, DockerError, OperationError, StoreError};
use piqueld_core::{EnvironmentId, InstanceId, ObservedApplication, resource::APPLICATION_LABEL};

impl<D: DockerApi> Controller<D> {
    /// Detaches `id`'s services from the link networks its links no longer
    /// grant them, then removes the unused link networks of importers since
    /// deleted, as `observed` shows them. Neither waits for a rollout: an
    /// unrelated failing service or a running deployment never keeps a
    /// revoked link open, nor does an exporter hold up an importer's deletion.
    /// Each change is journaled under `operation_id`.
    pub(super) async fn enforce_links(
        &self,
        id: &EnvironmentId,
        operation_id: &str,
        observed: &ObservedApplication,
    ) -> Result<(), OperationError> {
        let environment = match self.store.get(id).await {
            Err(StoreError::NotFound) => return Ok(()),
            environment => environment?,
        };
        let granted = match environment.resolved {
            Some(target) if !environment.delete_intent() => {
                Some(self.store.with_links(target, observed).await?)
            }
            _ => None,
        };
        let instance = InstanceId::parse(self.store.instance_id()).expect("valid store identity");
        let ownership = self.ownership_labels(id);
        for (service, network) in observed.revoked_links(&instance, id, granted.as_ref()) {
            self.journaled_link_change(operation_id, "detach_network", service, || {
                self.docker.detach_network(service, network, &ownership)
            })
            .await?;
        }
        for network in observed
            .linked_networks
            .iter()
            .filter(|network| !network.in_use)
        {
            let Some(importer) = network
                .labels
                .get(APPLICATION_LABEL)
                .and_then(|importer| EnvironmentId::parse(importer.clone()).ok())
            else {
                continue;
            };
            if !matches!(self.store.get(&importer).await, Err(StoreError::NotFound)) {
                continue;
            }
            let ownership = self.ownership_labels(&importer);
            self.journaled_link_change(operation_id, "remove_network", &network.name, || {
                self.docker.remove_network(&network.name, &ownership)
            })
            .await?;
        }
        Ok(())
    }

    /// Runs `change` under the mutation lock as a journaled `action` on
    /// `resource`.
    async fn journaled_link_change<F: Future<Output = Result<(), DockerError>>>(
        &self,
        operation_id: &str,
        action: &str,
        resource: &str,
        change: impl FnOnce() -> F,
    ) -> Result<(), OperationError> {
        let _guard = self.mutations.lock().await;
        let journal = self
            .store
            .begin_action(Some(operation_id), action, Some(resource))
            .await?;
        let result = match self.store.action_request(&journal, 1).await {
            Ok(()) => change().await.map_err(OperationError::from),
            Err(error) => Err(error.into()),
        };
        self.store
            .finish_action(
                &journal,
                result.as_ref().err().map(OperationError::diagnostic),
            )
            .await?;
        result
    }
}
