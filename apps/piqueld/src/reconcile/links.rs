//! Keeps link networks to what the links grant now, whatever state the
//! environments they link are in.

use super::{Controller, DockerApi, DockerError, OperationError, StoreError};
use piqueld_core::{EnvironmentId, ObservedApplication, resource::APPLICATION_LABEL};

impl<D: DockerApi> Controller<D> {
    /// Detaches `id`'s services from the link networks its links no longer
    /// grant them, then removes the unused link networks of importers since
    /// deleted, as `observed` shows them. Neither waits for a rollout: an
    /// unrelated failing service or a running deployment never keeps a
    /// revoked link open, nor does an exporter hold up an importer's deletion.
    /// Runs on full scans and as an environment's deletion ends. Each change
    /// is journaled under `operation_id`.
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
        // A deployment's job prerequisites start before it is promoted, so
        // its prepared target's links stay open while it may still roll out.
        let mut granted = Vec::new();
        if !environment.delete_intent() {
            let prepared = if self
                .may_roll_out(&self.store.operation(operation_id).await?)
                .await?
            {
                self.store.prepared_target(operation_id).await?
            } else {
                None
            };
            for target in environment.resolved.into_iter().chain(prepared) {
                granted.push(self.store.with_links(target, observed).await?);
            }
        }
        let instance = self.store.instance_id();
        let ownership = self.ownership_labels(id);
        for (service, network) in observed.revoked_links(instance, id, &granted) {
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
                .filter(|importer| network.is_owned_by(instance, importer))
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

    /// Removes environment `id`'s telemetry socket directories no service
    /// mounts but its receiving service while granted: a revoked or deleted
    /// environment's, once the next repair unmounts them, and a renamed
    /// receiver's, once its old service is removed. Export to them stopped
    /// as soon as they stopped being the receiver. Holds the mutation lock
    /// and observes afresh, so no mount lands between the observation and
    /// the removal. Runs on full scans and as an environment's deletion ends.
    pub(super) async fn enforce_telemetry(&self, id: &EnvironmentId) -> Result<(), OperationError> {
        let Some(sockets) = self.sockets.as_ref().filter(|sockets| sockets.exists(id)) else {
            return Ok(());
        };
        let receiver = self.store.telemetry_receiver(id).await?;
        if !sockets.stale(id, receiver.as_ref()) {
            return Ok(());
        }
        let _mutations = self.mutations.lock().await;
        let observed = self.docker.observe(id).await?;
        let mounts: Vec<_> = observed
            .services
            .into_iter()
            .filter_map(|service| service.telemetry)
            .collect();
        sockets.remove_unmounted(id, &mounts);
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
        self.journaled(operation_id, action, Some(resource), async {
            Ok(change().await?)
        })
        .await
    }
}
